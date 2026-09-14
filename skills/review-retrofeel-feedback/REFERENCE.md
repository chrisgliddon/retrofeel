# RetroFeel desktop feedback reference

## Supported sources

The analysis helper accepts a directory or one of these files within a desktop
app session:

- `manifest.json` / `manifest.ron`;
- `input.json`;
- `video.mkv`.

Paths declared by a desktop manifest may be absolute or session-relative. The
helper resolves both. A manifest with
`capture_provenance.kind = steam_game_recording` belongs to
`review-deck-feedback`, even when its files are local.

## Timing contracts

| `video_timing.kind` | Input/video comparison |
| --- | --- |
| `frame_indexed` | Frame count and nominal FPS are the contract. |
| `cfr_resampled_sck` | `elapsed_us` and decoded master PTS share the encoded CFR grid. The frame map preserves irregular source PTS. |

`external_variable_frame_rate` identifies the Deck-sidecar path and should be
reviewed with `review-deck-feedback`.

`track_alignment.narration` is valid when `status=complete`; `degraded` is
still usable with its stated uncertainty but must not be described as
sample-accurate. `not_captured` is an intentional policy, not an unavailable
hardware capability.

## Generated files

All helper outputs require `--out` or use a temporary directory. Never place
contact sheets, decoded frames, LLM packs, tracking CSV, or reports below the
source session unless the user explicitly requests that exact directory.

The LLM pack is capped by `--max-frames` (default 12) and contains only a
summary, timing/map evidence, transitions, transcript excerpts, and selected
PNG frames. It is not a substitute for the master recording.

`transition-spans` converts changes in authoritative `input.json` into
timestamped held-state spans. `transcript-evidence` exposes the local
`TranscriptDocument.segments` timestamps without sending narration elsewhere.

`overlay-video --out OUTSIDE_SESSION/video-overlay.mkv` is an explicit
derivative renderer. It reads the clean master and artifacts, producing status
text, a mic waveform when present, and input-transition captions; it never
edits `video.mkv`.
