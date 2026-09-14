#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
ssh_target=${RETROFEEL_DECK_SSH:-deck}

whisper_version=1.9.2
runtime_archive=whisper-bin-ubuntu-x64.tar.gz
runtime_url="https://github.com/ggml-org/whisper.cpp/releases/download/v${whisper_version}/${runtime_archive}"
runtime_sha256=46811a3ecf584307480a220b9ef5ff81b7b22dc41577cbc274ce3afc61f753b1
model_url=https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin
model_sha1=137c40403d78fd54d454da0f9bd998f78703390c
wrapper="$repo_root/apps/retrofeel-deck-recorder/packaging/retrofeel-whisper-cli"

usage() {
  printf '%s\n' \
    "Usage: $0 [--target SSH_HOST]" \
    "" \
    "Installs whisper.cpp v${whisper_version} and the 142 MiB base.en model" \
    "into the Deck user's home directory. SteamOS itself is not modified."
}

while (($#)); do
  case "$1" in
    --target)
      ssh_target=$2
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

if [[ ! -x "$wrapper" ]]; then
  printf 'Whisper wrapper is missing or not executable: %s\n' "$wrapper" >&2
  exit 1
fi
if [[ $(ssh "$ssh_target" 'id -un') != deck ]]; then
  printf 'Refusing to install anywhere except the Deck user account.\n' >&2
  exit 1
fi

temporary=$(mktemp -d)
cleanup() {
  rm -rf -- "$temporary"
}
trap cleanup EXIT

archive="$temporary/$runtime_archive"
curl -fL --retry 3 -o "$archive" "$runtime_url"
printf '%s  %s\n' "$runtime_sha256" "$archive" | shasum -a 256 -c -

remote_archive="/tmp/retrofeel-${runtime_archive}-$$"
remote_wrapper="/tmp/retrofeel-whisper-cli-$$"
scp "$archive" "$ssh_target:$remote_archive"
scp "$wrapper" "$ssh_target:$remote_wrapper"

ssh "$ssh_target" "
  set -eu
  runtime_dir=\"\$HOME/.local/lib/retrofeel/whisper-cpp-v${whisper_version}\"
  model_dir=\"\$HOME/.local/share/retrofeel/transcription-models/whisper-base-en\"
  model=\"\$model_dir/ggml-base.en.bin\"
  mkdir -p \"\$runtime_dir\" \"\$model_dir\" \"\$HOME/.local/bin\"
  tar -xzf '$remote_archive' --strip-components=1 -C \"\$runtime_dir\"
  install -m 0755 '$remote_wrapper' \"\$HOME/.local/bin/retrofeel-whisper-cli\"
  rm -f '$remote_archive' '$remote_wrapper'
  if [ ! -f \"\$model\" ] || [ \"\$(sha1sum \"\$model\" | cut -d ' ' -f1)\" != '$model_sha1' ]; then
    curl -fL --retry 5 --retry-delay 2 -C - -o \"\$model.part\" '$model_url'
    test \"\$(sha1sum \"\$model.part\" | cut -d ' ' -f1)\" = '$model_sha1'
    mv \"\$model.part\" \"\$model\"
  fi
  \"\$HOME/.local/bin/retrofeel-whisper-cli\" --version
  printf 'model: '
  du -h \"\$model\"
"

printf '\nInstalled Deck transcription runtime and model on %s.\n' "$ssh_target"
