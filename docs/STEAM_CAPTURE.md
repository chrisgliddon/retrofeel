# Steam game capture — GameHub workflow

RetroFeel records Steam/Wine games as three-point sessions (video + frame-synced
input log + mic/transcript) so human playtesters and LLMs can talk about *what
was pressed when* with a shared, exportable artifact.

## The one rule: let GameHub launch the game, attach to it

There are two launch modes (`steam.launch_mode` in config):

- **`GameHub`**: the game is launched from
  GameHub with its tuned Wine build (wine-proton 11 + GPTK). RetroFeel only
  *attaches* — it finds the running Wine process, captures its window with
  ScreenCaptureKit, and observes input with a listen-only event tap. It never
  injects or consumes events.
- **`WineDirect`**: RetroFeel spawns its own Wine (homebrew) with its own
  prefix. **Don't use this for GameHub-installed games** — it runs a second,
  differently-configured Wine environment next to GameHub's (separate
  wineserver, separate prefix, divergent saves) and can disrupt input routing.

## Recording a session

1. Launch the game from GameHub and get to gameplay.
2. Run:

```sh
retrofeel gamehub
```

RetroFeel discovers the running title from GameHub's Steam manifests, attaches
without owning or stopping the game, shows the status overlay, and starts a
recording. Stop it from the RetroFeel HUD or with `R`. If more than one GameHub
game is running, select one with only `--appid <ID>`; for example,
`retrofeel gamehub --appid 42`. Use `--no-record` when you only want to
attach and show the overlay.

`retrofeel steam` provides the same zero-argument auto-detection across all
configured attach-compatible Steam/Wine installs. The detailed `steam`
options (`--name`, `--exe`, `--attach`, and `--pid`) remain available for
diagnostics and unusual installs.

### Automated hardware smoke test

On macOS, the scripted smoke test drives GameHub's visible UI,
launches the selected installed title, records a bounded RetroFeel session,
injects one harmless unmapped key pulse to exercise the raw input log, and
validates the finalized video/input/frame-map timing. Derived reports and a
contact sheet are written under `/private/tmp`; the source session is not
modified, and the game is deliberately left running:

```sh
python3 scripts/gamehub_capture_smoke.py --profile /path/to/my-game.json
```

The default 8,400 frames run for 2m20s at 60 fps, crossing the historical
ffmpeg backpressure failure point. Use `--frames 600` for a short harness
check or `--no-launch` to attach to a game that is already running. The script
requires macOS Screen Recording and Accessibility permission for the invoking
terminal, plus `tesseract`, `ffmpeg`, and `ffprobe` on `PATH`. It never stops
GameHub or the game process.

Each run also writes `focus-events.jsonl` beside the report. Its 100 ms
samples identify the frontmost application, exact game PID/window, and
RetroFeel PID throughout attach and recording. The smoke check fails if
RetroFeel becomes frontmost or the selected game is not frontmost once
recording begins. After a successful GameHub attach, RetroFeel asks macOS to
focus the exact Wine game PID. macOS 14+ may reject background activation; if
the game does not respond after typing `retrofeel gamehub` in Terminal, click
the game once when the status overlay appears. The first click can be consumed
by the focus handoff, but subsequent input is delivered normally.

Alternatively, launch the RetroFeel UI with no args. The Steam section merges
configured games, installed GameHub manifests, and every installed game from
the native Steam client's primary and additional libraries. Clicking a
GameHub card attaches to its running GameHub process (and tells you to launch
it from GameHub first when needed). Clicking a native card launches it through
the Steam client; native-client capture is outside this GameHub workflow.

Notes:
- For games with regional executables or launchers, supply `--exe` or an exact
  `--pid` when automatic discovery selects the wrong process.
- Window targeting prefers an exact title match, ignores trademark glyphs
  (`™`/`®`), and never matches RetroFeel's own mirror window. If targeting
  still fails, run with `--verbose` — every on-screen window title is logged.

## macOS permissions (grant to your terminal when using `cargo run`)

- **Screen Recording** — ScreenCaptureKit window capture.
- **Input Monitoring** + **Accessibility** — the listen-only CGEventTap.
- **Microphone** — the mic track.

For exact GameHub/Wine PID attaches, the session event tap retains only
keyboard and mouse events whose macOS target PID is the selected game. Typing
into Terminal, chat, or another app while recording is therefore not
misrepresented in `input.json` as input delivered to the game. Native-Steam
title fallback cannot apply this filter until an exact game PID is available.

## Gamepad capture

Native Steam capture uses a single process-lifetime poller thread over Apple's
GameController framework; every captured frame snapshots it into the input log
(`gamepad_buttons` / `gamepad_axes`). Verify a controller is visible:

```sh
cargo test -p retrofeel-steamcapture --test gamepad_smoke -- --ignored --nocapture
```

GameHub/Wine capture deliberately does not initialize any RetroFeel gamepad
client. Wine and Steam Input own that route, while RetroFeel passively records
their translated keyboard/mouse output through a listen-only event tap. This
avoids both the raw-HID resets caused by gilrs/IOHIDManager and duplicate input
delivery caused by introducing a second controller client during play.

## If GameHub itself doesn't see the controller

GameHub's Wine/Steam Input route must expose the controller to the game:

1. Retest with no RetroFeel session running (older builds' HID leak could make
   the pad unresponsive everywhere until the process exited).
2. Leave GameHub's gamepad compatibility option disabled when Steam Input is
   enabled. Enabling both creates two candidate controller routes.
3. Check System Settings → Privacy & Security → **Input Monitoring** and
   **Bluetooth** include GameHub — Wine/SDL raw-HID access needs it.
4. If it still fails, test wired (USB-C) to split "Bluetooth problem" from
   "Wine problem", and consider updating the pad firmware via the Xbox
   Accessories app.

## Build note: Swift bridge under Command Line Tools

Swift 6.4's default SwiftPM engine (`swiftbuild`) currently fails without full
Xcode ("Could not initialize build system"). The `screencapturekit`/`apple-*`
build scripts shell out to `swift build`, so builds of the capture crates need
the shim:

```sh
PATH="$PWD/scripts/swiftpm-native-shim:$PATH" cargo build -p retrofeel
```

Durable fix: install full Xcode (or a CLT release where `swiftbuild` works)
and drop the shim.

The smoke harness requires a user-supplied JSON profile with `app_id` (positive
32-bit integer), `title`, `library_title`, and `process_names` (nonempty list of
executable filenames). Copy `scripts/profiles/fictional.json` outside the checkout
and replace its fictional metadata with your own installed game. No title is
selected by default. Automatic application discovery filters launch/configuration
executables and otherwise selects the largest candidate; explicit overrides
remain available. Choose whether the optional synthetic key probe is appropriate
for your game, or pass `--no-input-probe`.
