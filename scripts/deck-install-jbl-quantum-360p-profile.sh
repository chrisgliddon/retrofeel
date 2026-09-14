#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
ssh_target=${RETROFEEL_DECK_SSH:-deck}
restart_audio=0
profile_file="$repo_root/apps/retrofeel-deck-recorder/packaging/51-jbl-quantum-360p.conf"

usage() {
  printf '%s\n' \
    "Usage: $0 [--target SSH_HOST] [--restart-audio]" \
    "" \
    "Installs an exact JBL Quantum 360P WirePlumber startup-delay quirk." \
    "SteamOS's stock audio profile and mixer routing remain in use." \
    "Restart the Deck to activate the quirk cleanly before Steam starts." \
    "--restart-audio is for a stopped Steam session; live clients may not reconnect."
}

while (($#)); do
  case "$1" in
    --target)
      ssh_target=$2
      shift 2
      ;;
    --restart-audio)
      restart_audio=1
      shift
      ;;
    --no-restart)
      printf '%s\n' '--no-restart is now the default and is accepted for compatibility.' >&2
      restart_audio=0
      shift
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

if [[ ! -f "$profile_file" ]]; then
  printf 'Profile fragment not found: %s\n' "$profile_file" >&2
  exit 1
fi
if [[ $(ssh "$ssh_target" 'id -un') != deck ]]; then
  printf 'Refusing to install anywhere except the Deck user account.\n' >&2
  exit 1
fi

remote_profile=$(ssh "$ssh_target" 'mktemp /tmp/retrofeel-jbl-quantum-360p.XXXXXX')
cleanup_remote() {
  ssh "$ssh_target" "rm -f '$remote_profile'" >/dev/null 2>&1 || true
}
trap cleanup_remote EXIT
scp "$profile_file" "$ssh_target:$remote_profile"

ssh "$ssh_target" "
  set -eu
  profile_dir=\"\$HOME/.config/wireplumber/wireplumber.conf.d\"
  installed_profile=\"\$profile_dir/51-jbl-quantum-360p.conf\"
  mkdir -p \"\$profile_dir\"
  if [ -f \"\$installed_profile\" ] && ! cmp -s '$remote_profile' \"\$installed_profile\"; then
    backup=\"\$installed_profile.backup.\$(date -u +%Y%m%dT%H%M%SZ)\"
    cp -p \"\$installed_profile\" \"\$backup\"
    printf 'Backed up existing profile to %s\\n' \"\$backup\"
  fi
  install -m 0644 '$remote_profile' \"\$installed_profile.new\"
  mv -f \"\$installed_profile.new\" \"\$installed_profile\"
  rm -f '$remote_profile'
  printf 'Installed %s\\n' \"\$installed_profile\"
"
trap - EXIT

if ((restart_audio)); then
  ssh "$ssh_target" '
    set -eu
    systemctl --user restart \
      pipewire.service pipewire-pulse.service wireplumber.service filter-chain.service
    systemctl --user --quiet is-active \
      pipewire.service pipewire-pulse.service wireplumber.service filter-chain.service

    game_sink_name=alsa_output.usb-JBL_JBL_Quantum_360P_Console-00.analog-stereo
    game_sink_id=
    attempt=0
    while [ "$attempt" -lt 20 ]; do
      game_sink_id=$(pw-dump | jq -r --arg name "$game_sink_name" \
        '\''.[] | select(.type == "PipeWire:Interface:Node") |
        select(.info.props["node.name"] == $name) | .id'\'' | head -n 1)
      if [ -n "$game_sink_id" ]; then
        break
      fi
      attempt=$((attempt + 1))
      sleep 0.25
    done
    if [ -z "$game_sink_id" ]; then
      printf "JBL analog-stereo node is not present; connect the headset and select it in Steam.\\n" >&2
    fi
    wpctl status -n
  '
else
  printf '%s\n' \
    'Startup-delay quirk staged without restarting live audio services.' \
    'Restart the Deck to activate it cleanly before Steam and games connect.'
fi
