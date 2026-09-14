#!/usr/bin/env bash
set -euo pipefail

ssh_target=${RETROFEEL_DECK_SSH:-deck}
session=latest
game_id=
destination_root=${RETROFEEL_PULL_DIR:-"$PWD/recordings/steam"}

usage() {
  printf '%s\n' \
    "Usage: $0 [--target SSH_HOST] [--session RECORDING_ID|latest] [--game-id ID] [--out DIRECTORY]" \
    "" \
    "Copies RetroFeel sidecars and remuxes the referenced Steam video over SSH."
}

while (($#)); do
  case "$1" in
    --target)
      ssh_target=$2
      shift 2
      ;;
    --session)
      session=$2
      shift 2
      ;;
    --game-id)
      game_id=$2
      shift 2
      ;;
    --out)
      destination_root=$2
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'Unknown argument: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ "$session" == latest && -z "$game_id" ]]; then
  printf 'Selecting latest requires --game-id; otherwise pass an exact --session.\n' >&2
  exit 2
fi

sessions_json=$(ssh -o ClearAllForwardings=yes "$ssh_target" \
  '~/.local/bin/retrofeel-deck-recorder list --json')
selection=$(printf '%s' "$sessions_json" | python3 -c '
import json
import sys

requested = sys.argv[1]
sessions = json.load(sys.stdin)
game_id = sys.argv[2]
if game_id:
    sessions = [item for item in sessions if str(item["game_id"]) == game_id]
if not sessions:
    raise SystemExit("no finalized recordings on the Steam Deck")
if requested == "latest":
    selected = max(sessions, key=lambda item: (item.get("start_timestamp", 0), item["id"]))
else:
    selected = next((item for item in sessions if item["id"] == requested), None)
    if selected is None:
        raise SystemExit(f"recording not found: {requested}")
print(selected["id"])
print(selected["directory"])
' "$session" "$game_id")

session=$(printf '%s\n' "$selection" | sed -n '1p')
remote_directory=$(printf '%s\n' "$selection" | sed -n '2p')
if [[ ! "$session" =~ ^[A-Za-z0-9_-]+$ ]]; then
  printf 'Unsafe recording ID returned by Deck: %s\n' "$session" >&2
  exit 1
fi
if [[ ! "$remote_directory" =~ ^/home/deck/[A-Za-z0-9_./-]+$ ]]; then
  printf 'Unsafe recording directory returned by Deck: %s\n' "$remote_directory" >&2
  exit 1
fi

mkdir -p "$destination_root"
destination="$destination_root/$session"
if [[ -e "$destination" ]]; then
  printf 'Destination already exists: %s\n' "$destination" >&2
  exit 1
fi
temporary=$(mktemp -d "$destination_root/.${session}.partial.XXXXXX")
cleanup() {
  rm -rf -- "$temporary"
}
trap cleanup EXIT

ssh -o ClearAllForwardings=yes "$ssh_target" "tar -C '$remote_directory' -cf - ." | tar -C "$temporary" -xf -
ssh -o ClearAllForwardings=yes "$ssh_target" \
  "~/.local/bin/retrofeel-deck-recorder export-video --session '$session' --stdout" \
  > "$temporary/video.mkv"
mv "$temporary" "$destination"
trap - EXIT

printf 'Pulled %s to %s\n' "$session" "$destination"
