# Engine Export Guide

Phase 6 exports recorded sessions from `recordings/session-*` into engine-shaped
input traces.

```sh
retrofeel export --engine bevy --session recordings/session-123
retrofeel export --engine unity --session recordings/session-123
retrofeel export --engine godot --session recordings/session-123
retrofeel export --engine unreal --session recordings/session-123
```

By default, exports are written to `<session>/exports/`:

- `bevy.ron`
- `unity.json`
- `godot.json`
- `unreal.json`

Every export includes:

- `fps`: the exact core-reported frame rate from the recording manifest;
- `frame_count`: the captured input-frame count from the manifest;
- `source_y_axis`: `positive-down`, matching libretro analog stick Y;
- `export_y_axis`: the convention used in that file;
- `binding_map`: the host input bindings active during capture, if present;
- per-frame `frame`, seconds/time, port, normalized buttons, sticks, triggers,
  mapped keyboard/mouse state, and optional raw host input.

Analog values are normalized from libretro's `[-0x7fff, 0x7fff]` range to
`[-1.0, 1.0]`. Triggers are clamped to `[0.0, 1.0]`.

## Bevy

`bevy.ron` is shaped as a raw timeline that can be converted into
`leafwing-input-manager` action state updates.

Y axis: `positive-up`.

The repository includes a tiny parser/replayer example:

```sh
cargo run -p retrofeel-export --example bevy_replay -- recordings/session-123/exports/bevy.ron
```

```rust
use leafwing_input_manager::prelude::*;
use serde::Deserialize;

#[derive(Actionlike, Clone, Copy, Debug, Eq, PartialEq, Hash, Reflect)]
enum Action {
    A,
    Right,
    LeftStick,
}

#[derive(Deserialize)]
struct Export {
    metadata: Metadata,
    timeline: Vec<Frame>,
}

#[derive(Deserialize)]
struct Metadata {
    fps: f64,
}

#[derive(Deserialize)]
struct Frame {
    frame: u64,
    pressed: Vec<String>,
    left_stick: [f32; 2],
}

fn apply_frame(frame: &Frame, actions: &mut ActionState<Action>) {
    for action in [Action::A, Action::Right] {
        actions.release(&action);
    }
    for name in &frame.pressed {
        match name.as_str() {
            "A" => actions.press(&Action::A),
            "Right" => actions.press(&Action::Right),
            _ => {}
        }
    }
    actions.set_axis_pair(&Action::LeftStick, frame.left_stick.into());
}
```

## Unity

`unity.json` is a frame-indexed trace for a `MonoBehaviour` replayer. The JSON
keeps RetroPad button names and normalized stick/trigger values so projects can
map them to their own `InputAction`s.

Y axis: `positive-up`.

```csharp
using System;
using System.Collections.Generic;
using UnityEngine;

[Serializable] public sealed class RetrofeelExport {
    public Metadata metadata;
    public List<InputFrame> events;
}

[Serializable] public sealed class Metadata {
    public double fps;
}

[Serializable] public sealed class InputFrame {
    public ulong frame;
    public double time;
    public string[] buttons;
    public float[] left_stick;
    public float left_trigger;
    public float right_trigger;
}

public sealed class RetrofeelReplayer : MonoBehaviour {
    public TextAsset exportJson;
    RetrofeelExport trace;
    int cursor;

    void Awake() {
        trace = JsonUtility.FromJson<RetrofeelExport>(exportJson.text);
    }

    void FixedUpdate() {
        if (cursor >= trace.events.Count) return;
        var frame = trace.events[cursor++];
        foreach (var button in frame.buttons) {
            Debug.Log($"retrofeel button {button} at frame {frame.frame}");
        }
    }
}
```

## Godot

`godot.json` uses `actions` and `joypad_motion` records that mirror Godot's
action and joypad-motion concepts.

Y axis: `positive-down`.

```gdscript
extends Node

var trace = []
var cursor := 0

func _ready() -> void:
    var file := FileAccess.open("res://godot.json", FileAccess.READ)
    trace = JSON.parse_string(file.get_as_text()).input_events

func _physics_process(_delta: float) -> void:
    if cursor >= trace.size():
        return
    var frame = trace[cursor]
    cursor += 1
    for action_name in frame.actions:
        Input.action_press(action_name)
    for motion in frame.joypad_motion:
        print("%s = %s" % [motion.axis, motion.value])
```

## Unreal

`unreal.json` emits Enhanced Input-shaped frames: each frame has an `actions`
array with boolean buttons, 2D stick values, and 1D trigger values.

Y axis: `positive-up`.

```cpp
struct FRetrofeelAction
{
    FString Name;
    TSharedPtr<FJsonValue> Value;
};

struct FRetrofeelFrame
{
    int64 Frame = 0;
    double TimeSeconds = 0.0;
    TArray<FRetrofeelAction> Actions;
};

void ApplyRetrofeelFrame(const FRetrofeelFrame& Frame)
{
    for (const FRetrofeelAction& Action : Frame.Actions)
    {
        UE_LOG(LogTemp, Verbose, TEXT("retrofeel %s at frame %lld"),
            *Action.Name, Frame.Frame);
        // Route Action.Name and Action.Value into project-specific
        // Enhanced Input handlers or pawn movement code here.
    }
}
```
