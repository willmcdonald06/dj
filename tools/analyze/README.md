# Offline analyzer

Analyses tracks on a desktop and writes a JSON sidecar next to each one
(`<track>.dj.json`): beats and downbeats from the [beat_this] neural beat tracker, a
constant-tempo fit (BPM and first downbeat), and the musical key. The app imports
sidecars when it scans its library: their key replaces its own, and their grid is used
when the app isn't sure of its own grid.

It also separates each track into stems with [Demucs] (`htdemucs`): vocals, melody
(Demucs's "other"), bass and drums, written as `<track>.stems.flac` (see below). The
app loads them with the track.

## Setup (once)

```sh
cd tools/analyze
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
```

Everything runs on your machine. The network is used only to download the two models
the first time they are needed: the beat model (~80 MB) into
`~/.cache/torch/hub/checkpoints/` and the Demucs model (~80 MB) into
`~/.cache/huggingface/hub/`. After that the script runs offline: it sets
`HF_HUB_OFFLINE` once the Demucs model is cached, because otherwise the Hugging Face
library asks huggingface.co for updates on every run.

## Use

The script re-runs itself under `tools/analyze/.venv` when that exists, so plain
`tools/analyze/analyze.py` or `python3 tools/analyze/analyze.py` works.

```sh
tools/analyze/analyze.py ~/Music       # new or changed files only
tools/analyze/analyze.py --force a.mp3 # redo one
```

Beats and key take roughly 4–15 s per track on a laptop CPU; stems about 2 minutes
(much less with a GPU, `--device cuda`). Up-to-date sidecars and stems are skipped
independently, so it can be rerun on the whole library after adding music.
`--no-stems` skips separation; `--stems-only` does only separation.

## Stems file (version 1)

One 8-channel, 16-bit FLAC at 44.1 kHz: vocals L/R, melody L/R, bass L/R, drums L/R.
Separated stems can peak above full scale, so if they would clip they are all turned
down by the same factor, and the FLAC comment tag gives the gain that undoes it:

```
dj-stems/1 gain=1.000000 stems=vocals,melody,bass,drums model=htdemucs
```

The app rejects files with another version and stems that don't add up to the track.

## Sidecar format (version 1)

```json
{
 "format": "dj-analysis", "version": 1, "analyzer": "...",
 "file": {"name": "a.mp3", "size": 1234, "duration": 312.5},
 "bpm": 128.0, "first_downbeat": 0.123,
 "beats": [0.123, 0.592, ...], "downbeats": [0.123, 1.998, ...],
 "key": {"tonic": "A", "pitch_class": 9, "mode": "minor", "strength": 0.4, "margin": 0.1}
}
```

Times are in seconds from the start of the decoded audio. `bpm` and `first_downbeat`
are `null` when the track has no steady beat; `key` is `null` for silence.

[beat_this]: https://github.com/CPJKU/beat_this
[Demucs]: https://github.com/adefossez/demucs

## Scoring the app's beatgrids

`make_refs.py` writes reference beats for a list of tracks (into a folder of its own,
not next to the tracks), and the `grid_eval` example scores the app's grids against
them:

```sh
find ~/Music -name '*.mp3' | shuf -n 100 > sample.txt
tools/analyze/.venv/bin/python tools/analyze/make_refs.py sample.txt refs/
cargo run --release -p dj-loader --example grid_eval -- refs/
```
