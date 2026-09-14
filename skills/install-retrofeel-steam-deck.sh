#!/usr/bin/env bash
set -euo pipefail

script_directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
link_skill() {
  local source_skill=$1
  local skill_name=$2
  local consumers=$3
  local destination_root=$4
  local destination="$destination_root/$skill_name"

  mkdir -p "$destination_root"
  if [[ -L "$destination" ]]; then
    local current
    current=$(readlink "$destination")
    if [[ "$current" == "$source_skill" ]]; then
      printf 'Already linked for %s: %s\n' "$consumers" "$destination"
      return
    fi
    printf 'Refusing to replace a different symlink: %s -> %s\n' \
      "$destination" "$current" >&2
    exit 1
  fi
  if [[ -e "$destination" ]]; then
    printf 'Refusing to replace an existing skill: %s\n' "$destination" >&2
    exit 1
  fi

  ln -s "$source_skill" "$destination"
  printf 'Linked for %s: %s -> %s\n' \
    "$consumers" "$destination" "$source_skill"
}

remove_renamed_link() {
  local legacy_name=$1
  local consumers=$2
  local destination_root=$3
  local destination="$destination_root/$legacy_name"
  local expected_source="$script_directory/$legacy_name"

  if [[ ! -L "$destination" ]]; then
    return
  fi

  local current
  current=$(readlink "$destination")
  if [[ "$current" != "$expected_source" ]]; then
    printf 'Leaving unrelated legacy symlink for %s: %s -> %s\n' \
      "$consumers" "$destination" "$current"
    return
  fi

  rm -- "$destination"
  printf 'Removed renamed link for %s: %s\n' "$consumers" "$destination"
}

for destination_root in "$HOME/.agents/skills" "$HOME/.claude/skills"; do
  if [[ "$destination_root" == "$HOME/.claude/skills" ]]; then
    consumers="Claude Code"
  else
    consumers="Codex, Gemini CLI, Kimi Code, and OpenCode"
  fi
  remove_renamed_link retrofeel-steam-deck "$consumers" "$destination_root"
  remove_renamed_link retrofeel-recordings "$consumers" "$destination_root"
done

# Install both complementary skills. Existing destinations are never replaced.
for skill_name in review-deck-feedback review-retrofeel-feedback; do
  source_skill="$script_directory/$skill_name"
  if [[ ! -f "$source_skill/SKILL.md" ]]; then
    printf 'Skill source is incomplete: %s\n' "$source_skill" >&2
    exit 1
  fi
  link_skill "$source_skill" "$skill_name" \
    "Codex, Gemini CLI, Kimi Code, and OpenCode" "$HOME/.agents/skills"
  link_skill "$source_skill" "$skill_name" "Claude Code" "$HOME/.claude/skills"
done

printf '%s\n' \
  "Installation complete. Restart existing agent sessions to refresh discovery."
