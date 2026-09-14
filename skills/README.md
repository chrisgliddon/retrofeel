# RetroFeel agent skills

The canonical, version-controlled skills are:

```text
skills/review-deck-feedback/
skills/review-retrofeel-feedback/
```

Install them as global, live links for Codex, Gemini CLI, Claude Code, Kimi
Code, and OpenCode:

```sh
skills/install-retrofeel-steam-deck.sh
```

The installer keeps this repository as the only source of truth:

- `review-deck-feedback` reviews Steam Deck sidecars in place over SSH or from
  an optional explicit pull, with Steam game-ID and controller-layout context;
- `review-retrofeel-feedback` reviews local sessions created by the RetroFeel
  desktop app (libretro or macOS ScreenCaptureKit).

A Deck sidecar remains in the first skill's scope after it is pulled locally,
so the two skills never compete for the same recording.

Both are linked below `~/.agents/skills/` for Codex, Gemini CLI 0.33+, Kimi
Code, and OpenCode, and below `~/.claude/skills/` for Claude Code.

Existing destinations are never overwritten. The installer removes the old
skill names only when they are symlinks to this repository, then installs the
new names.
