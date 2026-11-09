# Third-party code

Vendored C++ libraries, each with its own licence file. Compiled by `crates/stretch`.

| Directory | Library | Licence | Source |
| --- | --- | --- | --- |
| `signalsmith-stretch` | Signalsmith Stretch (time-stretch / pitch-shift) | MIT | https://github.com/Signalsmith-Audio/signalsmith-stretch |
| `signalsmith-linear` | Signalsmith Linear (FFT, STFT) | MIT | https://github.com/Signalsmith-Audio/linear |

Copied unmodified from the sources bundled in the `signalsmith-stretch` 0.1.3 crate. The
C wrapper in `crates/stretch/src/wrapper.cpp` is our own.
