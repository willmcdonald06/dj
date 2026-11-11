# dj

A standalone two-deck DJ application in Rust. It is developed on macOS/Linux with a
Roland DJ-505 and targets a Raspberry Pi CM5 with a 7″ touchscreen.

![Two decks playing: full-width scrolling waveforms with beatgrids and transient markers, deck panels either side of the mixer, and the library browser below](demo.png)

## Status

- **Milestone 0 (skeleton):** done. Workspace, CI, a window, and a sine tone through
  `cpal` with the real-time rules enforced.
- **Milestone 1 (one deck):** done. Load a FLAC/MP3/M4A/WAV/AIFF, play, pause and CDJ-style
  CUE from the keyboard. A loader thread decodes and resamples into a buffer allocated up
  front; the track is playable after the first packet (tens of ms) while decoding continues.
- **Milestone 2 (DJ-505, two decks, mixer):** done, but the DJ-505 mapping has only been
  tested against a simulated controller (see below). Two decks; per channel trim, 3-band
  isolator EQ with kills, one-knob low/high-pass filter, channel fader and headphone cue
  (PFL); main on outputs 1–2, cue on 3–4. Tempo is varispeed (pitch follows tempo) until
  key lock lands in Milestone 4. MIDI monitor, and a control thread that connects to the
  DJ-505 whenever it is plugged in.
- **Milestone 3 (waveforms, transient markers):** done. On-screen text; stacked
  scrolling waveforms (low/mid/high bands) with a shared centre playhead, scaled by
  tempo so matched kicks scroll together; overview strips; kick markers detected from
  the audio; and a live phase offset readout between the decks' kicks. Kick markers
  land within ±0.75 ms on synthetic kicks and roughly ±2 ms on real music.
- **Milestone 4 (key lock, tempo, beatgrid, sync):** done. Key lock (on by default)
  through Signalsmith Stretch behind the `TimeStretcher` trait; a constant-tempo
  beatgrid fitted to the kick markers (whole-number BPMs when they fit); SYNC per deck
  with a master deck, phase kept by a small rate correction rather than jumps;
    quantized cue points; fine keyboard tempo (0.02% per press).
- **Milestone 5 (cues, loops, pads):** done. 8 hot cues per deck (a slot holds a cue
  point or a saved loop), loop in/out, auto loops of a chosen length, halve/double,
  reloop, loop rolls with slip, beat jump; all quantized when QUANTIZE is on, and loops stay seamless with key lock.
  Hot cues and saved loops are stored in the library (SQLite) the moment they change
  and come back when the track is loaded again. DJ-505 pads, pad modes and loop buttons
    are mapped, with pad LEDs in the hot cue colours.
- **Milestone 6 (library, browser, offline analyzer):** done, with crates and
  playlists. The library scans `~/Music` (or `--music DIR`) and USB drives as they
  are plugged in, and analyses every track in the background for BPM and key. The
  browser (always on screen under the decks) has search, sortable columns and sources.
  The offline analyzer in `tools/analyze` writes sidecars with neural beats and
  downbeats, which the app imports (their key is preferred; their grid is a fallback).
- **Milestone 7 (stems):** done. The analyzer separates each track into vocals, melody,
  bass and drums (Demucs) in `<track>.stems.flac`; the app loads them with the track,
  keeps them compressed in memory and mutes them live per deck (STEMS pad mode, the
  stem chips on each deck, and the DJ-505's SAMPLER pad mode). See [Stems](#stems).
- **Milestone 8 (beat FX):** done. One FX unit (echo, reverb, filter sweep, flanger,
  roll) on deck A, deck B or the main mix, post-fader, timed in beats from the deck's
  grid or TAP, with tails. In the top bar; not yet mapped on the DJ-505. See
  [Beat FX](#beat-fx).
- **Milestone 9 (CM5 / Linux port):** waiting for the hardware.
- **Milestone 10 (hardware modes, desktop parts):** recording, the settings screen and
  per-deck output routing done. See [Recording](#recording) and [Settings](#settings).

## Build and run

```sh
# Linux needs ALSA headers: libasound2-dev (Debian) / alsa-lib-devel (Fedora)
cargo run -- track.flac            # window, track on deck A (a second file goes to B)
cargo run -- --list-devices
cargo run -- --device "DJ-505"     # pick an output by name substring
cargo run -- --headless 10 a.mp3   # no window; plays deck A (or the tone) and logs stats
cargo run -- --midi-monitor        # log incoming MIDI and what it maps to
cargo run -- --help
```

### The screen

```text
 top bar: audio health · QUANTIZE · zoom − / + · HELP
 deck A scrolling waveform (full width)
 phase meter
 deck B scrolling waveform (full width)
 deck A panel  │  mixer  │  deck B panel
 browser
```

The two scrolling waveforms always span the window, one above the other, around a
shared centre playhead; each is scaled by its deck's tempo so matched beats line up (a
nudge slides it along rather than stretching it). They are coloured by spectrum (red
bass, green mids, blue highs; `--waveform bands` gives the layered blue/orange/white
style instead) and dimmed where already played. The beatgrid shows as grey ticks on
every beat and a numbered white line on every bar; hot cues are numbered flags, the
loop is green and the cue point is a pair of orange triangles. Between the waveforms,
each deck's transients are spikes standing on a centre line (deck A's pointing up,
deck B's down), so decks in phase have spikes that line up. The phase meter in the top
bar says which deck's kicks land first, and by how many ms, while both decks play.

Each deck panel has the track header (deck letter, title, artist, key in its colour on
the circle of fifths, outlined green when it mixes with the other deck, BPM, tempo and
range, remaining and elapsed time), the overview strip, then CUE (hold), PLAY, SYNC
(MASTER when the other deck follows this one), KEY lock, bend ◂ ▸ and fine tempo − +
(both repeat while held); IN, OUT, the auto-loop length with ◂ ▸ (halves or doubles the
active loop too), LOOP and RELOOP; pad modes and the eight pads; and a vertical tempo
fader on the outer edge (up is slower, as on a CDJ) under its range button. The mixer
has trim, high, mid, low and filter knobs, a fader, a meter and headphone CUE per
channel, and main level, headphone level, cue/main blend and SPLIT in the middle.

Everything works with a mouse or a single-finger touch. Drag knobs up or down; drag
faders; double-click either to reset it. Click an overview strip to move the playhead
there, or a deck letter to select the deck. Drag a scrolling waveform to scratch it, like a
hand on the record: the track moves with the pointer, forwards or backwards, at the
speed you drag, and a playing deck carries on when you let go; the mouse wheel
over the waveforms zooms. Loaded tracks are cued at their first beat, skipping leading
silence (only until you touch the deck). HELP (or **H**) lists every key.

Keys act on the selected deck: **Tab** selects deck A/B, **Space** play/pause, **C**
(hold) CUE, **Left/Right** seek 4 s, **Up/Down** tempo ±0.02% (**PgUp/PgDn** ±1%),
**,/.** nudge, **S** sync on/off, **K** key lock, **Q** quantize, **R**
tempo range, **P** headphone cue, **E** eject, **W** record, **U** settings, **T** test tone, **F11** fullscreen (close the window to quit). Drop a file on the
window to load it onto the selected deck (refused while that deck plays).

CUE works like a CDJ: while playing, CUE returns to the cue point and stops. While
stopped, CUE sets the cue point at the playhead and plays while held; releasing it
returns to the cue point. Pressing Space during that preview keeps playing.

`--play` starts the files given on the command line as soon as they load.

### Roland DJ-505

The app finds the DJ-505 by its MIDI port name and connects whenever it is plugged in
(`--midi NAME` picks another port, `--no-midi` turns this off). It switches the 505 into
Serato mode (the software drives every LED), keeps it there with keep-alives, and reads
the starting position of every fader and knob. Mapped: PLAY, CUE, PFL (the channel CUE
button), pad modes (HOT CUE, ROLL, LOOP as beat jump, SAMPLER as stems), tempo fader, SHIFT+KEY LOCK (tempo range), jog wheels (side: nudge;
top touched: scratch, playing or stopped, like a record under the hand), trim, EQ, filter, channel faders. The 505's own
master, booth, phones and cue/master mix knobs act on its hardware mixer, so the app's
headphone bus carries only the cue signal.

The mapping was built from the 505's documented MIDI messages and tested against a
simulated 505 (`cargo test -p dj-control --test virtual_dj505 -- --ignored`, Linux
only). With the real controller, check with `--midi-monitor` that the tempo fader runs
the right way and that jog nudge and scratch feel right; the constants are
`NUDGE_PER_TICK` and `SCRATCH_SECONDS_PER_TICK` (512 ticks per turn, 45 rpm) in
`crates/control/src/dj505.rs`, and `SCRATCH_SMOOTHING_SECONDS` in the engine's deck. Top right is
the debug overlay, a histogram of callback duration as a share of the period budget; a
red bar under it means at least one xrun.

## Layout

| Crate | Purpose | RT-safe |
| --- | --- | --- |
| `engine` | Audio graph, `process(out, frames)`; no hardware deps | `Engine::process`: yes |
| `audio-io` | `cpal` device selection, stream, callback stats | data callback: yes |
| `stretch` | `TimeStretcher` trait; Signalsmith Stretch (vendored C++, MIT) | `process`: yes |
| `loader` | Decoding (`symphonia`), resampling (`rubato`), stems loading, loader thread | no |
| `control` | MIDI ports, mappings (DJ-505), control thread, MIDI monitor | no |
| `library` | SQLite library: tracks, analysis, hot cues; sidecar import | no |
| `analysis` | Waveform summaries (3-band, multi-resolution), kick markers | no |
| `ui` | `winit` + `wgpu` renderer | no |
| `app` | Binary: wires it all together | no |

## Rules

- **No one-line conditional expressions** (`let x = if a { b } else { c };`). Use a
  multi-line `if/else` or a `match`. `tools/check-style.sh` enforces this, and
  `rustfmt.toml` stops `cargo fmt` from collapsing if/else onto one line.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` must
  pass (CI runs all three plus the style check on Linux x86_64, Linux ARM64 and macOS).
- The audio callback must not allocate, lock, do I/O or log. In debug builds the binary
  installs `assert_no_alloc::AllocDisabler`, and an allocation inside the callback
  **aborts the process**.
- The audio thread never frees memory either. The loader keeps an `Arc` to every track
  it hands to the engine and drops it on its own thread once the engine has let go.

## Key

Each track's musical key is detected during analysis and shown in the deck header,
in Open Key by default (`--key-notation camelot` or `musical` to change; the settings
screen will take this over). It turns green when it mixes with the other deck's key:
the same key, one step round the wheel, or the relative major/minor. With key lock off,
it shows the key the tempo has moved the track to, e.g. `5m (+1)`.

Detection folds each track's spectrum onto the 12 pitch classes and matches it against
Temperley's key profiles. On songs with published keys it gets most right; its usual
slip is reporting the relative major for a minor key (same wheel number, other letter).
The offline analyzer (Milestone 6) will provide better keys.

## Library and browser

The library database (`~/.local/share/dj/library.db`) holds every track found in the
music folders (`~/Music` unless `--music DIR` is given, repeatable) and on USB drives,
which are picked up when mounted. A background thread rescans every two minutes (or on
RESCAN), reads tags from new and changed files, and analyses tracks one at a time, so
BPM and key fill in across the whole library; the first run over a large library keeps
one CPU core busy for a while.

The browser fills the bottom of the screen. Clicking it (or **B**) gives it the arrow
keys, shown by an outline in the selected deck's colour; clicking anywhere else, or
**Esc**, gives them back to the decks.

- Sources on the left (all tracks, each folder, each USB drive).
- Click a column header to sort; again to reverse.
- **/** or click the search box, then type; every word must match the title, artist,
  album or file name. Enter or Esc leaves the search box.
- ↑/↓ and Page Up/Down select, **Enter** loads onto the selected deck (Tab switches),
  LOAD A / LOAD B load onto either, and a double-click loads onto the selected deck.
- Tracks on a deck are marked A/B. Keys are in their wheel colour, filled when they mix
  with what's loaded.
- **Crates** (unordered, sorted like the rest) and **playlists** (your order, shown in
  the # column) are in the sidebar. The + by CRATES or PLAYLISTS makes one and you
  type its name (Enter to keep it). Drag a track onto one to add it. While viewing one:
  RENAME, DELETE (press twice), REMOVE the selected track, and in playlists UP / DOWN
  to reorder. All of it is saved in the library at once.

On the DJ-505, turning the browse knob moves the selection (and gives the browser the
arrow keys), pressing it toggles that, and LOAD loads onto that deck.

### Offline analysis

`tools/analyze` (see its README) runs a neural beat and downbeat tracker on a desktop
and writes `<track>.dj.json` next to each track. The library imports these on its next
scan. Their key replaces the app's own; their beatgrid is used only when the app isn't
sure of its own, since the app's grids sit on the attacks the same way on every track
(the tracker marks the beat as heard, 10–20 ms later), which is what sync needs.

### Beatgrids

The app finds every track's grid itself (`crates/analysis/src/tempo.rs`): the tempo
from the whole track's onsets, the period and phase from folding them, a straight-line
fit to the attacks on the beats, a whole-number BPM when it stays within 8 ms over the
track, the phase set on the kick markers, and the downbeat where the harmony changes
from bar to bar. `cargo run --release -p dj-loader --example grid_eval -- REFS_DIR`
scores it against reference beats from a neural tracker (see that file). On a sample
of 109 tracks from a real library, against beat_this:

- the grid is on the beat for practically every steady dance track;
- for all tracks with a steady tempo, about 90% of beats land within 25 ms;
- bar starts are right about 85% of the time; and
- tracks without a steady tempo (live drummers, tempo changes, acapellas) can't have
  one constant grid, and are mostly flagged as unsure (not used for sync).

When a grid is wrong, the **GRID** pad mode fixes it by hand: **1 HERE** makes the beat
nearest the playhead the first of the bar, **BEAT ›** moves the bar start by a beat,
**½ BEAT** shifts the grid half a beat, **‹ 2 ms / 2 ms ›** nudge it (hold to repeat),
**BPM ×2 / ÷2** double or halve the tempo, and **RESET** goes back to the analysis.
Each change applies at once and is saved in the library, where it beats re-analysis
and sidecars.

## Hot cues, loops and pads

Each deck has eight pads with three modes:

- **HOT CUE:** an empty pad stores the playhead, or the current loop if one is playing.
  A set pad jumps there while playing (keeping the beat with quantize on), or plays from
  it while held on a stopped deck, like CUE. **DELETE** then a pad clears it.
- **ROLL:** hold for a loop roll of 1/16 to 8 beats; the track keeps running underneath
  (shown as a faint playhead) and playback rejoins it when you let go.
- **JUMP:** jump 1, 2, 4 or 8 beats back (top row) or forward (bottom row).
- **GRID:** fix the beatgrid by hand (see Beatgrids).
- **STEMS:** pads 1–4 mute and unmute vocals, melody, bass and drums; pads 5–8 play one
  stem alone (press again for all of them). See [Stems](#stems).

The loop row has IN, OUT, 4 BEAT (EXIT while looping), ½X, 2X and RELOOP. Keys: **1–8**
hot cues (hold), **I/O** loop in/out, **L** 4-beat loop / exit, **[ ]** halve/double,
**M** switches the pads between HOT CUE and STEMS (in STEMS mode **1–8** are the stem
pads).

Hot cues live in the library database, `~/.local/share/dj/library.db` by default
(`--library FILE` to choose another).

## Stems

Run the offline analyzer (`tools/analyze`, see its README) to separate tracks: it
writes `<track>.stems.flac` next to each one, four stereo stems in one 8-channel FLAC
(about 2 minutes per track on a laptop CPU, 50–80 MB per track). The library ignores
these files. When a track with a stems file is loaded, its stems follow about a second
later, and four chips (**V M B D**) appear at the right of the pad tabs: lit while that
stem plays, dark while muted, click to toggle. Mutes pressed before the stems arrive
apply when they do.

How it works (`crates/engine/src/stems.rs`):

- **Subtraction, not summing.** The deck plays the original track and subtracts the
  muted stems from it, so with every stem on you hear the file untouched, only muted
  stems cost any CPU, and a stem fades in or out over 12 ms with no click. Muting all
  four leaves the separator's residual (typically 30 dB down).
- **Before the stretcher.** Stems are taken out of the source frames, so key lock costs
  the same with or without them.
- **Compressed in memory.** Stems are re-encoded on load with a small FLAC-like
  lossless codec (`stem_codec.rs`: fixed predictors, Rice codes, 1024-frame blocks) that
  the audio thread can decode without allocating: about 8 µs per block per muted stem.
  A 3.5-minute track's stems take about 80 MB, roughly half their 16-bit size; two
  decks of long tracks fit the spec's ~300 MB.
- **Lined up with the track.** On load the stems are cross-correlated with the decoded
  track (decoders can disagree about where an MP3 starts) and rejected, with a notice,
  if they don't rebuild it to within 12 dB (a stems file from another version of the
  track, say).

`cargo run --release -p dj-loader --example stems -- TRACK [OUT.wav]` reports a stems
file's alignment, residual, size and load time, and can write 20 s with the vocals
taken out to check by ear.

## Beat FX

One effect unit, in the top bar either side of the phase meter: **FX** (on/off), the
effect (◂ ▸ or click the name), its length in beats (◂ ▸: 1/4, 1/2, 3/4, 1, 2, 4, 8,
16), the target (**A**, **B** or **MAIN**), the LEVEL knob, and **TAP** with the FX
tempo. Keys: **F** on/off, **G** next effect, **Z / X** shorter/longer, **V** tap.

- **ECHO** repeats every length, darker each time; **REVERB** decays over the length.
  Both are sends: turning the FX off stops feeding them and the tail rings out. While
  on, the dry signal dips slightly as LEVEL rises, so the sum doesn't get louder.
- **FILTER** sweeps a resonant low-pass down and back up once per length, open on the
  beat; **FLANGER** sweeps once per length; **ROLL** repeats the current division of
  the beat, starting on the grid, and rejoins the track when switched off. These are
  inserts: LEVEL is the dry/wet balance.

The FX tempo and beat phase come from the target deck's grid (for MAIN: the sync
master, else the playing deck with the higher fader). TAP (two or more taps) overrides
it; click the tempo next to TAP to follow the decks again. The unit sits after the
channel faders, so an echo tail carries on after the fader comes down, and it is
bypassed with per-deck outputs. Changing the effect or the target fades rather than
clicks.

`cargo run --release -p dj-loader --example fx_render -- TRACK OUT_DIR` renders 16 s of
a track through each effect (on from 4 s to 10 s) into WAVs, and prints what each
costs per 128-frame callback (about 2–3% here, both decks' playback included).

## Settings

The sliders button at the top right (or **U**) opens the settings. **↑/↓** choose a
row, **◂ ▸** (or **←/→**, or a click on the value) change it, **Esc** closes. Every
change applies at once and is saved to `~/.config/dj/settings.toml` (macOS:
`~/Library/Application Support/dj/settings.toml`; `--settings FILE` uses another).

- **Audio:** output device, buffer size and outputs (internal mix, or per deck: A on
  1–2 and B on 3–4 for an external mixer, with the software EQ, faders and filter
  bypassed). These reopen the audio stream on the spot; the decks carry on after a
  short gap. If the new setup won't open (say, per-deck outputs on a 2-channel device),
  the old one is kept and the row says why. When the dropout watchdog has had to raise
  the buffer, the buffer row says so.
- **Decks:** tempo range and key lock, for both decks now and at every start (**R**
  and **K** still change one deck).
- **Display:** key notation (Open Key, Camelot, musical) and waveform colours.

Command-line options (`--device`, `--buffer`, `--rate`, `--key-notation`,
`--waveform`) override the file for that run and are never written into it. A file
that can't be read is kept as `settings.toml.bad` and the defaults are used.

## Recording

**REC** in the top bar (or **W**) records the main output, exactly as sent to the
speakers, to a 24-bit WAV at the engine's sample rate. The button shows how long it
has been recording; press it again to stop. Files are named by the local date and time
(`2026-11-10 21-04-33.wav`) and go into `Recordings` in the music folder
(`~/Music/Recordings`, or `--recordings DIR`), so they appear in the library.
With per-deck outputs the mix happens in the external mixer, so REC captures only what
goes to outputs 1–2 (deck A).

The audio thread only copies the main bus into a 4-second lock-free queue; a writer
thread saves it. If the disk stalls for longer than that, the gap is filled with
silence (keeping the timing right) and reported when the recording stops. While
recording, the file is called `….wav.part` with its header updated every second; if
the app dies, the next start repairs and renames it. A recording longer than WAV's
4 GB limit (about 4 hours at 48 kHz) carries on in `… part 2.wav`.

## Key lock and sync

Key lock runs each deck through a time-stretcher; on this machine two locked decks use
about 2% of each 2.7 ms callback (7% worst case): `cargo run --release -p dj-stretch
--example stretch_cost`. The stretcher's C++ is in `third_party/` and only needs a C++
compiler. The playhead always shows the frame you hear: the stretcher is primed on every
start and jump so its first output is exactly that frame.

The beatgrid is fitted to the kick markers; grids with a confidence under 0.6 are not
used (the header shows `~BPM` and sync is unavailable for that deck).

SYNC works as in Serato or Rekordbox. Turning it on for a deck syncs the other deck too
and makes that one the **master** (or makes this deck master if the other is empty).
The follower takes the master's tempo and stays on its beat, correcting drift with a
rate offset of at most ±4%. Tempo changes on either synced deck move both. Turning SYNC
off keeps the deck's current tempo; if the master turns it off, the other deck leads.
A following deck you start lands on the master's beat. Pressing S says on screen what
happened, or why sync isn't possible (an empty deck, or a deck with no beatgrid).

Quantize (**Q**) makes cue points snap to the nearest beat and ←/→ jump by whole bars
(4 beats), so jumping never knocks a deck off the beat. Hot cues, loops and beat jump
(Milestone 5) will snap too.

## Test data

To tune the kick detector on real music, `cargo run --release -p dj-loader --example
kicks -- FILE` prints how many kicks it found and how steady their spacing is.

`test-data/` holds generated tones only, made by `tools/gen-test-data.sh` (needs
`ffmpeg`). The loader tests decode each of them.
