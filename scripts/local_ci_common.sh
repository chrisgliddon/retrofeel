#!/usr/bin/env bash

require_command() {
  local command_name=$1
  local hint=$2
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "$hint" >&2
    exit 1
  fi
}

preflight_linux_build_deps() {
  [[ "$(uname -s)" == "Linux" ]] || return 0

  require_command pkg-config \
    "pkg-config is required for local QA. On Debian/Ubuntu: sudo apt-get install pkg-config"

  local missing=()
  for pkg in alsa libudev wayland-client xkbcommon; do
    if ! pkg-config --exists "$pkg"; then
      missing+=("$pkg")
    fi
  done

  if [[ ${#missing[@]} -gt 0 ]]; then
    echo "missing Linux development packages for local QA: ${missing[*]}" >&2
    echo "On Debian/Ubuntu, install:" >&2
    echo "  sudo apt-get install pkg-config libasound2-dev libudev-dev libwayland-dev libxkbcommon-dev" >&2
    exit 1
  fi
}

preflight_linux_x11_runtime() {
  [[ "$(uname -s)" == "Linux" ]] || return 0
  [[ -n "${DISPLAY:-}" ]] || return 0

  if command -v ldconfig >/dev/null 2>&1 && \
    ldconfig -p 2>/dev/null | grep -q 'libxkbcommon-x11\.so\.0'; then
    return 0
  fi

  local libdir
  for libdir in \
    /lib \
    /usr/lib \
    /lib64 \
    /usr/lib64 \
    /lib/x86_64-linux-gnu \
    /usr/lib/x86_64-linux-gnu \
    /lib/aarch64-linux-gnu \
    /usr/lib/aarch64-linux-gnu; do
    if [[ -e "$libdir/libxkbcommon-x11.so.0" ]]; then
      return 0
    fi
  done

  echo "missing Linux/X11 runtime library: libxkbcommon-x11.so.0" >&2
  echo "On Debian/Ubuntu, install:" >&2
  echo "  sudo apt-get install libxkbcommon-x11-0" >&2
  exit 1
}

preflight_linux_desktop_display() {
  [[ "$(uname -s)" == "Linux" ]] || return 0
  if [[ -z "${DISPLAY:-}" && -z "${WAYLAND_DISPLAY:-}" ]]; then
    echo "local app QA requires an X11 or Wayland display." >&2
    echo "Run from a desktop session, or use xvfb-run for X11 automation." >&2
    exit 1
  fi
}
