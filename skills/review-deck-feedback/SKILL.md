---
name: review-deck-feedback
description: Review recordings made by RetroFeel's Steam Deck sidecar, either in place over SSH or from an optional explicit pull. Use for Deck session discovery, Steam game-ID scoping, Steam Input/controller mapping, frame-aligned input, or gameplay-feel measurements; do not use for RetroFeel desktop-app recordings.
---

# Review Deck feedback

Use the directory containing this file as `SKILL_DIR`. Resolve symlinks when
needed; never assume the current working directory is the RetroFeel repository.

This skill owns every Steam Deck sidecar capture, including one that has
already been copied to another machine. Use `review-retrofeel-feedback` only
for recordings created by the RetroFeel desktop app.

## Pin the game before selecting a session

Never select the newest recording across all games.

1. Read the applicable project `AGENTS.md` files and the current request before
   listing sessions. In a project dedicated to one game, keep this concise
   entry in the project's `AGENTS.md`:

   ```md
   ## RetroFeel feedback target
   - Game title: <title>
   - Steam game ID: `<id>`
   ```

   For a native Steam title, the game ID is its App ID. For a non-Steam
   shortcut, retain the exact 64-bit `external_capture.game_id` reported by the
   recorder.
2. If the entry is missing, resolve the ID from the named game and the Deck
   session metadata. When the match is unambiguous, add or update the entry
   while preserving the rest of `AGENTS.md`; ask the user when it is ambiguous.
   Do not pin a multi-game or tooling repository to an arbitrary game.
3. Filter `retrofeel-deck-recorder list --json` by exact `game_id` first. Only
   then may the newest matching session be used when no session was named.
4. An explicitly named session from another game is a task-scoped reference
   override. Review it, call out the different game ID, and leave the project's
   saved target unchanged unless the user asks to change it.

Always report the selected session ID, game title, and game ID.

## Review on the Deck by default

Check reachability once without treating an offline Deck as a recording error:

```sh
ssh -o ConnectTimeout=5 deck true
ssh deck '~/.local/bin/retrofeel-deck-recorder list --json'
```

From the returned objects, choose an explicit session using the game-scoping
rules above. Keep its `id` and `directory`; do not substitute the first object
from an unfiltered list. Sidecars can be inspected in place over SSH:

```sh
retrofeel_session_id='<selected id>'
retrofeel_session_dir='<selected directory>'

ssh deck "jq '{game: .core.name, game_id: .external_capture.game_id, status: .external_capture.status, frames: .frame_count, dropped: .dropped_frames}' '$retrofeel_session_dir/manifest.json'"
ssh deck "jq . '$retrofeel_session_dir/input-devices.json'"
ssh deck "jq . '$retrofeel_session_dir/controller-map.json'"
ssh deck "python3 - summary '$retrofeel_session_dir'" \
  < "$SKILL_DIR/scripts/session_inputs.py"
```

The last command streams the analysis program, not the recording, and reads
`input.json` on the Deck. The same form works with `events`, `runs`, and
`cadence`. Remote inspection is sufficient for session identity, capture
health, controller tracks and layouts, exact input transitions, held spans,
and cadence. Use the manifest-declared source video with Deck-side `ffprobe`
when only media metadata or decode health is needed.

## Pull only when useful

Pulling is optional. Use it when the user asks for a local copy or when repeated
frame extraction, contact sheets, OpenCV tracking, full local decode, or durable
derived artifacts make local media access worthwhile. Always pass the selected
session explicitly:

```sh
scripts/deck-pull-recording.sh --target deck \
  --session "$retrofeel_session_id"
```

Never use the pull helper's unfiltered `latest` default. For an already-local
Deck session:

```sh
python3 "$SKILL_DIR/scripts/session_inputs.py" summary SESSION
python3 "$SKILL_DIR/scripts/session_inputs.py" summary SESSION --device DEVICE_ID
python3 "$SKILL_DIR/scripts/session_inputs.py" runs SESSION --min-duration 0.25
python3 "$SKILL_DIR/scripts/video_metrics.py" probe SESSION
```

`SESSION` may be a session directory, `manifest.json`, `input.json`, or
`video.mkv`.

## Audit before trusting completeness

Use the bundled `session_inputs.py audit SESSION --game-id GAME_ID --probe-media
--json` on the host that can read the referenced media. Add `--recorder
retrofeel-deck-recorder` when the installed CLI supports strict fragment validation. It exits 1 for health
findings, including wrong segment identity, decoded/input count mismatch,
missing clock provenance and partial layouts. Read the per-device raw/canonical
coverage table before summarizing a selected port. Shared ports do not imply
an active mapping, and a neutral selected stream does not prove no input.
Parsed canonical transitions are compared as objects, not serialized key order.

A successful transcription job does not establish narration coverage. Inspect
its exact media source and segments; do not describe a short music marker as
proof the player supplied no feedback. Legacy `complete` status and zero drops
or uncertainty must not override contradictory media/device evidence.

For recorder versions supporting it, `validate-media --source EXACT_MPD` checks
the declared fragment list directly. Steam DASH probing can request an extra
fragment after the declared end; retain the warning and validate the actual
inventory instead of either ignoring warnings or assuming corruption.
`derive-session --session EXACT_ID --out NEW_ROOT` creates a versioned derivative
with source hashes and clock provenance. Never modify originals to make an
audit pass.

## Explicit development tests

Passive recording remains the default. For requested synthetic-controller work,
read the repository's `docs/DECK_DEVKIT.md` and use `scripts/deck-devkit.py` (or
installed `retrofeel-deck-devkit`). Coordinate device ownership first; use its
shared lease for every cooperating workload. An idle process check does not
reserve the Deck. Keep direct input delivery, Steam-route testing, physical HID,
game-received actions, pixels and human feel as separate evidence. The headless
receiver smoke test proves delivery only; it is not a game/performance trial.

## Evidence rules

- Verify the manifest game ID before analyzing anything, then validate capture
  status, frame/input counts, timestamp monotonicity, and dropped frames.
- Read `controller-map.json` before translating captured Steam/XInput names to
  printed physical labels.
- Use each frame's `elapsed_us`; nominal FPS is descriptive for Deck captures.
- Treat input timing as measured and motion inferred from pixels as estimated.
  State units, calibration, sample count, excluded intervals, and confidence.
- Do not infer private game state or mechanics from controller input alone.

## Resources

- Read [REFERENCE.md](REFERENCE.md) for SSH operations, file schemas,
  controller names, measurement recipes, caveats, and troubleshooting.
- `scripts/session_inputs.py` provides deterministic input summaries, runs,
  events, and cadence comparisons locally or streamed to the Deck.
- `scripts/video_metrics.py` provides local video probing, contact sheets,
  tracking, camera compensation, and jump estimates. It needs `ffprobe`,
  NumPy, and OpenCV; see [requirements.txt](requirements.txt).
