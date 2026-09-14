use serde::Serialize;

use crate::{
    export_metadata, normalized_frames_for, serialize_json, ExportError, ExportMetadata,
    LoadedSession, YAxisConvention,
};
use retrofeel_types::{MouseState, RawHostInput};

#[derive(Debug, PartialEq, Serialize)]
struct UnrealExport {
    engine: &'static str,
    metadata: ExportMetadata,
    enhanced_input_frames: Vec<UnrealFrame>,
}

#[derive(Debug, PartialEq, Serialize)]
struct UnrealFrame {
    frame: u64,
    time_seconds: f64,
    actions: Vec<UnrealAction>,
    mouse: MouseState,
    keyboard: Vec<i32>,
    raw_host: Option<RawHostInput>,
}

#[derive(Debug, PartialEq, Serialize)]
struct UnrealAction {
    name: &'static str,
    value: UnrealValue,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(untagged)]
enum UnrealValue {
    Bool(bool),
    Axis2d([f32; 2]),
    Axis1d(f32),
}

pub fn emit(session: &LoadedSession) -> Result<String, ExportError> {
    serialize_json(&build(session))
}

fn build(session: &LoadedSession) -> UnrealExport {
    let enhanced_input_frames = normalized_frames_for(session, YAxisConvention::PositiveUp)
        .into_iter()
        .map(|frame| {
            let mut actions: Vec<UnrealAction> = frame
                .buttons
                .into_iter()
                .map(|name| UnrealAction {
                    name,
                    value: UnrealValue::Bool(true),
                })
                .collect();
            actions.push(UnrealAction {
                name: "LeftStick",
                value: UnrealValue::Axis2d(frame.left_stick),
            });
            actions.push(UnrealAction {
                name: "RightStick",
                value: UnrealValue::Axis2d(frame.right_stick),
            });
            actions.push(UnrealAction {
                name: "LeftTrigger",
                value: UnrealValue::Axis1d(frame.triggers[0]),
            });
            actions.push(UnrealAction {
                name: "RightTrigger",
                value: UnrealValue::Axis1d(frame.triggers[1]),
            });
            UnrealFrame {
                frame: frame.frame,
                time_seconds: frame.seconds,
                actions,
                mouse: frame.mouse,
                keyboard: frame.keyboard,
                raw_host: frame.raw_host,
            }
        })
        .collect();
    UnrealExport {
        engine: "unreal-enhanced-input",
        metadata: export_metadata(session, YAxisConvention::PositiveUp),
        enhanced_input_frames,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{normalize_axis, tests::fixture_session, YAxisConvention};

    #[test]
    fn emits_unreal_json_golden() {
        let session = fixture_session();
        let export = super::build(&session);
        assert_eq!(export.engine, "unreal-enhanced-input");
        assert_eq!(
            export.metadata.export_y_axis,
            YAxisConvention::PositiveUp.label()
        );
        assert_eq!(export.enhanced_input_frames[0].actions[0].name, "Right");
        assert_eq!(export.enhanced_input_frames[0].actions[1].name, "A");
        assert_eq!(
            export.enhanced_input_frames[0].actions[2].value,
            super::UnrealValue::Axis2d([normalize_axis(0x4000), -normalize_axis(-0x4000)])
        );

        let value: serde_json::Value =
            serde_json::from_str(&super::emit(&session).unwrap()).unwrap();
        assert_eq!(value["engine"], json!("unreal-enhanced-input"));
        assert_eq!(value["metadata"]["export_y_axis"], json!("positive-up"));
        assert_eq!(
            value["enhanced_input_frames"][0]["actions"][1]["name"],
            json!("A")
        );
    }
}
