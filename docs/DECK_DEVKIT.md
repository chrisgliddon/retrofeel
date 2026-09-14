# Steam Deck development tools

Passive recording remains the default. `scripts/deck-devkit.py` is a separate,
explicit Linux injection tool. The recorder never creates a test controller.
Python 3 and `evdev` are required for injection; the guard also uses `xdotool`
for game windows. The headless receiver fixture needs no display.

## Reserve the device

Coordinate with the current human/agent owner before any Deck workload. An
idle process list is not a reservation. All cooperating tools use the same
`$XDG_RUNTIME_DIR/retrofeel-deck.lease` lock (normally `/run/user/1000`).

```sh
python3 scripts/deck-devkit.py lease-status
python3 scripts/deck-devkit.py lease-run --owner agent-name --project project-name \
  --purpose 'read-only capture audit' --game-id 9223372058363166720 -- \
  retrofeel-deck-audit audit SESSION --game-id 9223372058363166720 --probe-media \
  --recorder retrofeel-deck-recorder
```

The owner retains an advisory `flock` for its process lifetime, with a separate
atomic JSON status containing owner, PID/start time, project, purpose, game,
phase, heartbeat and expiry. The stable lock file is never unlinked. Expiry is
informational: it never permits stealing a live lock. Process exit releases the
lock even if its metadata is stale. A process holding a hung lease must be
coordinated with its owner; the tool does not kill that owner.

The runner acquires this lease itself. Do not wrap `run` in `lease-run`.
Legacy tools must adopt the lease; it cannot reserve the Deck against a tool
that ignores it. The process guard independently checks Steam IDs inherited by
native and Proton processes, including games whose launcher wrapper exited.
It exports no process environments. Opaque known Deck system helpers are
excluded; other unreadable same-user processes fail closed. Unknown non-Steam
applications cannot be classified reliably, so the explicit reservation still
matters.

## Validate delivery with the minimal receiver

```sh
python3 scripts/deck-devkit.py run scripts/devkit/scenarios/receiver-smoke.json \
  --fixture --out /tmp/retrofeel-receiver-trial --owner agent-name --project retrofeel
```

This opens one synthetic controller and a separate receiver process that reads
only its owned evdev node. The fixture asserts stick motion, neutral return,
R3 press and release. It does not claim a Steam Input mapping, rendered outcome,
physical HID behavior, performance acceptance or human feel.

The output directory must be new. `report.json` contains acceptance/failure,
source scenario hash and ownership receipt. `injected.jsonl` retains planned,
actual and completed `CLOCK_BOOTTIME` dispatch times plus UTC correlation.
`received.jsonl` is independent application-received input. Cancellation,
focus loss, foreign games, PID reuse, late dispatch and missing receipts reject
a run. Cleanup neutralizes and closes only the owned synthetic device. It
never signals an attached game or forces focus back.

## Scenario and adapter contract

Version 1 JSON requires:

- `schema_version: 1`, `mode: diagnostic`, and an explicit `route: direct|steam`;
- a controller name prefixed `RetroFeel test `, vendor/product, explicit Linux
  gamepad button codes, and axis `[minimum, maximum, neutral]` triples;
- ordered `events` with `at_ms`, named `phase`, and axis/button states; optional
  explicit `action: disconnect|connect` events for a game adapter;
- a bounded `deadline_ms`, `timing_tolerance_ms`, and nonempty observable
  `assertions` with `match` fields and positive `min_count`;
- for a game, `expected_render_size: [width, height]`.

A game test attaches to a specified PID/window and requires an expected
executable SHA256. Supply `--pid`, `--window`, `--expected-sha256`, `--game-id`,
`--ready-receipt`, and `--telemetry`. The readiness JSON must contain `ready`,
`pid`, `/proc` `process_start`, `build_sha256`, and actual `render_size`.
Telemetry JSONL must contain `pid`, `process_start`, `build_sha256`, and
`boottime_us`, plus adapter-specific action/outcome fields. Rows from another
build/process or outside the run are excluded. The runner requires every
outcome assertion to pass; a fast trial that did no work fails.

`route: steam` additionally requires the target process to carry the requested
Steam identity. The user/adapter launches it through the real Steam route first.
Direct uinput delivery is not evidence of this route. Steam may expose additional
virtual pads for a synthetic device; game telemetry must identify which input
stream the game selected. Test fixtures do not suppress Steam's devices.

Game-specific readiness strings, screenshot logic, camera traces, view switching
and outcome assertions belong in adapters. Legacy UTC-only traces lack the full
process-bound boottime receipt contract and cannot be relabeled as accepted runs.
Validate each game adapter against its current build and input route.

## Replay one recorded device

```sh
python3 scripts/deck-devkit.py from-recording SESSION --game-id GAME_ID \
  --device EXACT_DEVICE_ID --template GAME_SCENARIO.json --deadzone 0.15 \
  --output REPLAY.json
```

The converter requires a completed canonical capture with an archive clock
receipt, selects by device identity rather than shared port, preserves VFR
elapsed times and held state, converts stick Y/ranges/deadzones, and records
source input SHA256. Unknown controls require an adapter instead of being
silently dropped. The scenario retains the template's outcome assertions.
This is clocked input replay, not deterministic replay of a commercial game.

Performance mode is intentionally rejected in v1. Before adding it, require
explicit instrumentation, actual rendering resolution, power/TDP/governor,
warmup, completed route, repeated trials/distributions, and game-specific
outcome assertions. Diagnostic tracing/capture cannot serve as an unqualified
performance measurement.

## Installation and tests

`scripts/deck-install-devkit.sh --target deck` installs only the tool and audit
helper; it does not change input permissions, packages, Steam settings or the
recorder service. Use `ssh -o ClearAllForwardings=yes deck` for remote work.

```sh
python3 -m unittest discover -s scripts/tests -p test_deck_devkit.py
python3 -m unittest discover -s skills/review-deck-feedback/tests -p test_session_inputs.py
```
