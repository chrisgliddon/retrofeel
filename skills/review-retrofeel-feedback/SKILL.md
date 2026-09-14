---
name: review-retrofeel-feedback
description: Review local recordings created by the RetroFeel desktop app. Use for libretro or macOS ScreenCaptureKit sessions and their manifest, video, input, frame-map, narration, transcript, or derived review artifacts; do not use for Steam Deck sidecar recordings, even after they are pulled locally.
---

# Review RetroFeel feedback

Use the directory containing this file as `SKILL_DIR`. Resolve symlinks when
needed; never assume the current working directory is the RetroFeel repository.

This skill owns sessions recorded directly by the RetroFeel desktop app:
libretro captures and local macOS ScreenCaptureKit captures. A Steam Deck
sidecar remains owned by `review-deck-feedback` after it is copied locally.
This workflow is local-only: do not SSH, download media, or modify the source
session merely to inspect it.

## Select the recording

Prefer a session path or ID named by the user. When selection is implicit,
honor any game title or Steam game ID in the project's `AGENTS.md` and never
choose a global newest recording across unrelated games. Verify the source in
`manifest.json`: desktop recordings use `capture_provenance.kind` of
`libretro` or `macos_screen_capture_kit`; a `steam_game_recording` sidecar
belongs to `review-deck-feedback`.

Write every derived artifact outside the source session unless the user names
that exact destination.

## Quick start

```sh
python3 "$SKILL_DIR/scripts/recording_analysis.py" summary SESSION
python3 "$SKILL_DIR/scripts/recording_analysis.py" contact-sheet SESSION \
  --start 0 --end 60 --every 5 --out /tmp/retrofeel-sheet.png
python3 "$SKILL_DIR/scripts/recording_analysis.py" frame SESSION --at 12.5 \
  --out /tmp/retrofeel-frame.png
python3 "$SKILL_DIR/scripts/recording_analysis.py" transitions SESSION
python3 "$SKILL_DIR/scripts/recording_analysis.py" transition-spans SESSION
python3 "$SKILL_DIR/scripts/recording_analysis.py" transcript-evidence SESSION
python3 "$SKILL_DIR/scripts/recording_analysis.py" overlay-video SESSION \
  --out /tmp/video-overlay.mkv
python3 "$SKILL_DIR/scripts/recording_analysis.py" llm-pack SESSION \
  --out /tmp/retrofeel-pack
```

`SESSION` may be a desktop session directory, `manifest.json`, `input.json`, or
`video.mkv`.

## Validation rules

1. Read `manifest.json`; require a complete session or explicitly report its
   degraded source/alignment status.
2. Treat `input.json` as authoritative. `input-transitions.jsonl` is a
   deterministic convenience index and must be reproducible from it.
3. Decode `video.mkv` when available and compare its decoded frame count with
   the input count. For `video_timing.kind = cfr_resampled_sck`, compare each
   decoded PTS with `InputFrame.elapsed_us` within one muxer timebase tick.
4. Check `frame-map.json` when declared: each encoded index and PTS must match
   the input log and its source PTS must remain visible. Report source-frame
   discards, grid duplicates, and writer dupes; never hide them.
5. Report source provenance, game-audio policy, narration offset/uncertainty,
   transcript availability, and degraded alignment before making timing claims.

Default visual extraction always uses the clean `video.mkv`, never an overlay
render. `overlay-video` reconstructs a shareable `video-overlay.mkv` from the
master, mic waveform, encoded FPS, and input-transition artifacts; it never
replaces the master.

Read [REFERENCE.md](REFERENCE.md) for desktop timing contracts, schemas,
commands, and interpretation.
