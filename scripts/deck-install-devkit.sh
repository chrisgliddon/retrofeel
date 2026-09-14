#!/usr/bin/env bash
set -euo pipefail
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target=deck
if [[ ${1:-} == --target && $# == 2 ]]; then target=$2
elif [[ $# != 0 ]]; then printf 'Usage: %s [--target SSH_HOST]\n' "$0" >&2; exit 2
fi
remote_user=$(ssh -o ClearAllForwardings=yes "$target" 'id -un')
[[ "$remote_user" == deck ]] || { printf 'Expected remote user deck.\n' >&2; exit 1; }
ssh -o ClearAllForwardings=yes "$target" 'mkdir -p ~/.local/share/retrofeel-devkit/devkit ~/.local/bin'
rsync -az -e 'ssh -o ClearAllForwardings=yes' --exclude __pycache__ \
  "$root/scripts/devkit/" "$target:.local/share/retrofeel-devkit/devkit/"
scp -o ClearAllForwardings=yes "$root/scripts/deck-devkit.py" \
  "$target:.local/share/retrofeel-devkit/deck-devkit.py"
scp -o ClearAllForwardings=yes "$root/skills/review-deck-feedback/scripts/session_inputs.py" \
  "$target:.local/share/retrofeel-devkit/session_inputs.py"
ssh -o ClearAllForwardings=yes "$target" 'chmod +x ~/.local/share/retrofeel-devkit/deck-devkit.py ~/.local/share/retrofeel-devkit/session_inputs.py
ln -sfn ../share/retrofeel-devkit/deck-devkit.py ~/.local/bin/retrofeel-deck-devkit
ln -sfn ../share/retrofeel-devkit/session_inputs.py ~/.local/bin/retrofeel-deck-audit'
printf 'Installed explicit devkit and capture audit on %s.\n' "$target"
