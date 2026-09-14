#!/bin/sh
set -eu

CARGO_BIN=${CARGO:-cargo}
MIN_FREE_KIB=$((20 * 1024 * 1024))

set -- $(df -Pk . | awk 'NR == 2 { print $4, $5 }')
free_kib=$1
used_percent=${2%%%}
target_dir=$($CARGO_BIN metadata --no-deps --format-version 1 \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')

printf 'Cargo target: %s\n' "$target_dir"
printf 'Development volume: %s GiB free, %s%% used\n' \
  "$((free_kib / 1024 / 1024))" "$used_percent"

if pgrep -x cargo >/dev/null 2>&1 || pgrep -x rustc >/dev/null 2>&1; then
  printf '%s\n' 'Refusing to clean while cargo or rustc is active.' >&2
  exit 1
fi

if [ "$free_kib" -lt "$MIN_FREE_KIB" ] || [ "$used_percent" -ge 95 ]; then
  printf '%s\n' 'Development volume is under the 20 GiB/95% safety threshold.' >&2
  printf '%s\n' 'Clean safe reproducible artifacts before running the demo.' >&2
  exit 1
fi

# The target directory may be shared with unrelated worktrees. Clean only the
# application package instead of erasing the complete shared Cargo cache.
$CARGO_BIN clean -p retrofeel
$CARGO_BIN run -p retrofeel
