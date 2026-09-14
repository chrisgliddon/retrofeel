#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)

cd "$repo_root"
PATH="$repo_root/scripts/swiftpm-native-shim:$PATH" cargo build -p retrofeel

target_dir=$(
  cargo metadata --no-deps --format-version 1 \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p'
)
if [ -z "$target_dir" ]; then
  echo "Could not resolve Cargo's target directory" >&2
  exit 1
fi

cargo_bin_dir=${CARGO_HOME:-"$HOME/.cargo"}/bin
mkdir -p "$cargo_bin_dir"
install -m 0755 "$target_dir/debug/retrofeel" "$cargo_bin_dir/retrofeel"

echo "Installed $cargo_bin_dir/retrofeel"
case ":$PATH:" in
  *":$cargo_bin_dir:"*) ;;
  *) echo "Add $cargo_bin_dir to PATH before running retrofeel" ;;
esac
