//! Decode a file into a [`Track`] at the engine sample rate.
//!
//! Not RT-safe; runs on loader threads.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dj_engine::{TRACK_CHANNELS, Track, TrackId, TrackWriter};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, MetadataRevision, StandardTag};

use crate::resample::{ResampleError, StereoResampler};

/// Extra room allocated beyond the frame count a file declares, in case the declared
/// count is a little short.
const CAPACITY_SLACK_SECONDS: usize = 2;

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("cannot open file: {0}")]
    Open(#[from] std::io::Error),
    #[error("unsupported or unrecognised format: {0}")]
    Format(String),
    #[error("file has no audio track")]
    NoAudioTrack,
    #[error("no decoder for this codec: {0}")]
    Codec(String),
    #[error("decoding failed: {0}")]
    Decode(String),
    #[error(transparent)]
    Resample(#[from] ResampleError),
    #[error("file contains no audio")]
    Empty,
    #[error("load cancelled")]
    Cancelled,
}

/// What we know about a track once decoding has started.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub id: TrackId,
    pub title: String,
    pub artist: Option<String>,
    /// Sample rate of the file (the track itself is at the engine rate).
    pub source_sample_rate: u32,
    pub source_channels: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecodeStats {
    pub frames: u64,
    /// Time from opening the file until the first audio was playable.
    pub time_to_playable: Duration,
    pub total_time: Duration,
    /// Packets that failed to decode and were skipped.
    pub skipped_packets: u64,
}

/// Decode `path` into a new track at `engine_rate`.
///
/// `on_playable` is called once, as soon as the first audio is in the track, with the
/// track and its info; the caller hands the track to the engine from there while
/// decoding continues. If the file doesn't declare its length, the whole file is decoded
/// first and `on_playable` is called at the end. Decoding stops early when `cancel` is
/// set.
pub fn decode_file(
    path: &Path,
    id: TrackId,
    engine_rate: u32,
    cancel: &AtomicBool,
    on_playable: &mut dyn FnMut(Arc<Track>, TrackInfo),
) -> Result<DecodeStats, LoadError> {
    let started = Instant::now();
    let mut source = Source::open(path)?;
    let source_rate = source.sample_rate;

    let info = TrackInfo {
        id,
        title: source.title.take().unwrap_or_else(|| file_stem(path)),
        artist: source.artist.take(),
        source_sample_rate: source_rate,
        source_channels: source.channels,
    };

    let expected = source
        .frames
        .map(|frames| (frames * engine_rate as u64).div_ceil(source_rate as u64) as usize);
    let mut sink = match expected {
        Some(expected) => {
            let capacity = expected + CAPACITY_SLACK_SECONDS * engine_rate as usize;
            let (_track, mut writer) = Track::new(id, engine_rate, capacity);
            writer.set_expected_len(expected);
            Sink::Track {
                writer,
                info: Some(info),
                playable_at: None,
            }
        }
        None => Sink::Staging {
            samples: Vec::new(),
            info,
        },
    };

    let mut resampler = match source_rate == engine_rate {
        true => None,
        false => Some(StereoResampler::new(source_rate, engine_rate)?),
    };

    let mut interleaved = Vec::new();
    let mut emit = |left: &[f32], right: &[f32]| {
        interleaved.clear();
        interleaved.extend(left.iter().zip(right).flat_map(|(&l, &r)| [l, r]));
        sink.push(&interleaved, on_playable);
    };

    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut cancelled = false;
    while source.next_stereo(&mut left, &mut right)? {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        match resampler.as_mut() {
            Some(resampler) => resampler.push(&left, &right, &mut emit)?,
            None => emit(&left, &right),
        }
    }
    if cancelled {
        sink.abandon();
        return Err(LoadError::Cancelled);
    }
    if let Some(resampler) = resampler.as_mut() {
        resampler.finish(&mut emit)?;
    }

    let skipped_packets = source.skipped_packets;
    let (frames, playable_at) = sink.finish(id, engine_rate, started, on_playable)?;
    Ok(DecodeStats {
        frames,
        time_to_playable: playable_at.duration_since(started),
        total_time: started.elapsed(),
        skipped_packets,
    })
}

/// Where decoded frames go: straight into the track when its length is known up front,
/// or into a staging buffer when it isn't.
enum Sink {
    Track {
        writer: TrackWriter,
        /// Taken when the track is handed out.
        info: Option<TrackInfo>,
        playable_at: Option<Instant>,
    },
    Staging {
        samples: Vec<f32>,
        info: TrackInfo,
    },
}

impl Sink {
    fn push(&mut self, interleaved: &[f32], on_playable: &mut dyn FnMut(Arc<Track>, TrackInfo)) {
        match self {
            Sink::Track {
                writer,
                info,
                playable_at,
            } => {
                let written = writer.push_interleaved(interleaved);
                if written * TRACK_CHANNELS < interleaved.len() {
                    tracing::warn!("track longer than declared; dropping the overflow");
                }
                if let Some(info) = info.take() {
                    *playable_at = Some(Instant::now());
                    on_playable(Arc::clone(writer.track()), info);
                }
            }
            Sink::Staging { samples, .. } => samples.extend_from_slice(interleaved),
        }
    }

    /// Stop early: whatever was decoded stays playable, and the track stops reporting
    /// that it is still loading.
    fn abandon(self) {
        if let Sink::Track { writer, .. } = self {
            writer.finish();
        }
    }

    /// Complete the track. Returns its length in frames and when it became playable.
    fn finish(
        self,
        id: TrackId,
        engine_rate: u32,
        started: Instant,
        on_playable: &mut dyn FnMut(Arc<Track>, TrackInfo),
    ) -> Result<(u64, Instant), LoadError> {
        match self {
            Sink::Track {
                writer,
                info,
                playable_at,
            } => {
                if info.is_some() {
                    return Err(LoadError::Empty);
                }
                let frames = writer.written_frames() as u64;
                writer.finish();
                Ok((frames, playable_at.unwrap_or(started)))
            }
            Sink::Staging { samples, info } => {
                let frames = samples.len() / TRACK_CHANNELS;
                if frames == 0 {
                    return Err(LoadError::Empty);
                }
                let (track, mut writer) = Track::new(id, engine_rate, frames);
                writer.push_interleaved(&samples);
                writer.finish();
                let playable_at = Instant::now();
                on_playable(track, info);
                Ok((frames as u64, playable_at))
            }
        }
    }
}

/// An open file: demuxer plus decoder for its default audio track.
struct Source {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    sample_rate: u32,
    channels: usize,
    /// Playable frames, if the file declares them.
    frames: Option<u64>,
    title: Option<String>,
    artist: Option<String>,
    planar: Vec<Vec<f32>>,
    skipped_packets: u64,
}

impl Source {
    fn open(path: &Path) -> Result<Self, LoadError> {
        let file = File::open(path)?;
        let stream = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(extension);
        }
        let mut format = symphonia::default::get_probe()
            .probe(
                &hint,
                stream,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|err| LoadError::Format(err.to_string()))?;

        let track = format
            .default_track(TrackType::Audio)
            .ok_or(LoadError::NoAudioTrack)?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .ok_or(LoadError::NoAudioTrack)?;
        let sample_rate = params
            .sample_rate
            .ok_or_else(|| LoadError::Format("sample rate unknown".into()))?;
        let channels = params.channels.as_ref().map_or(0, |c| c.count());
        let track_id = track.id;
        let frames = track.num_frames.filter(|&n| n > 0);
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|err| LoadError::Codec(err.to_string()))?;

        let (title, artist) = match format.metadata().skip_to_latest() {
            Some(revision) => title_and_artist(revision),
            None => (None, None),
        };

        Ok(Self {
            format,
            decoder,
            track_id,
            sample_rate,
            channels,
            frames,
            title,
            artist,
            planar: Vec::new(),
            skipped_packets: 0,
        })
    }

    /// Decode the next packet into planar stereo (mono is duplicated, channels beyond
    /// two are dropped). Returns `false` at the end of the file.
    fn next_stereo(
        &mut self,
        left: &mut Vec<f32>,
        right: &mut Vec<f32>,
    ) -> Result<bool, LoadError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => return Ok(false),
                // A new chained stream (e.g. in Ogg) starts; treat it as the end.
                Err(SymphoniaError::ResetRequired) => return Ok(false),
                Err(SymphoniaError::IoError(err))
                    if err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(false);
                }
                Err(err) => return Err(LoadError::Decode(err.to_string())),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::IoError(_)) => {
                    // A damaged packet: skip it, as players do.
                    self.skipped_packets += 1;
                    continue;
                }
                Err(err) => return Err(LoadError::Decode(err.to_string())),
            };
            if decoded.frames() == 0 {
                continue;
            }
            decoded.copy_to_vecs_planar::<f32>(&mut self.planar);
            let (l, r) = match self.planar.as_slice() {
                [] => continue,
                [mono] => (mono, mono),
                [l, r, ..] => (l, r),
            };
            left.clear();
            left.extend_from_slice(l);
            right.clear();
            right.extend_from_slice(r);
            return Ok(true);
        }
    }
}

/// A file's tags and length, read without decoding any audio.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Seconds, if the file declares its length.
    pub duration: Option<f64>,
}

/// Read `path`'s tags and length (for library scanning). Fast: no audio is decoded.
pub fn read_tags(path: &Path) -> Result<FileTags, LoadError> {
    let mut source = Source::open(path)?;
    let album = match source.format.metadata().skip_to_latest() {
        Some(revision) => album(revision),
        None => None,
    };
    Ok(FileTags {
        title: source.title.take(),
        artist: source.artist.take(),
        album,
        duration: source
            .frames
            .map(|frames| frames as f64 / source.sample_rate as f64),
    })
}

fn album(revision: &MetadataRevision) -> Option<String> {
    revision
        .media
        .tags
        .iter()
        .chain(
            revision
                .per_track
                .iter()
                .flat_map(|t| t.metadata.tags.iter()),
        )
        .find_map(|tag| match &tag.std {
            Some(StandardTag::Album(value)) => Some(value.trim().to_string()),
            _ => None,
        })
        .filter(|a| !a.is_empty())
}

fn title_and_artist(revision: &MetadataRevision) -> (Option<String>, Option<String>) {
    let tags = revision.media.tags.iter().chain(
        revision
            .per_track
            .iter()
            .flat_map(|t| t.metadata.tags.iter()),
    );
    let mut title = None;
    let mut artist = None;
    for tag in tags {
        match &tag.std {
            Some(StandardTag::TrackTitle(value)) if title.is_none() => {
                title = Some(value.trim().to_string());
            }
            Some(StandardTag::Artist(value)) if artist.is_none() => {
                artist = Some(value.trim().to_string());
            }
            _ => {}
        }
    }
    (
        title.filter(|t| !t.is_empty()),
        artist.filter(|a| !a.is_empty()),
    )
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
