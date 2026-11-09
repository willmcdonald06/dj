# Offline analyzer

Analyses tracks on a desktop and writes a JSON sidecar next to each one
(`<track>.dj.json`): beats and downbeats from the [beat_this] neural beat tracker, a
constant-tempo fit (BPM and first downbeat), and the musical key. The app imports
sidecars when it scans its library and prefers them over its own quick analysis.

## Setup (once)

```sh
cd tools/analyze
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
```

The beat model (~80 MB) downloads on first use into `~/.cache/torch/hub/checkpoints/`.

## Use

```sh
tools/analyze/.venv/bin/python tools/analyze/analyze.py ~/Music       # new or changed files only
tools/analyze/.venv/bin/python tools/analyze/analyze.py --force a.mp3 # redo one
```

Roughly 4–15 s per track on a laptop CPU. Up-to-date sidecars are skipped, so it can
be rerun on the whole library after adding music.

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
