#!/usr/bin/env python3
"""Reference beats for scoring the app's beatgrids.

Runs the beat_this neural beat tracker on each audio file listed in LIST (one path per
line) and writes `<hash>.json` files ({"path", "beats", "downbeats"}, in seconds) to
OUT_DIR, for:

    cargo run --release -p dj-loader --example grid_eval -- OUT_DIR

Unlike analyze.py it writes nothing next to the tracks. Files already done are skipped.
Uses the same venv as analyze.py (see README.md).

    tools/analyze/make_refs.py LIST OUT_DIR
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import time
from pathlib import Path

SAMPLE_RATE = 22_050


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("list", type=Path, help="text file with one audio path per line")
    parser.add_argument("out", type=Path, help="directory for the reference files")
    parser.add_argument("--device", default="cpu", help="torch device (default: cpu)")
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)

    import librosa
    from beat_this.inference import Audio2Beats

    model = Audio2Beats(checkpoint_path="final0", device=args.device, dbn=False)
    paths = [line.strip() for line in args.list.read_text().splitlines() if line.strip()]
    for i, path in enumerate(paths, 1):
        out = args.out / (hashlib.sha1(path.encode()).hexdigest()[:16] + ".json")
        if out.exists():
            continue
        started = time.monotonic()
        try:
            signal, _ = librosa.load(path, sr=SAMPLE_RATE, mono=True)
            beats, downbeats = model(signal, SAMPLE_RATE)
            out.write_text(json.dumps({
                "path": path,
                "beats": [float(b) for b in beats],
                "downbeats": [float(b) for b in downbeats],
            }))
            print(f"[{i}/{len(paths)}] {len(beats)} beats, {time.monotonic() - started:.1f} s: {path}", flush=True)
        except Exception as error:  # keep going: one bad file shouldn't stop the run
            print(f"[{i}/{len(paths)}] FAILED {path}: {error}", file=sys.stderr, flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
