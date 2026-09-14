use serde::Serialize;

use crate::{
    export_metadata, normalized_frames_for, serialize_ron, ExportError, ExportMetadata,
    LoadedSession, YAxisConvention,
};
use retrofeel_types::{MouseState, RawHostInput};

#[derive(Debug, PartialEq, Serialize)]
struct BevyExport {
    engine: &'static str,
    metadata: ExportMetadata,
    timeline: Vec<BevyFrame>,
}

#[derive(Debug, PartialEq, Serialize)]
struct BevyFrame {
    frame: u64,
    seconds: f64,
    port: u8,
    pressed: Vec<&'static str>,
    left_stick: [f32; 2],
    right_stick: [f32; 2],
    triggers: [f32; 2],
    mouse: MouseState,
    keyboard: Vec<i32>,
    raw_host: Option<RawHostInput>,
}

pub fn emit(session: &LoadedSession) -> Result<String, ExportError> {
    serialize_ron(&build(session))
}

fn build(session: &LoadedSession) -> BevyExport {
    let timeline = normalized_frames_for(session, YAxisConvention::PositiveUp)
        .into_iter()
        .map(|frame| BevyFrame {
            frame: frame.frame,
            seconds: frame.seconds,
            port: frame.port,
            pressed: frame.buttons,
            left_stick: frame.left_stick,
            right_stick: frame.right_stick,
            triggers: frame.triggers,
            mouse: frame.mouse,
            keyboard: frame.keyboard,
            raw_host: frame.raw_host,
        })
        .collect();
    BevyExport {
        engine: "bevy",
        metadata: export_metadata(session, YAxisConvention::PositiveUp),
        timeline,
    }
}

#[cfg(test)]
mod tests {
    use crate::{normalize_axis, normalize_trigger, tests::fixture_session, YAxisConvention};

    #[test]
    fn emits_bevy_ron_golden() {
        let session = fixture_session();
        let export = super::build(&session);
        assert_eq!(export.engine, "bevy");
        assert_eq!(export.metadata.fps, 60.0);
        assert_eq!(
            export.metadata.export_y_axis,
            YAxisConvention::PositiveUp.label()
        );
        assert_eq!(export.timeline.len(), 2);
        assert_eq!(export.timeline[0].pressed, vec!["Right", "A"]);
        assert_eq!(
            export.timeline[0].left_stick,
            [normalize_axis(0x4000), -normalize_axis(-0x4000)]
        );
        assert_eq!(
            export.timeline[0].triggers,
            [normalize_trigger(0x2000), normalize_trigger(0x7fff)]
        );
        assert_eq!(
            export.timeline[0]
                .raw_host
                .as_ref()
                .unwrap()
                .gamepad_buttons,
            vec!["South"]
        );

        let output = super::emit(&session).unwrap();
        assert!(output.contains("engine: \"bevy\""));
        assert!(output.contains("export_y_axis: \"positive-up\""));
    }
}
