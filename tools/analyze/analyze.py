#!/usr/bin/env python3
"""Offline track analyzer for the dj app.

For each audio file, writes a JSON sidecar next to it (`<file>.dj.json`) with:

- beats and downbeats (seconds), from the beat_this neural beat tracker;
- a constant-tempo fit: BPM and the time of the first downbeat;
- the musical key, from a constant-Q chromagram matched against key profiles.

and a stems file (`<file>.stems.flac`): vocals, melody, bass and drums from the
Demucs separator, as four stereo pairs in one 8-channel 16-bit FLAC.

The app imports sidecars when it scans the library and prefers them over its own
quick in-app analysis, and loads stems with the track. Run it on a desktop (it is too
heavy for the CM5):

    tools/analyze/analyze.py ~/Music            # everything not analysed yet
    tools/analyze/analyze.py --force track.mp3  # redo one file
    tools/analyze/analyze.py --no-stems ~/Music # beats and key only (much faster)

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

# Run under the tool's own virtualenv (tools/analyze/.venv) when it exists, so plain
# `tools/analyze/analyze.py` or `python3 analyze.py` finds beat_this and demucs.
_VENV = Path(__file__).resolve().parent / ".venv"
if (_VENV / "bin" / "python").exists() and Path(sys.prefix).resolve() != _VENV.resolve():
    _python = str(_VENV / "bin" / "python")
    os.execv(_python, [_python, str(Path(__file__).resolve()), *sys.argv[1:]])

import numpy as np  # noqa: E402

FORMAT = "dj-analysis"
# Bump when the output changes meaning, so the app can tell old sidecars apart.
VERSION = 1
SIDECAR_SUFFIX = ".dj.json"
STEMS_SUFFIX = ".stems.flac"
STEMS_FORMAT = "dj-stems"
# Bump when the stems change meaning (order, model), so old files get redone.
STEMS_VERSION = 1
# Order of the stereo pairs in a stems file, and the Demucs source for each.
STEMS = (("vocals", "vocals"), ("melody", "other"), ("bass", "bass"), ("drums", "drums"))
STEMS_MODEL = "htdemucs"
AUDIO_EXTENSIONS = {".mp3", ".flac", ".wav", ".aiff", ".aif", ".m4a", ".mp4", ".ogg", ".opus"}
SAMPLE_RATE = 22_050
# A whole-number BPM is used if it drifts from the fit by at most this (seconds) over
# the track; the same rule as the app.
ROUND_BPM_MAX_DRIFT = 0.008

# Temperley's (Kostka-Payne corpus) key profiles, tonic first: the same ones the app uses.
MAJOR = np.array([0.748, 0.060, 0.488, 0.082, 0.670, 0.460, 0.096, 0.715, 0.104, 0.366, 0.057, 0.400])
MINOR = np.array([0.712, 0.084, 0.474, 0.618, 0.049, 0.460, 0.105, 0.747, 0.404, 0.067, 0.133, 0.330])
NAMES = ["C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"]


def sidecar_path(audio: Path) -> Path:
    return audio.with_name(audio.name + SIDECAR_SUFFIX)


def stems_path(audio: Path) -> Path:
    return audio.with_name(audio.name + STEMS_SUFFIX)


def is_generated(name: str) -> bool:
    return name.endswith(STEMS_SUFFIX) or name.endswith(SIDECAR_SUFFIX)


def stems_up_to_date(audio: Path) -> bool:
    stems = stems_path(audio)
    if not stems.exists() or stems.stat().st_mtime < audio.stat().st_mtime:
        return False
    try:
        import soundfile

        with soundfile.SoundFile(str(stems)) as existing:
            tag = existing.comment or ""
    except Exception:  # unreadable: redo it
        return False
    return tag.startswith(f"{STEMS_FORMAT}/{STEMS_VERSION} ")


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
                    if (Path(name).suffix.lower() in AUDIO_EXTENSIONS and not name.startswith("._")
                            and not is_generated(name)):
                        found.append(Path(root) / name)
        elif path.suffix.lower() in AUDIO_EXTENSIONS and not is_generated(path.name):
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
    uses a whole-number BPM if it stays within a few milliseconds of that fit over the
    whole track, as most dance music has one. (A looser test, "fits almost as well",
    let grids drift by a quarter beat over a long track.)
    """
    if len(beats) < 8:
        return None, None
    rough = float(np.median(np.diff(beats)))
    span = beats[-1] - beats[0]
    step = rough * rough / max(span, rough) / 20
    periods = np.arange(rough * 0.97, rough * 1.03, step)
    scores = [coherence(beats, p)[0] for p in periods]
    period = float(periods[int(np.argmax(scores))])
    whole = 60.0 / round(60.0 / period)
    if abs(whole - period) * span / period <= ROUND_BPM_MAX_DRIFT:
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


def separate(audio: Path, model, device: str) -> str:
    """Write `<audio>.stems.flac`. Returns a short summary for the log.

    The stems are written as 16-bit FLAC. Separated stems can peak above full scale
    (even when the mix doesn't), so if they would clip they are all turned down by the
    same factor, and the comment tag carries the gain that restores them:
    `dj-stems/1 gain=1.2345 stems=vocals,melody,bass,drums model=htdemucs`.
    """
    import librosa
    import soundfile
    import torch
    from demucs.apply import apply_model

    signal, _ = librosa.load(str(audio), sr=model.samplerate, mono=False)
    if signal.ndim == 1:
        signal = np.stack([signal, signal])
    signal = signal[:2]
    with torch.no_grad():
        mix = torch.from_numpy(np.ascontiguousarray(signal, dtype=np.float32))
        sources = apply_model(model, mix[None], device=device, shifts=1, split=True, overlap=0.25,
                              progress=False)[0].cpu().numpy()
    order = [model.sources.index(source) for _, source in STEMS]
    # (frames, 8): vocals L, vocals R, melody L, ...
    stems = np.concatenate([sources[i] for i in order], axis=0).T
    peak = float(np.max(np.abs(stems))) if stems.size else 0.0
    headroom = 0.999
    gain = max(peak / headroom, 1.0)
    stems = stems / gain
    names = ",".join(name for name, _ in STEMS)
    comment = f"{STEMS_FORMAT}/{STEMS_VERSION} gain={gain:.6f} stems={names} model={STEMS_MODEL}"
    target = stems_path(audio)
    temporary = target.with_name(target.name + ".tmp")
    with soundfile.SoundFile(str(temporary), "w", samplerate=model.samplerate, channels=stems.shape[1],
                             subtype="PCM_16", format="FLAC") as out:
        out.comment = comment
        out.write(stems)
    os.replace(temporary, target)
    return f"stems {target.stat().st_size / 1e6:.0f} MB"


def write_sidecar(audio: Path, data: dict) -> None:
    sidecar = sidecar_path(audio)
    temporary = sidecar.with_name(sidecar.name + ".tmp")
    temporary.write_text(json.dumps(data, indent=1))
    os.replace(temporary, sidecar)


def stem_model_cached() -> bool:
    """Whether the Hugging Face cache already holds the Demucs model (its bag yaml and
    weights). Demucs loads it with `hf_hub_download`, which otherwise asks huggingface.co
    for the latest revision on every run even when the files are cached."""
    hub = os.environ.get("HF_HUB_CACHE") or Path(
        os.environ.get("HF_HOME") or Path.home() / ".cache" / "huggingface") / "hub"
    snapshots = Path(hub) / "models--adefossez--HTDemucs" / "snapshots"
    return any((snapshot / f"{STEMS_MODEL}.yaml").exists() and any(snapshot.glob("*.safetensors"))
               for snapshot in snapshots.glob("*"))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("paths", nargs="+", type=Path, help="audio files or folders")
    parser.add_argument("--force", action="store_true", help="analyse even if an up-to-date sidecar exists")
    parser.add_argument("--device", default=None, help="torch device (default: cuda if available, else cpu)")
    parser.add_argument("--no-stems", action="store_true", help="skip stem separation")
    parser.add_argument("--stems-only", action="store_true", help="only separate stems (no beats or key)")
    args = parser.parse_args()

    files = find_audio(args.paths)
    want_beats = not args.stems_only
    want_stems = not args.no_stems
    todo = []
    for f in files:
        beats = want_beats and (args.force or not up_to_date(f))
        stems = want_stems and (args.force or not stems_up_to_date(f))
        if beats or stems:
            todo.append((f, beats, stems))
    print(f"{len(files)} audio files, {len(todo)} to analyse", flush=True)
    if not todo:
        return 0

    # Everything runs locally; the network is only needed to fetch the models once. Set
    # before huggingface_hub is imported, since it reads these at import time.
    os.environ.setdefault("HF_HUB_DISABLE_TELEMETRY", "1")
    if stem_model_cached():
        os.environ.setdefault("HF_HUB_OFFLINE", "1")
    elif any(stems for _, _, stems in todo):
        print(f"downloading the Demucs model ({STEMS_MODEL}, ~80 MB) once", flush=True)

    import torch

    device = args.device or ("cuda" if torch.cuda.is_available() else "cpu")
    beat_model = None
    stem_model = None
    if any(beats for _, beats, _ in todo):
        from beat_this.inference import Audio2Beats

        beat_model = Audio2Beats(checkpoint_path="final0", device=device, dbn=False)
    if any(stems for _, _, stems in todo):
        from demucs.pretrained import get_model

        stem_model = get_model(STEMS_MODEL)
        stem_model.to(device)
        stem_model.eval()

    failures = 0
    for i, (audio, beats, stems) in enumerate(todo, 1):
        started = time.monotonic()
        parts = []
        try:
            if beats:
                data = analyze(audio, beat_model)
                write_sidecar(audio, data)
                key = data["key"]
                parts.append(f"{data['bpm']:.2f} BPM" if data["bpm"] else "no tempo")
                parts.append(f"{key['tonic']} {key['mode']}" if key else "no key")
            if stems:
                parts.append(separate(audio, stem_model, device))
            print(f"[{i}/{len(todo)}] {audio.name}: {', '.join(parts)} ({time.monotonic() - started:.1f} s)",
                  flush=True)
        except Exception as error:  # keep going: one bad file shouldn't stop a library run
            failures += 1
            print(f"[{i}/{len(todo)}] {audio.name}: FAILED: {error}", file=sys.stderr, flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
