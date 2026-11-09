#!/usr/bin/env python3
"""Offline track analyzer for the dj app.

For each audio file, writes a JSON sidecar next to it (`<file>.dj.json`) with:

- beats and downbeats (seconds), from the beat_this neural beat tracker;
- a constant-tempo fit: BPM and the time of the first downbeat;
- the musical key, from a constant-Q chromagram matched against key profiles.

The app imports sidecars when it scans the library and prefers them over its own
quick in-app analysis. Run it on a desktop (it is too heavy for the CM5):

    tools/analyze/analyze.py ~/Music            # everything not analysed yet
    tools/analyze/analyze.py --force track.mp3  # redo one file

Setup (once): see tools/analyze/README.md.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import sys
import time
from pathlib import Path

import numpy as np

FORMAT = "dj-analysis"
# Bump when the output changes meaning, so the app can tell old sidecars apart.
VERSION = 1
SIDECAR_SUFFIX = ".dj.json"
AUDIO_EXTENSIONS = {".mp3", ".flac", ".wav", ".aiff", ".aif", ".m4a", ".mp4", ".ogg", ".opus"}
SAMPLE_RATE = 22_050

# Temperley's (Kostka-Payne corpus) key profiles, tonic first: the same ones the app uses.
MAJOR = np.array([0.748, 0.060, 0.488, 0.082, 0.670, 0.460, 0.096, 0.715, 0.104, 0.366, 0.057, 0.400])
MINOR = np.array([0.712, 0.084, 0.474, 0.618, 0.049, 0.460, 0.105, 0.747, 0.404, 0.067, 0.133, 0.330])
NAMES = ["C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"]


def sidecar_path(audio: Path) -> Path:
    return audio.with_name(audio.name + SIDECAR_SUFFIX)


def up_to_date(audio: Path) -> bool:
    sidecar = sidecar_path(audio)
    if not sidecar.exists() or sidecar.stat().st_mtime < audio.stat().st_mtime:
        return False
    try:
        data = json.loads(sidecar.read_text())
    except (OSError, ValueError):
        return False
    return data.get("format") == FORMAT and data.get("version") == VERSION


def find_audio(paths: list[Path]) -> list[Path]:
    found = []
    for path in paths:
        if path.is_dir():
            for root, _dirs, files in os.walk(path):
                for name in sorted(files):
                    if Path(name).suffix.lower() in AUDIO_EXTENSIONS and not name.startswith("._"):
                        found.append(Path(root) / name)
        elif path.suffix.lower() in AUDIO_EXTENSIONS:
            found.append(path)
    return found


def coherence(times: np.ndarray, period: float) -> tuple[float, float]:
    """How well `times` sit on a grid of `period` (0..1), and the grid's phase in
    seconds: each time is a unit vector at angle 2*pi*t/period; on-grid times point the
    same way."""
    angles = 2 * np.pi * times / period
    x, y = np.cos(angles).sum(), np.sin(angles).sum()
    return math.hypot(x, y) / len(times), (math.atan2(y, x) / (2 * np.pi) * period) % period


def fit_tempo(beats: np.ndarray, downbeats: np.ndarray) -> tuple[float | None, float | None]:
    """BPM and first-downbeat time of the constant-tempo grid the beats fit best.

    Searches tempos near the median beat interval for the one at which the most beats
    line up (robust to breakdowns and dropped beats, unlike fitting beat numbers), and
    uses a whole-number BPM if it fits almost as well, as most dance music has one.
    """
    if len(beats) < 8:
        return None, None
    rough = float(np.median(np.diff(beats)))
    span = beats[-1] - beats[0]
    step = rough * rough / max(span, rough) / 20
    periods = np.arange(rough * 0.97, rough * 1.03, step)
    scores = [coherence(beats, p)[0] for p in periods]
    period = float(periods[int(np.argmax(scores))])
    best = max(scores)
    whole = 60.0 / round(60.0 / period)
    if coherence(beats, whole)[0] >= best * 0.97:
        period = whole
    bpm = 60.0 / period
    if abs(bpm - round(bpm)) < 1e-9:
        bpm = float(round(bpm))
    _, phase = coherence(beats, period)
    # Put the anchor on the first downbeat, so the grid's bars line up with the music.
    first = float(downbeats[0]) if len(downbeats) else float(beats[0])
    anchor = phase + round((first - phase) / period) * period
    return bpm, max(anchor, 0.0)


def detect_key(signal: np.ndarray) -> dict | None:
    import librosa

    # A2 up five octaves (110 Hz - 3.5 kHz): above kick drums, below Nyquist.
    chroma = librosa.feature.chroma_cqt(
        y=signal, sr=SAMPLE_RATE, hop_length=4096, fmin=librosa.note_to_hz("A2"), n_octaves=5
    )
    if not np.any(chroma):
        return None
    profile = chroma.mean(axis=1)
    scores = []
    for tonic in range(12):
        for mode, template in (("major", MAJOR), ("minor", MINOR)):
            rotated = np.roll(template, tonic)
            scores.append((float(np.corrcoef(profile, rotated)[0, 1]), tonic, mode))
    scores.sort(reverse=True)
    best, tonic, mode = scores[0]
    return {"tonic": NAMES[tonic], "pitch_class": tonic, "mode": mode, "strength": round(best, 3),
            "margin": round(best - scores[1][0], 3)}


def analyze(audio: Path, model) -> dict:
    import librosa

    signal, _ = librosa.load(str(audio), sr=SAMPLE_RATE, mono=True)
    beats, downbeats = model(signal, SAMPLE_RATE)
    beats = np.asarray(beats, dtype=float)
    downbeats = np.asarray(downbeats, dtype=float)
    bpm, first_downbeat = fit_tempo(beats, downbeats)
    stat = audio.stat()
    return {
        "format": FORMAT,
        "version": VERSION,
        "analyzer": "beat_this + librosa chroma_cqt",
        "file": {"name": audio.name, "size": stat.st_size, "duration": round(len(signal) / SAMPLE_RATE, 3)},
        "bpm": bpm,
        "first_downbeat": None if first_downbeat is None else round(first_downbeat, 4),
        "beats": [round(float(b), 4) for b in beats],
        "downbeats": [round(float(b), 4) for b in downbeats],
        "key": detect_key(signal),
    }


def write_sidecar(audio: Path, data: dict) -> None:
    sidecar = sidecar_path(audio)
    temporary = sidecar.with_name(sidecar.name + ".tmp")
    temporary.write_text(json.dumps(data, indent=1))
    os.replace(temporary, sidecar)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("paths", nargs="+", type=Path, help="audio files or folders")
    parser.add_argument("--force", action="store_true", help="analyse even if an up-to-date sidecar exists")
    parser.add_argument("--device", default=None, help="torch device (default: cuda if available, else cpu)")
    args = parser.parse_args()

    files = find_audio(args.paths)
    todo = [f for f in files if args.force or not up_to_date(f)]
    print(f"{len(files)} audio files, {len(todo)} to analyse", flush=True)
    if not todo:
        return 0

    import torch
    from beat_this.inference import Audio2Beats

    device = args.device or ("cuda" if torch.cuda.is_available() else "cpu")
    model = Audio2Beats(checkpoint_path="final0", device=device, dbn=False)

    failures = 0
    for i, audio in enumerate(todo, 1):
        started = time.monotonic()
        try:
            data = analyze(audio, model)
            write_sidecar(audio, data)
            key = data["key"]
            key_text = f"{key['tonic']} {key['mode']}" if key else "no key"
            bpm = f"{data['bpm']:.2f} BPM" if data["bpm"] else "no tempo"
            print(f"[{i}/{len(todo)}] {audio.name}: {bpm}, {key_text} ({time.monotonic() - started:.1f} s)", flush=True)
        except Exception as error:  # keep going: one bad file shouldn't stop a library run
            failures += 1
            print(f"[{i}/{len(todo)}] {audio.name}: FAILED: {error}", file=sys.stderr, flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
