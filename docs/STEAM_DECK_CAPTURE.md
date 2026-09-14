# Steam Deck Game Recording input companion

`retrofeel-deck-recorder` runs invisibly beside Steam in Gaming Mode. Steam
continues to own video and game audio. The companion observes Steam's
**on-demand** Game Recording lifecycle, records the virtual Steam Input evdev
devices, exact-allowlisted physical HID controllers, and optional exact-
allowlisted keyboard/mouse evdev capabilities. It snapshots the active
controller layout and converts every retained event track to held state for
each encoded video frame.

Some games use native Steam Input actions and remove their virtual Xbox device
after launch. The recorder therefore captures virtual and allowlisted physical
controllers concurrently for every game. No capture-time source guess can
discard the other track, and exact physical identity matching remains the trust
boundary so unrelated controllers are unaffected.

The implementation does not inject into games, install Decky, change Steam's
controller configuration, or copy Steam video while recording.

## What a finalized session contains

- `input.json` and `input.ron`: frame-indexed mapped and raw controller,
  keyboard, and mouse state;
- `input-events.jsonl`: original evdev events or decoded HID report changes on
  `CLOCK_BOOTTIME`;
- `input-devices.json`: stable device IDs, exact identity, available/admitted
  capabilities, admission reason, ports, source, and axis ranges;
- `controller-layouts/` and `controller-map.json`: best-effort snapshot and
  normalized Steam Input bindings, including Steam's translated 32-bit app ID
  for non-Steam shortcuts;
- `manifest.json` and `manifest.ron`: Steam IDs, clip reference, clock anchor,
  capture status, and exact source video path;
- `steam-audio-transcript.srt`: optional Whisper transcript of Steam's single
  mixed audio track (game/system sound plus microphone when Steam captures it);
- `capture.json`: lifecycle information used to inspect interrupted sessions.

Steam's DASH `session.mpd` remains the source of truth on the Deck. The pull
script remuxes it losslessly to `video.mkv` while transferring a session.

This captures controller intent and timing, not a game's private world state.
Character velocity can be estimated alongside the video or compared with
instrumented builds, but it cannot be recovered exactly from commercial games
without game-specific telemetry.

## Build and install

The Deck binary is Linux x86-64. A manual distribution should include the binary,
user service, example configuration, and installer.

From an extracted release archive:

```sh
./deck-install-recorder.sh --target deck
```

From this repository, pass a compatible Linux binary:

```sh
scripts/deck-install-recorder.sh \
  --target deck \
  --binary /path/to/retrofeel-deck-recorder
```

The installer checks that SSH is logged in as `deck`, installs only into the
user's `~/.local/bin` and `~/.config/systemd/user`, then enables:

```text
retrofeel-deck-recorder.service
```

Defaults need no configuration. To override them, copy
`deck-recorder.example.ron` to:

```text
~/.config/retrofeel/deck-recorder.ron
```

Useful Deck-side commands:

```sh
~/.local/bin/retrofeel-deck-recorder doctor
~/.local/bin/retrofeel-deck-recorder list
~/.local/bin/retrofeel-deck-recorder reconcile
~/.local/bin/retrofeel-deck-recorder sync-archives
~/.local/bin/retrofeel-deck-recorder migrate-archives --help
~/.local/bin/retrofeel-deck-recorder repair-archives --help
~/.local/bin/retrofeel-deck-recorder youtube-status --json
~/.local/bin/retrofeel-deck-recorder sync-youtube --dry-run --json
journalctl --user -u retrofeel-deck-recorder.service -f
```

`doctor` may report no virtual pad while no game is running. Run it again with
a Steam Input game open before treating that result as a permissions problem.

### Keyboard and mouse privacy policy

Keyboard and mouse capture is off by default for both physical and virtual
devices. Discovery reads sysfs identity and capability files first; the
recorder opens an evdev event node only after policy admits at least one of its
capabilities. Known Steam virtual gamepad capability remains admitted by
default, but keyboard or mouse capabilities exposed by the same composite
device still require an exact rule.

An `input_device_allowlist` rule always matches `vendor`, `product`, and `name`
exactly. Add `unique_id` and/or `physical_path` when `doctor` reports them to
constrain the identity further. A non-Steam device must expose a stable unique
ID or physical path. Virtual/uinput devices also require
`allow_virtual: true`; matching their name and IDs is not sufficient.

```ron
input_device_allowlist: [
    (
        vendor: 0x1234,
        product: 0x5678,
        name: "RetroFeel Test Keyboard and Mouse",
        unique_id: Some("retrofeel-kbm-1"),
        capabilities: [keyboard, mouse],
        allow_virtual: true,
    ),
],
```

Rules can request `gamepad`, `keyboard`, and `mouse` independently. A composite
device may list all three; each evdev event is read once and routed to exactly
one admitted capability. `doctor` reports every keyboard/mouse candidate as
admitted or rejected with its exact identity and reason, and reports configured
rules that have no exact visible match. An intentional unconfigured rejection
is healthy; a configured rule with invalid capabilities or no match is a failed
check.

The recorder never converts key events into text. It retains held evdev key
codes and symbolic names only for frame-aligned analysis. The bounded in-memory
`input_ring_seconds` window is the only input retained before Gamescope reports
recording started; keyboard/mouse held state older than that window is not
prefixed into a session. Events received after Gamescope reports recording
stopped are not written while the recorder waits for Steam's clip-saved marker.

For an allowlisted keyboard/mouse, `doctor --json` includes an admission reason
and an `evdev open succeeded` result. Admission does not bypass Linux
permissions. Custom uinput fixtures may need a narrowly matched udev rule that
grants the Deck user access; never make every `/dev/input/event*` globally
readable. With the allowlist removed, the same device must be rejected and its
event node must be absent from the service's open file descriptors. The only
exception is a composite Steam virtual pad already opened for its default
gamepad capability; in that case, verify keyboard/mouse events are filtered and
only gamepad events are retained.

### Concurrent physical capture for native-action games

Some native-action games switch from Steam's virtual Xbox device to native
Steam Input actions during startup. An explicitly configured physical fallback
is read from `/dev/hidraw*`, which remains observable when Steam has removed or
exclusively grabbed the corresponding evdev node. The decoder converts the
controller report to the same standard Xbox-style buttons, sticks, triggers,
and d-pad fields used for virtual Steam Input captures.

The built-in Steam Deck controller (`28de:1205`) is supported through its
64-byte Valve vendor report. The exact descriptor check rejects the Deck's
separate keyboard and mouse HID interfaces. Its trackpad clicks, Quick Access,
and L4/R4/L5/R5 buttons are also retained as named raw controls. This report
layout follows Linux's upstream `drivers/hid/hid-steam.c` implementation.

Configure only exact physical controllers that are safe to observe. For every
game, the recorder retains all Steam virtual pads and all exact physical
allowlist matches concurrently:

```ron
physical_gamepad_allowlist: [
    (
        vendor: 10462, // 0x28de
        product: 4613, // 0x1205
        name_contains: Some("Valve Software Steam Deck Controller"),
        unique_contains: None,
    ),
    (
        vendor: 11720, // 0x2dc8
        product: 12307, // 0x3013
        name_contains: Some("8BitDo Ultimate wireless Controller for PC"),
        unique_contains: Some("controller-unique-id"),
    ),
],
```

Each event retains its `device_id`, and `input-devices.json` records whether its
track came from `steam_virtual` or the compatibility-named
`physical_fallback` source. `input.json` preserves every track under
`raw_host.gamepads[]`; the legacy merged/mapped fields continue to prefer Steam
virtual port zero when present. `capture.json.input_sources` lists every source
retained by the session, while singular `input_source` remains the preferred
legacy projection for older consumers. Eligible devices that appear after
recording starts are added without dropping existing tracks. This avoids losing
physical input when a stale virtual pad disappears or never produces gameplay
events. Legacy configs may still contain `physical_fallback_game_ids`; the
field is accepted for migration but ignored and can be removed.

## JBL Quantum 360P resume quirk

The JBL Quantum 360P Console USB receiver (`0ecb:20ab`) exposes stereo 48 kHz
game playback, mono 16 kHz chat playback, and mono 16 kHz microphone capture.
Keep SteamOS's stock `output:analog-stereo+input:mono-fallback` ACP profile and
mixer routing for this headset.

The stock Game endpoint does need a bounded startup allowance. When it reopens
after an idle suspend, the receiver can initially report stale hardware
pointers and leave PipeWire in a permanent XRUN with repeating
`snd_pcm_avail after recover: Broken pipe` errors. The exact JBL
`analog-stereo` node rule sets `api.alsa.start-delay` to 1024 samples (about
21 ms at 48 kHz); it does not change the ACP profile, mixer paths, steady-state
period size, or device suspend.

Install the exact user-local WirePlumber rule with:

```sh
scripts/deck-install-jbl-quantum-360p-profile.sh --target deck
```

The installer checks that the remote account is `deck`, stages only
`~/.config/wireplumber/wireplumber.conf.d/51-jbl-quantum-360p.conf`, and
preserves a differing fragment as a timestamped backup. It does not modify
SteamOS, change volume, select a device, reset USB, or restart any service by
default.

Restart the Deck from its Power menu to activate the rule before Steam and
games connect. Do not restart PipeWire beneath a running Steam session: Steam
UI and game clients can retain stale PulseAudio
connections and produce no playback streams until Steam or the Deck restarts.
`--restart-audio` exists for controlled maintenance with Steam and games
stopped.

After activation, `pactl list cards` should show the stock
`output:analog-stereo+input:mono-fallback` profile, `wpctl status` should expose
the JBL `analog-stereo` sink, and that node should report a startup delay of
1024. Test idle resume, Steam UI sounds, and several game transitions, then
check that no new XRUN or `Broken pipe` entries appeared:

```sh
ssh deck 'pactl list cards; wpctl status -n'
ssh deck "pw-dump | jq -c '
  .[] | select(.type == \"PipeWire:Interface:Node\") |
  .info.props |
  select(.[\"node.name\"] ==
    \"alsa_output.usb-JBL_JBL_Quantum_360P_Console-00.analog-stereo\") |
  {node: .[\"node.name\"], start_delay: .[\"api.alsa.start-delay\"]}
'"
ssh deck 'journalctl --user -b -u wireplumber -u pipewire \
  --grep="XRUN|Broken pipe" --no-pager'
```

To remove the quirk, delete only its exact fragment and restart the Deck from
the Power menu:

```sh
ssh deck 'rm -f ~/.config/wireplumber/wireplumber.conf.d/51-jbl-quantum-360p.conf'
```

## Offline transcription on the Deck

Steam Game Recording currently writes one stereo audio representation to its
DASH session. Enabling `Audio_Mic` mixes microphone input into that track; it
does not provide RetroFeel with a separate microphone stem. Accordingly, Deck
transcripts are stored under `external_capture.audio_transcript` with source
`steam_mixed_audio`. They are deliberately not labelled as the manifest's
dedicated `mic_audio` or narration transcript.

Install the pinned user-space whisper.cpp runtime and English `base.en` model:

```sh
scripts/deck-install-transcription.sh --target deck
```

The installer verifies the official release archive and model hashes and does
not modify immutable SteamOS. Manually transcribe a completed session (or omit
`--session` for the newest one):

```sh
ssh deck '~/.local/bin/retrofeel-deck-recorder transcribe \
  --session fg_43_20260810_192450'
```

Enable post-finalization transcription in `deck-recorder.ron` with:

```ron
transcription: (
    automatic: true,
    executable: "/home/deck/.local/bin/retrofeel-whisper-cli",
    model: Some("/home/deck/.local/share/retrofeel/transcription-models/whisper-base-en/ggml-base.en.bin"),
    model_id: "whisper-base.en",
    language: "en",
    threads: 4,
),
```

Whisper cannot recover microphone dialogue that Steam did not capture. Check
Steam's microphone setting and the PipeWire source level when an SRT is empty;
game music in the mixed track can also reduce speech-recognition accuracy.

## Automatic portable archives

Steam keeps Game Recording video as a DASH playlist and fragments. Configure
`recording_archives` when selected games also need continuously reconciled,
ordinary MP4 files in a user-facing or network-shared directory:

```ron
recording_archives: [
    (
        game_id: "9223372058363166720",
        display_name: "Example Workshop",
        destination_dir: "/home/deck/Videos/Example-Workshop-History",
        format: mp4,
    ),
    (
        game_id: "9223372041183297536",
        display_name: "Example Meadow World",
        destination_dir: "/home/deck/Videos/Example-Meadow-World-History",
        format: mp4,
    ),
    (
        game_id: "9223372045478264832",
        display_name: "Example Meadow Unity",
        destination_dir: "/home/deck/Videos/Example-Meadow-Unity-History",
        format: mp4,
    ),
    (
        game_id: "9223372062658134016",
        display_name: "Example Meadow Blocks",
        destination_dir: "/home/deck/Videos/Example-Meadow-Blocks-History",
        format: mp4,
    ),
],
archive_sync_interval_seconds: 60,
```

Rules match the complete Steam game ID exactly, including the 64-bit ID Steam
assigns to a non-Steam shortcut. The watcher performs encoding on a background
thread so video work cannot pause input capture. Each reconciliation pass:

- considers only complete, frame-aligned sessions for configured game IDs;
- creates `<Game-Name>__<recording-id>.mp4` through an exact
  `.<stem>.partial.mp4` file and an atomic rename;
- preserves the source wall-clock timeline while producing exact 60 fps CFR
  H.264 High 4:2:0 video and AAC-LC 48 kHz audio, avoiding player-dependent
  interpretations of Steam's variable-rate packet cadence;
- places the MP4 metadata atom before media data for Fast Start;
- verifies the container, delivery profile, CFR cadence, audio presence,
  duration, audio/video start alignment, Fast Start layout, and a full
  audio/video decode pass;
- records the verification outcome, stream details, duration, packet count,
  byte size, and modification time in a versioned `.archive.json` receipt;
- refreshes small manifest, controller-map, and mixed-audio transcript sidecars;
- skips expired Steam sources and degraded desktop-session duplicates; and
- leaves both the original Steam recording and RetroFeel companion session
  untouched.

The first pass backfills every still-readable matching session. Run
`sync-archives --json` for an immediate machine-readable reconciliation report;
subsequent watcher passes are idempotent. A cross-process lock prevents a
manual command and the background worker from remuxing the same session at the
same time.

Older configurations that omit `format` continue producing Matroska archives.
Migrate managed MKVs one at a time with a dry run first:

```sh
~/.local/bin/retrofeel-deck-recorder migrate-archives \
  --format mp4 --delete-verified-mkv --dry-run --json
~/.local/bin/retrofeel-deck-recorder migrate-archives \
  --format mp4 --delete-verified-mkv --json
```

For each file, migration produces and fully verifies the partial MP4, publishes
it atomically, writes its receipt, and only then deletes that exact MKV. A
failed packet, stream, duration, decode, or rename check retains the original.
SRT, transcript JSON, manifest, controller-map, and receipt sidecars remain.
Only one additional recording-sized file exists during migration.

MP4 archives made by the older variable-frame-rate stream-copy path can be
replaced in place, one at a time, without changing Steam source recordings or
sidecars. Always inspect the dry run first:

```sh
~/.local/bin/retrofeel-deck-recorder repair-archives --dry-run --json
~/.local/bin/retrofeel-deck-recorder repair-archives --json
```

Each replacement is encoded to a sibling partial, checked against the prior
MP4's wall-clock duration, fully decoded, and only then atomically renamed over
the derived archive. The receipt is upgraded after publication; an interrupted
run is safe to repeat.

## YouTube publishing and retention

YouTube publishing is optional and uses one sequential worker across isolated
publisher profiles. Network latency cannot pause input capture or archive
creation. Each profile owns an exact game-ID route, expected channel ID, OAuth
token, rollout boundary, and private resumable state:

```ron
youtube: None, // retained for older single-channel configurations
youtube_publishers: [
    (
        publisher_id: "example-workshop",
        // Fictional IDs demonstrating multiple routes for one publisher.
        game_ids: [
            "9223372058363166720",
            "9223372054068199424",
            "9223372049773232128",
        ],
        enabled: false,
        upload_not_before: "2026-01-01T00:00:00Z",
        oauth_client_path: "/home/deck/.local/state/retrofeel/youtube/oauth-client.json",
        oauth_token_path: "/home/deck/.local/state/retrofeel/youtube/example-workshop/oauth-token.json",
        expected_channel_id: "<EXACT_EXAMPLE_WORKSHOP_CHANNEL_ID>",
        privacy_status: private,
        category: gaming,
        caption_language: "en",
        timezone: "America/Vancouver",
        retention_days: 7,
        idle_only: true,
    ),
    (
        publisher_id: "example-meadow-world",
        game_ids: [
            "9223372041183297536",
            "9223372045478264832",
            "9223372062658134016",
        ],
        enabled: false,
        upload_not_before: "2026-01-01T00:00:00Z",
        oauth_client_path: "/home/deck/.local/state/retrofeel/youtube/oauth-client.json",
        oauth_token_path: "/home/deck/.local/state/retrofeel/youtube/example-meadow-world/oauth-token.json",
        expected_channel_id: "<EXACT_EXAMPLE_MEADOW_WORLD_CHANNEL_ID>",
        privacy_status: private,
        category: gaming,
        caption_language: "en",
        timezone: "America/Vancouver",
        retention_days: 7,
        idle_only: true,
    ),
],
```

Historical game IDs may share a route when they are old identities of the same
game; each profile must still include at least one currently configured archive
rule. The rollout cutoff, route, and exact channel ID become immutable when each
publisher writes its local policy. A game ID cannot appear in two profiles, and
receipts cannot move across profiles or channels. Only complete manifests and
current verified MP4 receipts qualify; diagnostic degraded or manifest-less
archives stay local. Eligible work is newest-first so new playtests are not
stuck behind the backfill. Keep `enabled: false` until channel and canary checks pass; status,
OAuth, dry runs, and one explicitly requested canary remain available.

Set up the Google side in this order:

1. Select the Google Cloud project that owns the OAuth client and enable
   YouTube Data API v3. An existing project can be reused.
2. Configure a personal-use OAuth consent screen, move it to In Production,
   and create a Desktop OAuth client. Testing-mode refresh tokens expire after
   seven days. If Production is unavailable, finish the Branding configuration
   using the app's real homepage and privacy-policy URLs. Testing can be used
   temporarily by adding the configured channel owners as test users, but it requires
   reauthorization after seven days. Production status and YouTube's upload
   compliance audit are separate settings.
3. Download the client JSON to the shared `oauth_client_path`. From Deck Desktop
   Mode run `youtube-auth --publisher example-workshop`, select the Example Workshop
   channel, then repeat for `example-meadow-world` and select that channel. The
   PKCE loopback flow requests `youtube.force-ssl`; each profile stores a
   separate refresh token. See [Google's installed-app OAuth guidance](https://developers.google.com/identity/protocols/oauth2/native-app).
4. Confirm `youtube-status --json` reports an exact match for all configured channels.
   Use each channel's owning Google/Brand Account: delegated YouTube Studio
   permissions do not grant API access. Service accounts cannot own an ordinary
   creator channel.
5. For Private uploads, set `privacy_status: private`; no upload-compliance
   audit is needed to lift a Private-only restriction. If Unlisted or Public
   is wanted later, request the YouTube API compliance audit first. Uploads
   from a new unaudited project can be forced Private even when Unlisted was requested; see the
   [`videos.insert` audit restriction](https://developers.google.com/youtube/v3/docs/videos/insert).
6. Leave automatic uploads disabled initially. Run one bounded Private
   smoke per channel with `sync-youtube --publisher ID --private-canary --json`.
7. Run `sync-youtube --dry-run --json`. Confirm processing, playback, A/V
   synchronization, Private visibility, and captions before setting both
   profiles to `enabled: true`. A repeated disabled sync can upload another
   new video; it is not a read-only status check. Use `youtube-status` for
   non-mutating local status.
8. Confirm the channel is verified for videos longer than 15 minutes when the
   recording workload requires it.

The publisher waits until no Steam-launched game is running. It checks activity
between 8 MiB chunks, so launching a game pauses rather than loses a resumable
upload. It uses persisted byte progress, bounded backoff, `Retry-After`, and
restart recovery. Before replacing an expired
upload, it searches recent channel uploads for the stable
`RetroFeel-Session-ID` description marker. It persists a discovered video ID
immediately, polls processing, and refuses to start retention unless YouTube
reports both successful processing and the privacy requested by that receipt.
Subscriber notifications are disabled, the category is Gaming, and videos are
declared not made for children. Titles use each archive rule's display name, so
each fictional example title remains distinguishable on its configured channel.

A non-empty English `steam-audio-transcript.srt` is uploaded independently;
the video never waits for transcription. Empty, failed, cancelled, or
unavailable transcription gets an explicit terminal no-caption state. A
pending caption blocks pruning, and a regenerated SRT is updated by content
hash. Caption insertion currently costs 400 quota units; see the
[`captions.insert` reference](https://developers.google.com/youtube/v3/docs/captions/insert).
The publisher conservatively reserves at most 95 of the separate 100 daily
video insertions and 8,800 of the ordinary 10,000 daily units across configured
profiles. Caption insertion costs 400 units and update costs 450, so the
caption work can span several Pacific-time quota days. Excess
captions remain pending without blocking already published videos.

OAuth tokens, quota state, and resumable URLs live only under
`~/.local/state/retrofeel/youtube/`, with directory mode `0700` and file mode
`0600`, and are excluded from logs. Public `.youtube.json` receipts beside the
archives retain the publisher, channel, requested privacy, video ID, and
publication state. Seven days after processing, privacy, and current caption
handling are confirmed, the worker writes a prune tombstone and removes only
the derived MP4. Manifests, transcripts, SRTs, archive receipts, YouTube
receipts, and video IDs remain indefinitely. A later SRT is still attached
after MP4 pruning. Failed or incomplete uploads retain the MP4.

## Recording in Gaming Mode

1. Enable Steam Game Recording and select the on-demand/manual mode.
2. Launch a game normally in Gaming Mode.
3. Use Steam's **Start Recording** action.
4. Play the reference segment.
5. Use Steam's **Stop Recording** action and wait for Steam to save the clip.
6. Confirm the companion lists a finalized `fg_…` session.

Background recording sessions (`bg_…`) are intentionally ignored. The input
recorder keeps a short ring before the foreground start marker so controller
and admitted keyboard/mouse events are not lost while Steam initializes the
encoder.

The playtester-facing Start/Stop action belongs exclusively to Gamescope/Steam.
RetroFeel does not bind, intercept, or infer that shortcut and does not use game
launch/exit as a recording boundary. It creates a session only after parsing
Steam's eligible recording-start marker, stops appending input immediately on
the matching recording-stop marker, and uses the later clip-saved marker only
to finish resolving and packaging Steam's video.

## Optional pull to the development machine

Most sidecar and input review can run in place over SSH. Pull a session when
local video decoding, repeated frame access, or a retained copy is useful.

List completed sessions and filter them by the target game's exact `game_id`:

```sh
ssh deck '~/.local/bin/retrofeel-deck-recorder list --json'
```

Then pull the selected recording, optionally choosing a destination:

```sh
scripts/deck-pull-recording.sh \
  --target deck \
  --session fg_123_20260730_150035 \
  --out ./reference-captures
```

The destination is created atomically. Existing directories are never
overwritten. Sidecars are copied first, then the remote CLI uses `ffmpeg` to
remux Steam's referenced video and audio to Matroska over SSH.

## Coding-agent analysis skill

The version-controlled `skills/review-deck-feedback/` skill teaches coding
agents how to select recordings by the project's Steam game ID, review
sidecars in place over SSH, optionally pull a selected capture, interpret Steam
Input names, and measure input and video primitives. The complementary
`skills/review-retrofeel-feedback/` skill is limited to recordings created by
the desktop app. Install live global links for Codex, Gemini CLI, Claude Code,
Kimi Code, and OpenCode:

```sh
skills/install-retrofeel-steam-deck.sh
```

Its bundled tools can summarize exact input timing, compare held movement and
button cadence, make timestamped contact sheets, track an actor with optional
camera compensation, and estimate jump latency, height, and airborne duration.
The skill explicitly treats commercial-game motion as a visual estimate rather
than private game telemetry.

## Clock alignment

Steam logs a first-video `CLOCK_BOOTTIME` anchor and a live normalized PTS.
An archived segment can rebase that PTS to zero, so subtracting the live
normalized PTS from the anchor is insufficient to align the archive.

Finalization now requires the exact recording directory in the exact clip;
it never substitutes a sibling or another game's newest video. It parses
Steam's `clip.pb` game/timeline/segment identities and offsets, retaining the
whole split-segment list. Only the recognized untrimmed, zero-origin mapping
is reconstructed automatically. Missing or conflicting anchors, trims, multiple timelines,
or ambiguous mappings remain partial with raw input preserved.

The validator resolves the finite local fragment inventory from Steam's static
DASH manifest, then decodes those exact video and audio fragments. This avoids
the DASH demuxer's speculative N+1 request after the declared final fragment.
Missing/extra fragments, decode warnings, invalid PTS, changing source hashes,
video packet/decoded-frame disagreement and substantially truncated duration
are errors. Declared duration is metadata, not an exact frame count. Canonical
input is emitted only after validation and uses decoded presentation timestamps.
Steam writes longer durations with minutes and hours (`PT10M30.639S`, for
example); these are supported alongside seconds-only durations with the same
strict fragment inventory and timestamp checks.

`media-validation.json` records source SHA256s and decoded timing;
`archive-clock.json` records the clip hash, segment mapping, both live clock
values, archived origin and 1 ms metadata uncertainty. Failures retain
`.partial-*` data with `finalization-health.json`. Missing/unknown alignment
never reports zero uncertainty. Audio presence is probed; Steam mixed audio
is not a separately aligned microphone stem.

Read-only inspection and versioned reconstruction:

```sh
python3 skills/review-deck-feedback/scripts/session_inputs.py audit SESSION \
  --game-id GAME_ID --probe-media --json
retrofeel-deck-recorder validate-media --source /exact/recording/session.mpd
retrofeel-deck-recorder derive-session --session EXACT_RECORDING_ID \
  --out /new/derivative-root --transcribe
```

Derivation also accepts an exact stopped recording retained as
`.partial-EXACT_RECORDING_ID` when no completed directory exists. It preserves
the partial original and records its path and sidecar hashes in the derivative.
A missing transcript after a finalization failure does not establish that
Steam lost the microphone audio: inspect the exact mixed audio source first.

The audit exits 1 for findings and includes each device's raw and canonical
coverage, partial mappings, parsed transition comparison and transcript/media
consistency. `--latest` accepts a root only with `--game-id`. Pulling `latest`
also requires `--game-id`; transcription requires an exact `--session`.

`derive-session` requires a stopped on-demand source and a fresh output root.
It copies raw sidecars/layouts, records original hashes in
`repair-provenance.json`, and rebuilds only after exact-source/clock validation.
Original media and sidecars are never repaired in place. Optional transcription
runs against the newly selected exact media only after validation succeeds.

See [the explicit development devkit](DECK_DEVKIT.md) for shared device leases,
synthetic scenarios, the independent receiver and guarded game adapters.

At each encoded frame timestamp, the sampler consumes every admitted event at
or before that timestamp. Held keys and mouse buttons remain set until release.
`REL_X`/`REL_Y` and `REL_HWHEEL`/`REL_WHEEL` accumulate between frames and reset
after each sample. `MouseState.wheel_x` is positive for wheel-right and
`wheel_y` is positive for wheel-up. These additive wheel fields default to zero
when older logs are read. Raw add/remove lifecycle markers reset all held state
for a stable `device_id`, so an `eventN` recreation cannot leave a key or button
stuck or rewrite earlier events.

## Live keyboard/mouse acceptance

Use a dedicated test device whose exact identity is allowlisted; avoid a
general-purpose keyboard if its activity should not enter the pre-roll ring.

1. With no matching rule, restart the service and run `doctor --json`. Confirm
   a dedicated keyboard/mouse device is rejected and its event node is not open
   by the service. For a Steam-pad composite, confirm only its gamepad
   capability is admitted and keyboard/mouse events are absent.
2. Add the exact rule (and `allow_virtual: true` for uinput), restart, and
   confirm `doctor` admits only the requested capabilities.
3. Before the playtester starts Gamescope recording, confirm no RetroFeel
   session exists. Optionally emit one distinctive transition inside the
   configured pre-roll window.
4. Have the playtester start Gamescope/Steam recording through the normal UI or
   shortcut. Hold/release a key, click/release the intended mouse buttons, send
   relative motion at multiple report rates, and send vertical/horizontal wheel
   ticks. If testing churn, remove and recreate the device with the same exact
   identity, then repeat one transition.
5. Have the playtester stop Gamescope/Steam recording, generate a little more
   input, wait for the finalized `fg_…` session, and pull it with
   `scripts/deck-pull-recording.sh --target deck`.
6. Require exact parity in the pulled session:

   ```sh
   jq '.frame_count' manifest.json
   jq 'length' input.json
   ffprobe -v error -count_frames -select_streams v:0 \
     -show_entries stream=nb_read_frames -of default=nw=1 video.mkv
   ```

7. Inspect transitions without reconstructing typed text:

   ```sh
   jq -c '.[] | select(
     (.raw_host.keyboard_key_codes | length) > 0 or
     ((.raw_host.mouse // {}) | (.buttons // 0) != 0 or
       (.dx // 0) != 0 or (.dy // 0) != 0 or
       (.wheel_x // 0) != 0 or (.wheel_y // 0) != 0)
   ) | {frame, elapsed_us, raw_host}' input.json
   rg 'KEY_|BTN_|REL_(X|Y|WHEEL|HWHEEL)|"lifecycle"' input-events.jsonl
   ```

Confirm held state spans the expected frames, relative/wheel deltas appear once
in the correct interval, the optional pre-roll transition precedes video frame
zero on `CLOCK_BOOTTIME`, churn produces lifecycle markers without stuck state,
and post-stop input is absent. Controller acceptance should still confirm every
virtual/physical track, controller-map normalization, and a decodable remux.

## Remaining live acceptance checks

- suspend/resume once to verify `CLOCK_BOOTTIME` alignment;
- pull a completed session and play the remuxed `video.mkv` on the development
  machine.

An isolated PipeWire microphone track remains future work. Steam remains
responsible for the mixed game/microphone audio used by Deck transcription.
