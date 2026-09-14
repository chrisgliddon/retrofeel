# Development

RetroFeel is a Rust workspace using Bevy 0.18 for its desktop GUI. Shared recording
contracts live in `retrofeel-types`; the libretro host, backend, database, exporters,
analysis packages, and platform capture adapters are separate crates. The Linux
Deck recorder is a separate application. The `.feel` package format is version 1;
analysis results use schema version 2. Capture callbacks provide the recording clock.

Use a machine with enough memory and disk for Rust/Bevy builds. Check free space
before compiling. Do not delete recordings, ROMs, save states, or credentials to
make space. Linux requires compiler tooling and ALSA, udev, X11/Wayland libraries;
macOS Steam capture additionally requires the platform permissions documented in
[Steam capture](STEAM_CAPTURE.md).

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --locked -p retrofeel -p mock-core
python3 -m unittest discover -s scripts/tests
python3 -m unittest discover -s skills/review-deck-feedback/tests
python3 -m unittest discover -s skills/review-retrofeel-feedback/tests
```

Python media tests require NumPy, OpenCV, and ffmpeg/ffprobe. Hardware-dependent
capture checks are separate from unit tests and must run on the appropriate
platform with explicit device access. Use the mock core for deterministic GUI
fixtures; never distribute test ROMs or third-party game captures accidentally.

The website and its checks are documented in [site/README.md](../site/README.md).
No automated release publisher or GitHub Actions workflow is included.

The candidate is verified with Rust 1.96.0 on Ubuntu 24.04. Use `--locked`:
`sherpa-onnx` and `sherpa-onnx-sys` must remain on matching 1.13.4 versions.
The native transcription libraries require a recent C++ runtime. Linux build
dependencies also include `liblzma-dev` for the locked compression dependency.

Use pnpm exclusively for JavaScript tooling on local machines and Ubuntu Servers.
Use the version pinned in `site/package.json`, install with
`pnpm install --frozen-lockfile`, and use `pnpm run` / `pnpm exec` for commands.
Do not add npm, Yarn, or Bun lockfiles or bypass the package manager checks.
See [website tooling](../site/README.md) for the dependency policy.
