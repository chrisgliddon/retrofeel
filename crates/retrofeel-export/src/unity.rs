use serde::Serialize;

use crate::{
    export_metadata, normalized_frames_for, serialize_json, ExportError, ExportMetadata,
    LoadedSession, YAxisConvention,
};
use retrofeel_types::{MouseState, RawHostInput};

#[derive(Debug, PartialEq, Serialize)]
struct UnityExport {
    engine: &'static str,
    metadata: ExportMetadata,
    events: Vec<UnityFrame>,
}

#[derive(Debug, PartialEq, Serialize)]
struct UnityFrame {
    frame: u64,
    time: f64,
    gamepad: u8,
    buttons: Vec<&'static str>,
    left_stick: [f32; 2],
    right_stick: [f32; 2],
    left_trigger: f32,
    right_trigger: f32,
    mouse: MouseState,
    keyboard: Vec<i32>,
    raw_host: Option<RawHostInput>,
}

pub fn emit(session: &LoadedSession) -> Result<String, ExportError> {
    serialize_json(&build(session))
}

fn build(session: &LoadedSession) -> UnityExport {
    let events = normalized_frames_for(session, YAxisConvention::PositiveUp)
        .into_iter()
        .map(|frame| UnityFrame {
            frame: frame.frame,
            time: frame.seconds,
            gamepad: frame.port,
            buttons: frame.buttons,
            left_stick: frame.left_stick,
            right_stick: frame.right_stick,
            left_trigger: frame.triggers[0],
            right_trigger: frame.triggers[1],
            mouse: frame.mouse,
            keyboard: frame.keyboard,
            raw_host: frame.raw_host,
        })
        .collect();
    UnityExport {
        engine: "unity-input-system",
        metadata: export_metadata(session, YAxisConvention::PositiveUp),
        events,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{normalize_axis, tests::fixture_session, YAxisConvention};

    #[test]
    fn emits_unity_json_golden() {
        let session = fixture_session();
        let export = super::build(&session);
        assert_eq!(export.engine, "unity-input-system");
        assert_eq!(export.metadata.fps, 60.0);
        assert_eq!(
            export.metadata.export_y_axis,
            YAxisConvention::PositiveUp.label()
        );
        assert_eq!(export.events[0].buttons, vec!["Right", "A"]);
        assert_eq!(
            export.events[0].left_stick,
            [normalize_axis(0x4000), -normalize_axis(-0x4000)]
        );

        let value: serde_json::Value =
            serde_json::from_str(&super::emit(&session).unwrap()).unwrap();
        assert_eq!(value["engine"], json!("unity-input-system"));
        assert_eq!(value["metadata"]["fps"], json!(60.0));
        assert_eq!(value["events"][0]["buttons"], json!(["Right", "A"]));
    }
}
