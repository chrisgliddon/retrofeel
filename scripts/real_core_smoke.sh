#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CORE_DIR="$ROOT/target/real-cores/gong"
CORE_ZIP="$CORE_DIR/gong_libretro.so.zip"
CORE="$CORE_DIR/gong_libretro.so"
OUT="$ROOT/qa-out/real-core-smoke"

source "$ROOT/scripts/local_ci_common.sh"

cd "$ROOT"

require_command curl \
  "curl is required to download the Gong libretro core. On Debian/Ubuntu: sudo apt-get install curl"
require_command unzip \
  "unzip is required to unpack the Gong libretro core. On Debian/Ubuntu: sudo apt-get install unzip"
preflight_linux_build_deps
preflight_linux_x11_runtime

case "$(uname -s):$(uname -m)" in
  Linux:x86_64)
    CORE_URL="https://buildbot.libretro.com/nightly/linux/x86_64/latest/gong_libretro.so.zip"
    ;;
  Linux:aarch64|Linux:arm64)
    CORE_URL="https://buildbot.libretro.com/nightly/linux/aarch64/latest/gong_libretro.so.zip"
    ;;
  *)
    echo "real_core_smoke.sh currently downloads Linux Gong .so cores only." >&2
    echo "Unsupported platform: $(uname -s) $(uname -m)" >&2
    exit 1
    ;;
esac

mkdir -p "$CORE_DIR" "$OUT"

if [[ ! -f "$CORE" ]]; then
  echo "downloading Gong libretro core: $CORE_URL"
  curl --fail --location --retry 3 --output "$CORE_ZIP" "$CORE_URL"
  unzip -o -q "$CORE_ZIP" -d "$CORE_DIR"
fi

if [[ ! -f "$CORE" ]]; then
  echo "download did not produce expected core: $CORE" >&2
  exit 1
fi

rm -rf "$OUT"
mkdir -p "$OUT"

cargo run -p headless-runner -- \
  --core "$CORE" \
  --frames 30 \
  --input idle \
  --out "$OUT/idle" \
  --skip-determinism \
  --skip-state-roundtrip

cargo run -p headless-runner -- \
  --core "$CORE" \
  --frames 30 \
  --input gameplay \
  --out "$OUT/gameplay-input" \
  --skip-determinism \
  --skip-state-roundtrip

IDLE_FINAL="$OUT/idle/run1_frame_0029.png"
GAMEPLAY_FINAL="$OUT/gameplay-input/run1_frame_0029.png"
if [[ ! -f "$IDLE_FINAL" || ! -f "$GAMEPLAY_FINAL" ]]; then
  echo "headless runner did not write expected final frames" >&2
  echo "  missing? $IDLE_FINAL" >&2
  echo "  missing? $GAMEPLAY_FINAL" >&2
  exit 1
fi

if cmp -s "$IDLE_FINAL" "$GAMEPLAY_FINAL"; then
  echo "gameplay input did not change Gong's final frame versus idle input" >&2
  echo "  idle:     $IDLE_FINAL" >&2
  echo "  gameplay: $GAMEPLAY_FINAL" >&2
  exit 1
fi

cargo run -p headless-runner -- \
  --core "$CORE" \
  --frames 30 \
  --input gameplay \
  --out "$OUT/gameplay-determinism"

RETROFEEL_CORE="$CORE" cargo test -p libretro-host --test user_smoke -- \
  --ignored \
  --nocapture \
  --test-threads=1

echo "real-core smoke complete: $CORE"
