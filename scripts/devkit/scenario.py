"""Version 1 timed controller scenarios use explicit evdev codes and raw ranges."""
import math


def validate(scenario):
    if scenario.get("schema_version") != 1:
        raise ValueError("unsupported scenario version")
    if scenario.get("route") not in ("direct", "steam"):
        raise ValueError("route must be direct or steam")
    if scenario.get("mode") != "diagnostic":
        raise ValueError("v1 is diagnostic; performance acceptance requires a game-specific trial adapter")
    controller = scenario["controller"]
    if not controller.get("name", "").startswith("RetroFeel test "):
        raise ValueError("synthetic device must use a RetroFeel test name")
    for key in ("vendor", "product"):
        if not isinstance(controller[key], int) or not 0 <= controller[key] <= 65535:
            raise ValueError("invalid controller identity")
    for code, bounds in controller["axes"].items():
        if not str(code).isdigit() or not 0 <= int(code) <= 63 or len(bounds) != 3:
            raise ValueError("invalid axis capability")
        low, high, neutral = bounds
        if any(type(value) is not int or not -(2**31) <= value < 2**31 for value in bounds) or not low <= neutral <= high or low == high:
            raise ValueError("invalid axis range/neutral")
    if any(type(code) is not int or not (0x120 <= code <= 0x15f or 0x220 <= code <= 0x223 or 0x2c0 <= code <= 0x2ff) for code in controller["buttons"]):
        raise ValueError("only explicit gamepad button capabilities are allowed")
    deadline = scenario["deadline_ms"]
    tolerance = scenario["timing_tolerance_ms"]
    if not 0 < deadline <= 300000 or not 0 < tolerance <= 1000:
        raise ValueError("invalid deadline/timing tolerance")
    previous = -1
    connected = True
    if not scenario.get("events"):
        raise ValueError("scenario must contain actions")
    for event in scenario["events"]:
        at = event["at_ms"]
        if not isinstance(at, (float, int)) or not math.isfinite(at) or not previous <= at <= deadline or at < 0 or not event.get("phase"):
            raise ValueError("events must be ordered within deadline with named phases")
        previous = at
        action = event.get("action", "state")
        if action in ("connect", "disconnect"):
            if (action == "connect") == connected:
                raise ValueError("invalid connection lifecycle")
            connected = action == "connect"
            continue
        if action != "state" or not connected:
            raise ValueError("cannot dispatch state while disconnected")
        for code, value in event.get("axes", {}).items():
            if code not in controller["axes"] or type(value) is not int or not controller["axes"][code][0] <= value <= controller["axes"][code][1]:
                raise ValueError("axis is undeclared or out of range")
        for code, value in event.get("buttons", {}).items():
            if int(code) not in controller["buttons"] or type(value) is not int or value not in (0, 1):
                raise ValueError("button is undeclared or invalid")
    if not scenario.get("assertions"):
        raise ValueError("observable outcome assertions are required")
    for assertion in scenario["assertions"]:
        if not assertion.get("match") or type(assertion.get("min_count")) is not int or assertion["min_count"] <= 0:
            raise ValueError("invalid observable assertion")
    return scenario
