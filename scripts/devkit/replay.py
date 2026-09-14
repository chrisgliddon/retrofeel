"""Convert one explicitly selected canonical device track to a clocked scenario."""
import hashlib
import json
from pathlib import Path

AXES = {"LeftStickX": "0", "LeftStickY": "1", "RightStickX": "3", "RightStickY": "4", "LeftTrigger": "2", "RightTrigger": "5"}
BUTTONS = {"South": 304, "East": 305, "West": 307, "North": 308, "LeftShoulder": 310,
           "RightShoulder": 311, "Select": 314, "Start": 315, "LeftThumb": 317, "RightThumb": 318}


def from_recording(directory, game_id, device_id, template, deadzone=0.15):
    if not 0 <= deadzone < 1:
        raise ValueError("deadzone must be in [0, 1)")
    directory = Path(directory)
    manifest = json.loads((directory / "manifest.json").read_text())
    external = manifest.get("external_capture") or {}
    if str(external.get("game_id")) != game_id:
        raise ValueError("recording game ID mismatch")
    if external.get("status") != "complete" or not (directory / "archive-clock.json").is_file():
        raise ValueError("recorded replay requires a verified canonical archive clock")
    input_bytes = (directory / "input.json").read_bytes()
    frames = json.loads(input_bytes)
    clock = json.loads((directory / "archive-clock.json").read_text())
    media = json.loads((directory / "media-validation.json").read_text())
    if (clock.get("schema_version") != 1 or clock.get("recording_id") != external.get("recording_id")
            or clock.get("method") != "whole_segment_zero_origin"
            or clock.get("first_frame_boottime_us") != (external.get("video_clock") or {}).get("video_pts_zero_boottime_us")):
        raise ValueError("archive clock receipt does not match recording")
    pts = media.get("frame_pts_us") or []
    if (not frames or not pts or manifest.get("frame_count") != len(frames) or media.get("packet_count") != len(frames)
            or [frame.get("elapsed_us") for frame in frames] != [value - pts[0] for value in pts]):
        raise ValueError("canonical input and validated media timelines disagree")
    scenario = json.loads(json.dumps(template))
    scenario["events"] = []
    previous = None
    seen = False
    connected = True
    for frame in frames:
        if "elapsed_us" not in frame:
            raise ValueError("recorded replay requires per-frame elapsed_us")
        at = frame["elapsed_us"] / 1000
        pads = [pad for pad in (frame.get("raw_host") or {}).get("gamepads", []) if pad.get("device_id") == device_id]
        if len(pads) > 1:
            raise ValueError("ambiguous duplicate device ID")
        if not pads:
            if connected:
                scenario["events"].append({"at_ms": at, "phase": "device_absent", "action": "disconnect"})
                connected = False
            previous = None
            continue
        seen = True
        if not connected:
            scenario["events"].append({"at_ms": at, "phase": "device_returned", "action": "connect"})
            connected = True
        pad = pads[0]
        unknown = set(pad.get("buttons", [])) - set(BUTTONS)
        if unknown:
            raise ValueError(f"recorded buttons require an explicit adapter: {sorted(unknown)}")
        axes = {}
        for name, value in pad.get("axes", {}).items():
            if name not in AXES:
                raise ValueError(f"recorded axis requires an explicit adapter: {name}")
            code = AXES[name]
            low, high, neutral = scenario["controller"]["axes"][code]
            if not -1 <= value <= 1:
                raise ValueError("recorded axis is outside normalized range")
            # RetroFeel canonical Y is positive-up; Linux stick Y is positive-down.
            if name.endswith("StickY"):
                value = -value
            value = 0 if abs(value) <= deadzone else (abs(value) - deadzone) / (1 - deadzone) * (1 if value > 0 else -1)
            axes[code] = round(neutral + value * (high - neutral if value >= 0 else neutral - low))
        pressed = {BUTTONS[name] for name in pad.get("buttons", [])}
        buttons = {str(code): int(code in pressed) for code in scenario["controller"]["buttons"]}
        state = (axes, buttons)
        if state != previous:
            scenario["events"].append({"at_ms": at, "phase": f"frame_{frame['frame']}", "axes": axes, "buttons": buttons})
        previous = state
    if not seen:
        raise ValueError("selected device was never present")
    scenario["deadline_ms"] = frames[-1]["elapsed_us"] / 1000 + 100
    scenario["recorded_source"] = {"game_id": game_id, "recording_id": external["recording_id"],
        "device_id": device_id, "input_sha256": hashlib.sha256(input_bytes).hexdigest(), "deadzone": deadzone}
    # Retain adapter outcome assertions; input delivery alone is not game acceptance.
    from .scenario import validate
    return validate(scenario)
