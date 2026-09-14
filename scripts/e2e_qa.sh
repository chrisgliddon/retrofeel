#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
OUT="$ROOT/qa-out"
APP_LOG="$OUT/retrofeel-record.log"

source "$ROOT/scripts/local_ci_common.sh"

cd "$ROOT"
rm -rf "$OUT" recordings saves
mkdir -p "$OUT"

cleanup() {
  if [[ -n "${APP_PID:-}" ]] && kill -0 "$APP_PID" 2>/dev/null; then
    kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

require_command ffmpeg \
  "ffmpeg is required for recording QA. On Debian/Ubuntu: sudo apt-get install ffmpeg"
preflight_linux_build_deps
preflight_linux_desktop_display
preflight_linux_x11_runtime

cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo test --workspace
cargo build -p mock-core

MOCK_SEARCH_DIRS=("$ROOT/target")
if [[ -d "$HOME/.cargo-target" ]]; then
  MOCK_SEARCH_DIRS+=("$HOME/.cargo-target")
fi
MOCK=$(find "${MOCK_SEARCH_DIRS[@]}" -name "libmock_core.*" 2>/dev/null | sort | tail -1 || true)
if [[ -z "$MOCK" ]]; then
  echo "mock core was not built" >&2
  exit 1
fi

cargo run -p headless-runner -- --core "$MOCK" --frames 30 --out "$OUT/headless"

RUST_LOG=info cargo run -p retrofeel -- --no-config --core "$MOCK" --record-frames 120 \
  >"$APP_LOG" 2>&1 &
APP_PID=$!

for _ in $(seq 1 90); do
  if grep -q "recording complete: 120 frames" "$APP_LOG"; then
    break
  fi
  if ! kill -0 "$APP_PID" 2>/dev/null; then
    cat "$APP_LOG" >&2
    echo "retrofeel exited before recording completed" >&2
    exit 1
  fi
  sleep 1
done

grep -q "recording complete: 120 frames" "$APP_LOG" || {
  cat "$APP_LOG" >&2
  echo "recording did not complete within 90 seconds" >&2
  exit 1
}

cleanup
unset APP_PID

SESSION=$(find recordings -mindepth 1 -maxdepth 1 -type d | sort | tail -1)
if [[ -z "$SESSION" ]]; then
  echo "recording session was not created" >&2
  exit 1
fi

cargo run -p retrofeel -- export --engine bevy --session "$SESSION" --out "$OUT/exports"
cargo run -p retrofeel -- export --engine unity --session "$SESSION" --out "$OUT/exports"
cargo run -p retrofeel -- export --engine godot --session "$SESSION" --out "$OUT/exports"
cargo run -p retrofeel -- export --engine unreal --session "$SESSION" --out "$OUT/exports"
cargo run -p retrofeel-export --example bevy_replay -- "$OUT/exports/bevy.ron"

grep -q '"frame_count": 120' "$OUT/exports/unity.json"
grep -q '"frame_count": 120' "$OUT/exports/godot.json"
grep -q '"frame_count": 120' "$OUT/exports/unreal.json"
test "$(grep -c '"frame":' "$OUT/exports/unity.json")" -eq 120
test "$(grep -c '"frame":' "$OUT/exports/godot.json")" -eq 120
test "$(grep -c '"frame":' "$OUT/exports/unreal.json")" -eq 120

rm -rf recordings saves
echo "retrofeel QA complete"
