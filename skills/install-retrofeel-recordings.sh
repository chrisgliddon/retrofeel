#!/usr/bin/env bash
set -euo pipefail

# Backward-compatible entry point for the combined RetroFeel skill installer.
script_directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec "$script_directory/install-retrofeel-steam-deck.sh"
