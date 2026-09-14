use std::collections::BTreeSet;
use std::env;
use std::fs;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct BevyExport {
    metadata: Metadata,
    timeline: Vec<Frame>,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    fps: f64,
    frame_count: u64,
    export_y_axis: String,
}

#[derive(Debug, Deserialize)]
struct Frame {
    frame: u64,
    seconds: f64,
    pressed: Vec<String>,
    left_stick: [f32; 2],
    right_stick: [f32; 2],
    triggers: [f32; 2],
}

#[derive(Debug, Default)]
struct ReplayState {
    pressed: BTreeSet<String>,
    left_stick: [f32; 2],
    right_stick: [f32; 2],
    triggers: [f32; 2],
}

impl ReplayState {
    fn apply(&mut self, frame: &Frame) {
        self.pressed.clear();
        self.pressed.extend(frame.pressed.iter().cloned());
        self.left_stick = frame.left_stick;
        self.right_stick = frame.right_stick;
        self.triggers = frame.triggers;
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args()
        .nth(1)
        .ok_or("usage: cargo run -p retrofeel-export --example bevy_replay -- <bevy.ron>")?;
    let text = fs::read_to_string(path)?;
    let export: BevyExport = ron::from_str(&text)?;
    let mut state = ReplayState::default();
    let last_frame = export.timeline.len().saturating_sub(1);

    for (index, frame) in export.timeline.iter().enumerate() {
        state.apply(frame);
        if index < 3 || index == last_frame {
            println!(
                "frame={} seconds={:.6} pressed={:?} left_stick={:?}",
                frame.frame, frame.seconds, state.pressed, state.left_stick
            );
        }
    }

    println!(
        "replayed {} frames at {:.6} fps ({})",
        export.metadata.frame_count, export.metadata.fps, export.metadata.export_y_axis
    );
    Ok(())
}
