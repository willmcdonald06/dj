#!/usr/bin/env bash
# Regenerate the synthetic audio fixtures in test-data/. Needs ffmpeg with libmp3lame.
# Every file is a generated tone (peak 0.5): no recorded or copyrighted audio.
#
#   tone-44k1-stereo.flac  2 s, 44.1 kHz, 16-bit; left 440 Hz, right 1000 Hz
#   tone-48k-mono.mp3      2 s, 48 kHz, mono 440 Hz, 128 kbit/s
#   tone-44k1-stereo.m4a   2 s, 44.1 kHz AAC; left 440 Hz, right 1000 Hz
#   tone-96k-stereo.wav    1 s, 96 kHz, 24-bit; left 440 Hz, right 1000 Hz
set -euo pipefail
cd "$(dirname "$0")/../test-data"

stereo() { # rate seconds
  echo "-f lavfi -i sine=frequency=440:sample_rate=$1:duration=$2 \
        -f lavfi -i sine=frequency=1000:sample_rate=$1:duration=$2 \
        -filter_complex [0:a][1:a]join=inputs=2:channel_layout=stereo,volume=4[a] -map [a]"
}
meta=(-metadata title="Test Tone" -metadata artist="dj test-data")
ff=(ffmpeg -hide_banner -loglevel error -y)

"${ff[@]}" $(stereo 44100 2) -c:a flac -sample_fmt s16 "${meta[@]}" tone-44k1-stereo.flac
"${ff[@]}" -f lavfi -i sine=frequency=440:sample_rate=48000:duration=2 -af volume=4 \
  -c:a libmp3lame -b:a 128k "${meta[@]}" tone-48k-mono.mp3
"${ff[@]}" $(stereo 44100 2) -c:a aac -b:a 192k "${meta[@]}" tone-44k1-stereo.m4a
"${ff[@]}" $(stereo 96000 1) -c:a pcm_s24le tone-96k-stereo.wav
ls -l
