#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
ssh_target=${RETROFEEL_DECK_SSH:-deck}
binary=${RETROFEEL_DECK_BINARY:-}

usage() {
  printf '%s\n' \
    "Usage: $0 [--target SSH_HOST] [--binary LINUX_BINARY]" \
    "" \
    "Installs the recorder and its user service. The Deck must be reachable." \
    "Defaults: target=deck, binary=release archive binary or Cargo Linux build."
}

while (($#)); do
  case "$1" in
    --target)
      ssh_target=$2
      shift 2
      ;;
    --binary)
      binary=$2
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

if [[ -z "$binary" ]]; then
  for candidate in \
    "$script_dir/retrofeel-deck-recorder" \
    "$repo_root/target/x86_64-unknown-linux-gnu/release/retrofeel-deck-recorder" \
    "$repo_root/target/release/retrofeel-deck-recorder"
  do
    if [[ -f "$candidate" ]]; then
      binary=$candidate
      break
    fi
  done
fi

service_file=
for candidate in \
  "$script_dir/retrofeel-deck-recorder.service" \
  "$repo_root/apps/retrofeel-deck-recorder/packaging/retrofeel-deck-recorder.service"
do
  if [[ -f "$candidate" ]]; then
    service_file=$candidate
    break
  fi
done

if [[ -z "$binary" || ! -f "$binary" ]]; then
  printf '%s\n' \
    "Linux recorder binary not found." \
    "Pass --binary, or download/extract the Linux release archive." >&2
  exit 1
fi
if [[ -z "$service_file" ]]; then
  printf 'Service file not found.\n' >&2
  exit 1
fi

remote_user=$(ssh -o ClearAllForwardings=yes "$ssh_target" 'id -un')
if [[ "$remote_user" != deck ]]; then
  printf 'Refusing to install as remote user %q; expected deck.\n' "$remote_user" >&2
  exit 1
fi

remote_binary="/tmp/retrofeel-deck-recorder-install-$$"
remote_service="/tmp/retrofeel-deck-recorder.service-$$"
scp -o ClearAllForwardings=yes "$binary" "$ssh_target:$remote_binary"
scp -o ClearAllForwardings=yes "$service_file" "$ssh_target:$remote_service"

ssh -o ClearAllForwardings=yes "$ssh_target" "
  set -eu
  mkdir -p \"\$HOME/.local/bin\" \"\$HOME/.config/systemd/user\"
  install -m 0755 '$remote_binary' \"\$HOME/.local/bin/retrofeel-deck-recorder\"
  install -m 0644 '$remote_service' \"\$HOME/.config/systemd/user/retrofeel-deck-recorder.service\"
  rm -f '$remote_binary' '$remote_service'
  systemctl --user daemon-reload
  systemctl --user enable retrofeel-deck-recorder.service
  systemctl --user restart retrofeel-deck-recorder.service
  systemctl --user --no-pager --full status retrofeel-deck-recorder.service
"

printf '\nInstalled on %s. Validate while a game is open with:\n' "$ssh_target"
printf '  ssh %s ~/.local/bin/retrofeel-deck-recorder doctor\n' "$ssh_target"
