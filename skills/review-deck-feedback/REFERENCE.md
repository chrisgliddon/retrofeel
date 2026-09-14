# Deck feedback reference

## Environment and safety

The configured SSH alias is `deck`. It is passwordless and should resolve the
Steam Deck on the current network (normally `steamdeck.local`). The remote Unix
account is `deck`.

Use the alias rather than embedding an IP address:

```sh
ssh -G deck | sed -n 's/^\\(hostname\\|user\\) /\\1 /p'
ssh -o ConnectTimeout=5 deck true
```

The Deck is expected to move between networks. If SSH is unavailable, continue
with an already-pulled session that matches the project target rather than
repeatedly retrying. Do not alter Steam Input layouts, delete Steam recordings,
install system packages, use root, or change Gaming Mode unless the user
specifically requests it.

Useful read-only or recorder-scoped commands:

```sh
ssh deck '~/.local/bin/retrofeel-deck-recorder doctor'
ssh deck '~/.local/bin/retrofeel-deck-recorder list --json'
ssh deck 'systemctl --user status retrofeel-deck-recorder.service --no-pager'
ssh deck 'journalctl --user -u retrofeel-deck-recorder.service -n 100 --no-pager'
```

Review sidecars and input in place whenever that is enough for the question.
The optional pull helper is in the RetroFeel repository:

```sh
scripts/deck-pull-recording.sh --target deck --session SESSION_ID
```

Always pass the already-selected session ID. Omitting it chooses an unfiltered
global latest session and is not safe when several projects are recording
concurrently. The helper refuses to overwrite an existing destination. Its
default local destination is
`recordings/steam/<session-id>`.

## Session files

A Deck sidecar contains the following files. `video.mkv` appears after an
optional pull/remux; before that, the manifest references Steam's source video
on the Deck.

| File | Meaning |
| --- | --- |
| `manifest.json` | Shared RetroFeel metadata, game identity, timing, paths, and status |
| `input.json` | One held-state `InputFrame` for every encoded video frame |
| `input.ron` | RON equivalent of `input.json` |
| `input-events.jsonl` | Original Linux evdev or physical HID transitions on `CLOCK_BOOTTIME`, keyed by device ID |
| `input-devices.json` | Every retained virtual/allowlisted physical device identity, source, port, ranges, and capabilities |
| `controller-map.json` | Normalized snapshot of active Steam Input bindings |
| `controller-layouts/` | Original Steam layout snapshots |
| `capture.json` | Recorder lifecycle and clock observations |
| `video.mkv` | Lossless remux of Steam's DASH video and game-audio streams |

`manifest.json.external_capture.status` is normally `complete`. A
`degraded_alignment` capture may still be useful, but its input/video offset
must be treated as uncertain. A dedicated microphone/narration track is absent;
a transcript of Steam's mixed audio may be present in the external-capture
fields when Deck transcription was enabled.

## Time model

Steam video markers and evdev events use Linux `CLOCK_BOOTTIME`. The recorder
probes the actual PTS of every encoded frame and samples held input at that
instant. Therefore:

- `input.json[i]` describes the input held for encoded video frame `i`;
- `elapsed_us / 1_000_000` is the frame-aligned session time;
- nominal `timing.fps` is descriptive, not the source of timestamps;
- `input-events.jsonl` is appropriate for sub-frame event ordering;
- `input.json` is appropriate for reproducible frame/video comparisons.

For a healthy pull, compare:

```sh
jq '.frame_count, .dropped_frames, .external_capture.status' manifest.json
jq 'length' input.json
ffprobe -v error -count_frames -select_streams v:0 \
  -show_entries stream=nb_read_frames -of default=nw=1 video.mkv
```

Small mux timestamp warnings do not prove lost content. Report them, then
verify full decode and frame counts.

## Input schema and controller semantics

Important `InputFrame` fields:

```text
frame
elapsed_us
state                 # RetroFeel/libretro mapped state
raw_host
  gamepad_buttons     # legacy merged view
  gamepad_axes
  gamepads[]          # preferred per-device view
    device_id
    port
    name
    buttons
    axes
```

Prefer `raw_host.gamepads[]` by exact `device_id`. Multiple tracks can share
port `0`, so use `input-devices.json` to distinguish `steam_virtual` from the
compatibility-named `physical_fallback` source. The recorder keeps every Steam
virtual pad and every exact allowlisted physical controller concurrently; use
`session_inputs.py ... --device DEVICE_ID` to inspect a specific track. The
legacy merged fields prefer virtual port zero and can therefore be neutral when
a native-action game stops using a stale virtual pad. On this Deck, the built-in
`Valve Software Steam Deck Controller` at `28de:1205` is decoded from its exact
64-byte vendor HID report. The physical 8BitDo model may likewise appear only
through the configured HID capture path.

Common Bevy/evdev names:

| Captured name | XInput convention | Typical meaning |
| --- | --- | --- |
| `South` | A | bottom face button |
| `East` | B | right face button |
| `West` | X | left face button |
| `North` | Y | top face button |
| `LeftThumb` | LS click | left-stick click |
| `RightThumb` | RS click | right-stick click |
| `DPadLeft`, etc. | D-pad | digital direction |
| `LeftStickX/Y` | left stick | analog movement |

Do not infer the printed physical label solely from this table. Steam Input can
remap any source. Consult `controller-map.json` and describe both names when
ambiguity matters: for example, “captured XInput East/B, corresponding to the
player's physical B in this layout.”

## Reliable input-derived measurements

These require no computer vision:

- press and release time;
- hold duration;
- press-to-press cadence and frequency;
- digital movement-run duration;
- analog magnitude, direction, deadzone crossing, and reversal time;
- simultaneous/chorded inputs;
- sequence and delay between actions;
- comparison of player input behavior across labeled time windows.

Examples:

```sh
python3 "$SKILL_DIR/scripts/session_inputs.py" events SESSION --button LeftThumb
python3 "$SKILL_DIR/scripts/session_inputs.py" runs SESSION \
  --control DPadRight --control LeftStickX+ --min-duration 0.5
python3 "$SKILL_DIR/scripts/session_inputs.py" cadence SESSION --button East \
  --window horse:120:141 --window foot:142:164.5
```

Press cadence is not an animation duration. A game may ignore presses during
recovery, buffer them, or vary the animation independently.

## Video-derived gameplay primitives

Start with a contact sheet and representative individual frames. Choose
intervals with uncomplicated scenery and an unobscured actor.

### Movement velocity and acceleration

1. Track the actor with a tight initial bounding box.
2. For a fixed camera, use `screen_vx`/`screen_vy`.
3. For a scrolling camera, pass `--background-roi X,Y,W,H`; use
   `world_vx`/`world_vy`.
4. Ignore samples below the chosen confidence floor.
5. Calibrate pixels to world units only when a tile size or other scale is
   visible and stable.
6. Trim turnarounds, collisions, scene transitions, and camera easing unless
   those are the phenomenon being measured.

Useful derived values include steady-state speed, time to 90% speed,
acceleration, braking distance, reversal latency, diagonal normalization, and
analog response curves.

### Jump timing

Track a stable visual feature on the actor and correct for camera motion. For
each jump-button rising edge, distinguish:

- input-to-takeoff latency;
- airborne duration from takeoff to landing;
- time to apex;
- maximum visual height;
- landing or recovery time before the next accepted action.

`video_metrics.py jumps` estimates the first four from a tracking CSV. Its
thresholds are game- and scale-dependent. Inspect samples around every detected
takeoff and landing before reporting a result.

Top-down games may depict a jump as sprite offset or shadow separation rather
than world-Y movement. Track the sprite and, when possible, the shadow
separately. A 3D game with perspective may require a ground-plane calibration
or manual keyframes.

### Other reusable primitives

- **Dash:** input edge to velocity spike, spike duration, cooldown until the
  next successful spike.
- **Attack/action:** input edge to first changed frame, active visual interval,
  recovery until locomotion or another action.
- **Turn response:** direction reversal edge to facing change and to negative
  velocity.
- **Stopping:** release time to zero motion and total stopping distance.
- **Coyote time/input buffering:** vary edge timing around ledges or recovery;
  a single natural-play recording cannot establish the boundary reliably.
- **Camera feel:** actor motion versus background motion, follow delay, catch-up
  speed, dead zone, and overshoot.
- **Analog curve:** group steady runs by stick magnitude and compare measured
  speed, after excluding collisions and camera transitions.

## Confidence and reporting

Always include:

1. session ID and game;
2. exact time windows;
3. controller signal names and mapping evidence;
4. whether the value came from input, pixels, or game telemetry;
5. units and calibration;
6. sample count and a robust statistic such as median;
7. tracking confidence and excluded intervals;
8. uncertainty or alternative interpretations.

Prefer “the capture supports” or “the visual estimate is” over claiming an
internal mechanic. Preserve raw recordings and write derived CSV, frames, and
reports to a temporary or explicitly requested output directory.

## Troubleshooting

- **Deck unreachable:** try one bounded SSH connection, then work locally.
- **Wrong game selected:** compare `external_capture.game_id` with the exact
  project target before considering session timestamps. A named reference
  session may differ, but an implicit selection may not.
- **No sessions:** verify Steam used foreground/on-demand recording (`fg_...`);
  background (`bg_...`) sessions are intentionally ignored.
- **No controller events:** run `doctor` while a Steam Input game is open and
  inspect `input-devices.json`.
- **Wrong physical button name:** read `controller-map.json`; report the
  captured XInput name as the stable identifier.
- **Frame mismatch:** do not force alignment. Check remux completeness, manifest
  status, full decode, and source DASH availability.
- **Tracker drift:** tighten the actor box, shorten the interval, lower template
  update, increase search radius, split at animation/scene changes, or use
  manual keyframes.
- **Implausible world speed:** background ROI probably includes the actor, HUD,
  animated water/particles, parallax, or a different depth plane.
