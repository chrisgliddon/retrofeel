//! Headless libretro runner.
//!
//! Loads a core, runs N frames with scripted input, dumps the first/last frame
//! as PNG and any audio as WAV. Used to verify the host crate end-to-end
//! without a GUI. Exit criteria per Phase 1: real no-content cores run headless
//! producing correct PNGs; identical scripted input ⇒ byte-identical framebuffers
//! (determinism); save/load state round-trips.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{ArgAction, Parser, ValueEnum};
use libretro_host::{Core, Frame};
use retrofeel_types::device_ids::joypad;
use retrofeel_types::{InputState, RetroPadButtonBits};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum InputScript {
    /// No buttons pressed — pure determinism check.
    Idle,
    /// Press START on frame 10, A on frame 20 — exercises input state.
    Scripted,
    /// D-Pad Up on frames 5-14, Down on 15-24, A on 25-29.
    Gameplay,
}

#[derive(Parser, Debug)]
#[command(name = "headless-runner", version, about = "Headless libretro runner")]
struct Args {
    /// Path to the libretro core (.so/.dll/.dylib).
    #[arg(long)]
    core: PathBuf,

    /// Path to a ROM to load (optional for no-content cores).
    #[arg(long)]
    rom: Option<PathBuf>,

    /// Number of frames to run.
    #[arg(long, default_value_t = 60)]
    frames: u32,

    /// Input script.
    #[arg(long, value_enum, default_value_t = InputScript::Scripted)]
    input: InputScript,

    /// Directory to write outputs to.
    #[arg(long, default_value = "out")]
    out: PathBuf,

    /// System directory (BIOS etc.).
    #[arg(long, default_value = "system")]
    system: String,

    /// Skip the determinism harness.
    #[arg(long = "skip-determinism", action = ArgAction::SetFalse, default_value_t = true)]
    determinism: bool,

    /// Skip the save-state round-trip harness.
    #[arg(long = "skip-state-roundtrip", action = ArgAction::SetFalse, default_value_t = true)]
    state_roundtrip: bool,
}

fn scripted_input(frame: u32, script: InputScript) -> InputState {
    let mut buttons = RetroPadButtonBits::EMPTY;
    match script {
        InputScript::Idle => {}
        InputScript::Scripted => {
            if frame == 10 {
                buttons.set(joypad::START);
            }
            if frame == 20 {
                buttons.set(joypad::A);
            }
        }
        InputScript::Gameplay => {
            if (5..=14).contains(&frame) {
                buttons.set(joypad::UP);
            }
            if (15..=24).contains(&frame) {
                buttons.set(joypad::DOWN);
            }
            if (25..=29).contains(&frame) {
                buttons.set(joypad::A);
            }
        }
    }
    InputState {
        buttons,
        ..Default::default()
    }
}

fn run_once(
    core_path: &PathBuf,
    rom: &Option<PathBuf>,
    frames: u32,
    script: InputScript,
    system: &str,
    out: &PathBuf,
    tag: &str,
) -> Result<Vec<u8>> {
    let mut core = Core::load(core_path, system)
        .with_context(|| format!("loading core {}", core_path.display()))?;

    let rom_bytes = match rom {
        Some(p) => std::fs::read(p).with_context(|| format!("reading rom {}", p.display()))?,
        None => Vec::new(),
    };
    core.load_game(&rom_bytes, rom.as_ref().map(|p| p.to_str().unwrap()))
        .context("loading game")?;

    std::fs::create_dir_all(out).ok();

    let mut last_rgba: Vec<u8> = Vec::new();
    let mut all_audio: Vec<i16> = Vec::new();

    for f in 0..frames {
        let input = scripted_input(f, script);
        let out_run = core.run_frame(input).context("running frame")?;
        if let Some(frame) = &out_run.frame {
            last_rgba = frame.rgba.clone();
            if f == 0 || f == frames - 1 {
                let path = out.join(format!("{tag}_frame_{f:04}.png"));
                image::save_buffer(
                    &path,
                    &frame.rgba,
                    frame.width,
                    frame.height,
                    image::ColorType::Rgba8,
                )
                .with_context(|| format!("saving {}", path.display()))?;
            }
        }
        all_audio.extend(out_run.audio);
    }

    if !all_audio.is_empty() {
        let wav_path = out.join(format!("{tag}_audio.wav"));
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: core.av_info().sample_rate as u32,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&wav_path, spec)
            .with_context(|| format!("creating {}", wav_path.display()))?;
        for s in &all_audio {
            writer.write_sample(*s).context("writing audio sample")?;
        }
        writer.finalize().context("finalizing wav")?;
    }

    Ok(last_rgba)
}

fn save_frame_png(path: &Path, frame: &Frame) -> Result<()> {
    image::save_buffer(
        path,
        &frame.rgba,
        frame.width,
        frame.height,
        image::ColorType::Rgba8,
    )
    .with_context(|| format!("saving {}", path.display()))
}

fn first_differing_byte(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter()
        .zip(b)
        .position(|(left, right)| left != right)
        .or_else(|| (a.len() != b.len()).then_some(a.len().min(b.len())))
}

fn describe_frame_difference(first: &Frame, second: &Frame) -> String {
    let mut parts = Vec::new();
    if first.width != second.width || first.height != second.height {
        parts.push(format!(
            "dimensions differ: pass 1 {}x{}, pass 2 {}x{}",
            first.width, first.height, second.width, second.height
        ));
    }
    if first.rgba.len() != second.rgba.len() {
        parts.push(format!(
            "byte lengths differ: pass 1 {}, pass 2 {}",
            first.rgba.len(),
            second.rgba.len()
        ));
    }
    if let Some(byte_index) = first_differing_byte(&first.rgba, &second.rgba) {
        let pixel_index = byte_index / 4;
        let channel = match byte_index % 4 {
            0 => "R",
            1 => "G",
            2 => "B",
            _ => "A",
        };
        let width = first.width.max(1);
        let x = pixel_index as u32 % width;
        let y = (pixel_index as u32).checked_div(first.width).unwrap_or(0);
        let left = first.rgba.get(byte_index);
        let right = second.rgba.get(byte_index);
        parts.push(format!(
            "first differing byte {byte_index} (pixel {pixel_index}, x={x}, y={y}, channel {channel}): pass 1 {left:?}, pass 2 {right:?}"
        ));
    }
    if parts.is_empty() {
        "frames differ but no differing byte was found".to_string()
    } else {
        parts.join("; ")
    }
}

fn main() -> ExitCode {
    env_logger::init();
    let args = Args::parse();

    match run(&args) {
        Ok(()) => {
            println!("headless-runner: OK — outputs in {}", args.out.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("headless-runner: ERROR: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<()> {
    // First pass: produce PNG/WAV outputs.
    let _rgba_a = run_once(
        &args.core,
        &args.rom,
        args.frames,
        args.input,
        &args.system,
        &args.out,
        "run1",
    )?;

    // Determinism: reset the core and re-run the same script; the final
    // framebuffer must be byte-identical. We use a single core instance with
    // reset because libretro cores share TLS across dlopen handles on some
    // platforms (macOS), so two separate Core instances would share state.
    if args.determinism {
        let mut core = Core::load(&args.core, &args.system)
            .with_context(|| format!("loading core {}", args.core.display()))?;
        let rom_bytes = match &args.rom {
            Some(p) => std::fs::read(p)?,
            None => Vec::new(),
        };
        core.load_game(&rom_bytes, args.rom.as_ref().map(|p| p.to_str().unwrap()))?;
        let mut first: Option<Frame> = None;
        for f in 0..args.frames {
            first = Some(
                core.run_frame_required(scripted_input(f, args.input))
                    .context("run frame (pass 1)")?,
            );
        }
        core.reset().context("reset")?;
        let mut second: Option<Frame> = None;
        for f in 0..args.frames {
            second = Some(
                core.run_frame_required(scripted_input(f, args.input))
                    .context("run frame (pass 2)")?,
            );
        }
        let first = first.context("determinism pass 1 produced no final frame")?;
        let second = second.context("determinism pass 2 produced no final frame")?;
        if first.width != second.width || first.height != second.height || first.rgba != second.rgba
        {
            std::fs::create_dir_all(&args.out)
                .with_context(|| format!("creating {}", args.out.display()))?;
            let pass1_path = args.out.join("determinism_pass1_final.png");
            let pass2_path = args.out.join("determinism_pass2_final.png");
            save_frame_png(&pass1_path, &first)?;
            save_frame_png(&pass2_path, &second)?;
            anyhow::bail!(
                "determinism failed: {}; final frames saved to {} and {}",
                describe_frame_difference(&first, &second),
                pass1_path.display(),
                pass2_path.display()
            );
        }
        println!("determinism: PASS (byte-identical final framebuffers)");
    }

    // Save/load state round-trip: run N frames, save state, run one more
    // scripted frame (A); restore state, run the same scripted frame (B);
    // A == B.
    if args.state_roundtrip {
        let mut core = Core::load(&args.core, &args.system)?;
        let rom_bytes = match &args.rom {
            Some(p) => std::fs::read(p)?,
            None => Vec::new(),
        };
        core.load_game(&rom_bytes, args.rom.as_ref().map(|p| p.to_str().unwrap()))?;
        for f in 0..args.frames {
            core.run_frame(scripted_input(f, args.input))?;
        }
        let state = core.serialize().context("serialize")?;
        let next_input = scripted_input(args.frames, args.input);
        let after_a = core.run_frame_required(next_input)?;
        core.unserialize(&state).context("unserialize")?;
        let after_b = core.run_frame_required(next_input)?;
        if after_a.rgba != after_b.rgba {
            anyhow::bail!("state round-trip failed: post-state frame mismatch");
        }
        std::fs::write(args.out.join("state_roundtrip.bin"), &state)?;
        println!("state round-trip: PASS ({} bytes)", state.len());
    }

    Ok(())
}
