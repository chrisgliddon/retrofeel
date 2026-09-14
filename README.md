# RetroFeel

RetroFeel is a Bevy desktop app for playing libretro cores, recording gameplay
as three synchronized streams — video, a microphone track with a timestamped
SRT transcript, and per-frame input — and exporting that input timeline to
Bevy, Unity, Godot, and Unreal.

The GUI runs libretro cores in an isolated worker process:

- one `retro_run()` call produces one emulated frame;
- the input log uses the same frame index as the encoded video;
- recordings include the mapped RetroPad state and raw host
  keyboard/gamepad/mouse state (captured only while RetroFeel has focus);
- the microphone track starts at recording start and skips pause segments, so
  its timeline matches the frame-indexed video;
- exports keep the exact core-reported FPS so engines can convert frames to
  seconds without drift.

## Status

Implemented:

- workspace, libretro host, mock-core test fixture, and headless runner;
- Bevy shell with video, audio, pause, fast-forward, input, and overlay;
- embedded SQLite persistence for paths, cores, BIOS, core options, bindings,
  and recent ROMs (a legacy `retrofeel.ron` is migrated on first launch);
- a console-focused Library with compact collection tabs and a box-art grid (covers
  are fetched from the libretro-thumbnails project and cached locally);
- Library and Settings screens with path pickers, BIOS/core scans, bindings,
  core options, recording list, and export buttons;
- optional bundled cores (`bundled-cores` feature) plus an in-app core catalog;
- recording to `video.mkv`, `mic.wav`, `transcript.json`, `transcript.srt`, `input.json`,
  `input.ron`, `manifest.json`, and `manifest.ron`;
- an invisible Steam Deck companion that aligns Steam Game Recording video
  with virtual Steam Input events and controller-layout snapshots;
- self-contained `.feel` document packages with immutable capture evidence,
  aligned SRT revisions, synchronized playback, and structured agent analyses;
- CLI and UI exports for Bevy, Unity, Godot, and Unreal.

Out of scope for the first pass:

- hardware-rendered libretro cores;
- bundled BIOS files or ROMs;
- deterministic replay/re-render from an initial save state;
- RAM ground-truth capture.

## Requirements

- Rust stable
- `ffmpeg` on `PATH` for recording video
- A libretro core dynamic library (`.so`, `.dylib`, or `.dll`)
- Your own legally obtained ROMs and BIOS files where the core requires them
- Optional, for mic transcription: install a verified local model under
  Settings → Transcription, or choose an existing whisper.cpp executable and
  model. Common Homebrew and user-local executable locations are discovered.
  Without a transcriber the microphone track is still recorded.

On macOS the OS asks for microphone permission on the first recording; if it
is denied, sessions record without a mic track. Mic capture can be turned off
in Settings ("Record Microphone").

Linux local builds and app QA need the desktop/audio development packages and
the X11 runtime library used by Bevy/winit:

```sh
sudo apt-get install ffmpeg pkg-config libasound2-dev libudev-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-0
```

## Build

```sh
cargo build -p retrofeel --release
```

Run the app:

```sh
cargo run -p retrofeel
```

Install the current workspace build as a normal shell command:

```sh
scripts/install-retrofeel-cli.sh
```

The installer puts `retrofeel` in Cargo's bin directory (normally
`~/.cargo/bin`, which Rustup adds to `PATH`). Re-run it after rebuilding a newer
checkout.

The first launch opens the Library screen. Use Settings to choose directories
for cores, system/BIOS files, ROMs, saves, states, and recordings.

## Quickstart

1. Put libretro cores in the configured cores directory.
2. Put BIOS files in the configured system directory when a core requires them.
3. Put ROMs in the configured ROM directory.
4. Open Settings and click Refresh Scans.
5. Assign a default core to each ROM extension if automatic extension matching
   is not enough.
6. Return to Library and launch a ROM.
7. Press `Esc` for the in-game overlay.
8. Start/stop recording from the overlay, or press `R`.
9. Save a screenshot from the overlay, or press `P`.
10. Export completed recordings from Settings, or use the CLI.

Recent ROMs are persisted in the config and shown at the top of the Library.

## CLI

Record a running Steam game without copying app IDs, executable names, or Wine
paths. Launch the game first, then run one of:

```sh
retrofeel gamehub
retrofeel steam
```

Both commands auto-detect a single running GameHub/Steam-Wine game, attach
without taking ownership of it, show the status overlay, and start recording.
Stop from the RetroFeel HUD or with `R`. If more than one game is running, add
only `--appid <ID>` to choose one. Use `retrofeel gamehub --no-record` to attach
with the overlay but not begin recording.

Launch a core directly:

```sh
cargo run -p retrofeel -- --core /path/to/core.dylib --rom /path/to/game.sfc
```

Launch a no-content core:

```sh
cargo run -p retrofeel -- --core /path/to/core.dylib
```

Use the config backend:

```sh
cargo run -p retrofeel -- config init
cargo run -p retrofeel -- config doctor
cargo run -p retrofeel -- config resolve --rom /path/to/game.sfc
cargo run -p retrofeel -- config options --core /path/to/core.dylib
```

Export a recording:

```sh
cargo run -p retrofeel -- export --engine bevy --session recordings/session-123
cargo run -p retrofeel -- export --engine unity --session recordings/session-123
cargo run -p retrofeel -- export --engine godot --session recordings/session-123
cargo run -p retrofeel -- export --engine unreal --session recordings/session-123
```

Exports default to `<session>/exports/`.

## Recording Files

Each recording session writes:

- `video.mkv`: final video with audio when audio is present;
- `video-no-audio.mkv`: raw encoded video stream;
- `audio.wav`: captured game audio samples when present;
- `mic.wav`: microphone track (when mic capture is enabled and permitted);
- `transcript.json`: canonical timestamped transcript of the mic track;
- `transcript.srt`: compatibility rendering of the transcript;
- `input.json` and `input.ron`: per-frame input timeline;
- `manifest.json` and `manifest.ron`: core, ROM, timing, binding, pause, and
  artifact metadata, including `mic_audio` and `transcript` paths.

Stopping a recording finalizes its media and manifests before transcription
starts in the background. A completed manifest may therefore briefly have no
`transcript` path; both manifest formats are updated atomically if transcription
succeeds.

Recording requires `ffmpeg`. The UI and writer both report a clear error when it
is missing.

## Engine Exports

See [docs/ENGINE_EXPORTS.md](docs/ENGINE_EXPORTS.md) for schema details and
Bevy, Unity, Godot, and Unreal replayer snippets.

There is also a small Bevy-oriented parser/replayer example:

```sh
cargo run -p retrofeel-export --example bevy_replay -- recordings/session-123/exports/bevy.ron
```

## Steam Deck reference capture

The Linux-only `retrofeel-deck-recorder` companion records Steam Input and
optional exact-allowlisted keyboard/mouse devices alongside Steam's native
on-demand Game Recording flow in Gaming Mode. See
[docs/STEAM_DECK_CAPTURE.md](docs/STEAM_DECK_CAPTURE.md) for installation,
privacy configuration, recording, pull, clock alignment, and live-validation
instructions.

The repository includes two globally installable coding-agent skills:
`review-deck-feedback` reviews Deck sidecars remotely or from an optional pull,
while `review-retrofeel-feedback` reviews recordings created by the desktop app
(libretro or macOS CFR ScreenCaptureKit):

```sh
skills/install-retrofeel-steam-deck.sh
```

See [skills/README.md](skills/README.md) for the shared Codex, Gemini CLI,
Claude Code, Kimi Code, and OpenCode layout.

## Verification

Run the standard checks:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

Run the offline app recording/export QA script:

```sh
scripts/e2e_qa.sh
```

This builds the mock core, runs the headless runner, records 120 frames with
the Bevy app, exports all four engine formats, and replays the Bevy RON export
with the example parser. It does not download external cores.

Run the networked real-core smoke script:

```sh
scripts/real_core_smoke.sh
```

This downloads and caches the libretro buildbot Gong core under
`target/real-cores/gong/`, verifies idle vs. scripted gameplay input, runs the
headless runner's determinism and save-state round-trip checks, and runs the
ignored `libretro-host` real-core smoke tests with `RETROFEEL_CORE`.

## Development and packaging

See [development guidance](docs/DEVELOPMENT.md), [contribution guidance](CONTRIBUTING.md),
and [security reporting](SECURITY.md). Packaging is manual; this repository has
no automated release publisher or GitHub Actions workflows.

retrofeel does not provide cores, BIOS files, or ROMs.

Steam Deck capture audits and the explicit synthetic-controller development tools
are documented in [DECK_DEVKIT.md](docs/DECK_DEVKIT.md). Passive recording remains
the default; test injection requires a separate command and shared device lease.
