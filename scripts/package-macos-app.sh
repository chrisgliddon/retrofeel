#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 --binary-dir DIR --out RetroFeel.app [--version VERSION]" >&2
  exit 2
}

binary_dir=""
output=""
version=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary-dir) binary_dir="${2:-}"; shift 2 ;;
    --out) output="${2:-}"; shift 2 ;;
    --version) version="${2:-}"; shift 2 ;;
    *) usage ;;
  esac
done

[[ -n "$binary_dir" && -n "$output" ]] || usage
[[ "$output" == *.app ]] || { echo "output must end in .app: $output" >&2; exit 2; }
[[ -x "$binary_dir/retrofeel" ]] || { echo "missing retrofeel binary in $binary_dir" >&2; exit 1; }
[[ -x "$binary_dir/retrofeel-transcriber-worker" ]] || { echo "missing transcriber worker in $binary_dir" >&2; exit 1; }
[[ ! -e "$output" ]] || { echo "refusing to overwrite $output" >&2; exit 1; }

if [[ -z "$version" ]]; then
  version="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; data=json.load(sys.stdin); print(next(p["version"] for p in data["packages"] if p["name"] == "retrofeel"))')"
fi

contents="$output/Contents"
mkdir -p "$contents/MacOS" "$contents/Resources"
cp "$binary_dir/retrofeel" "$contents/MacOS/retrofeel"
cp "$binary_dir/retrofeel-transcriber-worker" "$contents/MacOS/retrofeel-transcriber-worker"
sed "s/__VERSION__/$version/g" apps/retrofeel/packaging/macos/Info.plist > "$contents/Info.plist"
printf 'APPL????' > "$contents/PkgInfo"

if [[ -d "$binary_dir/cores" ]]; then
  cp -R "$binary_dir/cores" "$contents/Resources/cores"
fi

plutil -lint "$contents/Info.plist" >/dev/null
codesign --force --deep --sign - "$output"
echo "$output"
