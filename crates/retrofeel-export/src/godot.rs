use serde::Serialize;

use crate::{
    export_metadata, normalized_frames_for, serialize_json, ExportError, ExportMetadata,
    LoadedSession, YAxisConvention,
};
use retrofeel_types::{MouseState, RawHostInput};

#[derive(Debug, PartialEq, Serialize)]
struct GodotExport {
    engine: &'static str,
    metadata: ExportMetadata,
    input_events: Vec<GodotFrame>,
}

#[derive(Debug, PartialEq, Serialize)]
struct GodotFrame {
    frame: u64,
    time_sec: f64,
    actions: Vec<&'static str>,
    joypad_motion: Vec<GodotAxis>,
    mouse: MouseState,
    keyboard: Vec<i32>,
    raw_host: Option<RawHostInput>,
}

#[derive(Debug, PartialEq, Serialize)]
struct GodotAxis {
    axis: &'static str,
    value: f32,
}

pub fn emit(session: &LoadedSession) -> Result<String, ExportError> {
    serialize_json(&build(session))
}

fn build(session: &LoadedSession) -> GodotExport {
    let input_events = normalized_frames_for(session, YAxisConvention::PositiveDown)
        .into_iter()
        .map(|frame| GodotFrame {
            frame: frame.frame,
            time_sec: frame.seconds,
            actions: frame.buttons,
            joypad_motion: vec![
                GodotAxis {
                    axis: "left_x",
                    value: frame.left_stick[0],
                },
                GodotAxis {
                    axis: "left_y",
                    value: frame.left_stick[1],
                },
                GodotAxis {
                    axis: "right_x",
                    value: frame.right_stick[0],
                },
                GodotAxis {
                    axis: "right_y",
                    value: frame.right_stick[1],
                },
            ],
            mouse: frame.mouse,
            keyboard: frame.keyboard,
            raw_host: frame.raw_host,
        })
        .collect();
    GodotExport {
        engine: "godot",
        metadata: export_metadata(session, YAxisConvention::PositiveDown),
        input_events,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{normalize_axis, tests::fixture_session, YAxisConvention};

    #[test]
    fn emits_godot_json_golden() {
        let session = fixture_session();
        let export = super::build(&session);
        assert_eq!(export.engine, "godot");
        assert_eq!(
            export.metadata.export_y_axis,
            YAxisConvention::PositiveDown.label()
        );
        assert_eq!(export.input_events[0].actions, vec!["Right", "A"]);
        assert_eq!(export.input_events[0].joypad_motion[0].axis, "left_x");
        assert_eq!(
            export.input_events[0].joypad_motion[1].value,
            normalize_axis(-0x4000)
        );

        let value: serde_json::Value =
            serde_json::from_str(&super::emit(&session).unwrap()).unwrap();
        assert_eq!(value["engine"], json!("godot"));
        assert_eq!(value["metadata"]["export_y_axis"], json!("positive-down"));
        assert_eq!(value["input_events"][0]["actions"], json!(["Right", "A"]));
    }
}
