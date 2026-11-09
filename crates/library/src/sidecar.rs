//! Analysis sidecars written by the offline analyzer (`tools/analyze`): a JSON file
//! next to each track, named `<track file name>.dj.json`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const SIDECAR_SUFFIX: &str = ".dj.json";
/// Sidecar format versions this app understands.
pub const SIDECAR_VERSIONS: std::ops::RangeInclusive<u32> = 1..=1;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SidecarKey {
    pub pitch_class: u8,
    /// "major" or "minor".
    pub mode: String,
    #[serde(default)]
    pub strength: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Sidecar {
    pub format: String,
    pub version: u32,
    #[serde(default)]
    pub bpm: Option<f64>,
    #[serde(default)]
    pub first_downbeat: Option<f64>,
    #[serde(default)]
    pub beats: Vec<f64>,
    #[serde(default)]
    pub downbeats: Vec<f64>,
    #[serde(default)]
    pub key: Option<SidecarKey>,
}

#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{path} is not a dj-analysis sidecar version this app reads")]
    Unsupported { path: PathBuf },
}

/// Where the sidecar for `track` lives.
pub fn sidecar_path(track: &Path) -> PathBuf {
    let mut name = track.file_name().unwrap_or_default().to_os_string();
    name.push(SIDECAR_SUFFIX);
    track.with_file_name(name)
}

impl Sidecar {
    pub fn read(path: &Path) -> Result<Self, SidecarError> {
        let text = std::fs::read_to_string(path).map_err(|source| SidecarError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, path)
    }

    pub fn parse(text: &str, path: &Path) -> Result<Self, SidecarError> {
        let sidecar: Sidecar =
            serde_json::from_str(text).map_err(|source| SidecarError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        if sidecar.format != "dj-analysis" || !SIDECAR_VERSIONS.contains(&sidecar.version) {
            return Err(SidecarError::Unsupported {
                path: path.to_path_buf(),
            });
        }
        Ok(sidecar)
    }

    /// Key as (pitch class, minor?).
    pub fn key(&self) -> Option<(u8, bool)> {
        self.key
            .as_ref()
            .map(|k| (k.pitch_class % 12, k.mode.eq_ignore_ascii_case("minor")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_analyzer_output() {
        let text = r#"{"format": "dj-analysis", "version": 1, "analyzer": "x",
            "file": {"name": "a.mp3", "size": 1, "duration": 2.0},
            "bpm": 128.0, "first_downbeat": 0.25, "beats": [0.25, 0.71], "downbeats": [0.25],
            "key": {"tonic": "A", "pitch_class": 9, "mode": "minor", "strength": 0.4, "margin": 0.1}}"#;
        let sidecar = Sidecar::parse(text, Path::new("a.mp3.dj.json")).unwrap();
        assert_eq!(sidecar.bpm, Some(128.0));
        assert_eq!(sidecar.first_downbeat, Some(0.25));
        assert_eq!(sidecar.key(), Some((9, true)));
    }

    #[test]
    fn rejects_other_formats_and_versions() {
        let other = r#"{"format": "other", "version": 1}"#;
        assert!(Sidecar::parse(other, Path::new("x")).is_err());
        let newer = r#"{"format": "dj-analysis", "version": 99}"#;
        assert!(Sidecar::parse(newer, Path::new("x")).is_err());
        let no_tempo = r#"{"format": "dj-analysis", "version": 1, "bpm": null, "key": null}"#;
        let sidecar = Sidecar::parse(no_tempo, Path::new("x")).unwrap();
        assert_eq!((sidecar.bpm, sidecar.key()), (None, None));
    }

    #[test]
    fn sidecar_sits_next_to_the_track() {
        assert_eq!(
            sidecar_path(Path::new("/m/a b.mp3")),
            PathBuf::from("/m/a b.mp3.dj.json")
        );
    }
}
