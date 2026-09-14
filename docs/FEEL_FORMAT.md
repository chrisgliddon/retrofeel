# RetroFeel `.feel` packages

A `.feel` file is a self-contained directory package. Finder presents the
directory as one document because the macOS app exports the
`com.retrofeel.feel` package UTI. On other platforms it remains an ordinary,
inspectable directory.

Format version 1 is designed around three rules:

1. `input.json` and the captured media are the authoritative evidence.
2. Source capture files and imported source subtitles are never rewritten.
3. Alignment, transcripts, frame selections, and agent conclusions are
   derived artifacts with explicit provenance and hashes.

## Layout

```text
Example.feel/
├── feel.json                       # package index, version, SHA-256 inventory
├── manifest.json                   # RetroFeel session/capture manifest
├── video.mkv                       # clean-master video and game audio
├── input.json                      # authoritative timestamped input samples
├── input-transitions.jsonl         # deterministic derived input changes
├── raw-input-events.jsonl          # capture-specific evidence, when available
├── controller-map.json             # Steam input semantics, when available
├── context/
│   └── brief.md                    # portable instructions/domain context
├── transcripts/
│   ├── source/*.srt                # byte-preserved imported subtitles
│   └── aligned/<revision>/
│       ├── narration.srt           # subtitle timing in video elapsed time
│       ├── narration.json          # canonical segment representation
│       └── alignment.json          # affine fit, anchors, residuals, provenance
└── analysis/runs/<run-id>/
    ├── evidence.json               # disclosed, time-coded evidence
    ├── frames/*.png                # representative frames supplied to an agent
    ├── result.json                 # validated structured result
    ├── report.md                   # readable insights report
    ├── action-plan.md              # implementation-oriented plan
    └── agent.json                  # adapter, version, permissions, model, files
```

Capture-specific files such as controller layouts, frame maps, metadata, and
initial-state files are retained and inventoried when present. Paths in
`feel.json` must be package-relative and may not traverse through `..` or
symlinks. Required files, byte lengths, and SHA-256 hashes are checked before
analysis or import.

## Transcript alignment

The alignment transform is:

```text
video_seconds = scale × source_srt_seconds + offset_seconds
```

Automatic alignment pairs normalized SRT words with timestamped Whisper words,
keeps a monotonic anchor sequence, fits an affine transform robustly, rejects
outliers, and records median/P95/maximum residuals plus extrapolated regions.
Manual scale and offset remain available when the recording has no useful mic
audio. Both modes preserve the original SRT under `transcripts/source/`.

## CLI

Create and validate a package:

```sh
retrofeel package \
  --session recordings/session-123 \
  --out "recordings/Example.feel" \
  --transcript ~/Downloads/session.srt \
  --whisper-json /tmp/video-words.json

retrofeel validate "recordings/Example.feel"
```

Import another transcript revision with either automatic anchors or a manual
transform:

```sh
retrofeel align "recordings/Example.feel" \
  --transcript ~/Downloads/session.srt \
  --offset 18.498 --scale 0.999749834
```

Discover installed agents, analyze, or open the synchronized evidence view:

```sh
retrofeel agents
retrofeel analyze "recordings/Example.feel" --agent codex
retrofeel open "recordings/Example.feel"
```

The Codex, Claude Code, OpenCode, and Kimi adapters stage a bounded evidence
set and invoke the selected CLI with read-only/no-persistence permissions. The
result must match RetroFeel's JSON schema and cite valid evidence indices before
it is registered in the package.

## macOS document behavior

Opening or dropping a `.feel` package offers three choices: open the package in
place, import a fully validated copy into the configured recording library, or
cancel. New RetroFeel recordings use a `.feel` session directory from the
start, so a completed native capture is already a document package.

## Analysis result schema

New `result.json` files use analysis schema version **2**. Every insight includes
`title`, `implication`, `project_relevance`, and evidence indices. The project is
described in `context/brief.md`; the default brief requests general playtest
observations. Reports label this field “Project relevance”. The runtime rejects
other analysis versions and unknown fields, missing relevance, invalid time
ranges, and missing evidence references. Retain older results separately; this
release does not silently migrate them or provide legacy field aliases.

The `.feel` package format remains version **1**. Package, transcription, and
agent-run metadata version numbers are independent of analysis result versions.
