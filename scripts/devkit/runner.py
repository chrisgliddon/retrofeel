"""Explicit synthetic input. No Steam layout changes, focus forcing or foreign signals."""
import argparse
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

from .guard import Guard, identity
from .lease import Lease, atomic_json, status
from .scenario import validate


def boottime_us():
    return time.clock_gettime_ns(time.CLOCK_BOOTTIME) // 1000


class LinuxController:
    def __init__(self, config):
        self.config = config
        self.pad = None
        self.connect()

    def connect(self):
        from evdev import AbsInfo, UInput, ecodes
        config = self.config
        axes = [(int(code), AbsInfo(bounds[2], bounds[0], bounds[1], 0, 0, 0))
                for code, bounds in config["axes"].items()]
        self.pad = UInput({ecodes.EV_KEY: config["buttons"], ecodes.EV_ABS: axes},
            name=config["name"], vendor=config["vendor"], product=config["product"], version=1)

    def write(self, kind, code, value):
        self.pad.write(kind, code, value)
        self.pad.syn()

    def neutralize(self):
        if self.pad:
            for code, bounds in self.config["axes"].items():
                self.pad.write(3, int(code), bounds[2])
            for code in self.config["buttons"]:
                self.pad.write(1, code, 0)
            self.pad.syn()

    def close(self):
        if self.pad:
            try:
                self.neutralize()
            finally:
                self.pad.close()
                self.pad = None


def dispatch(scenario, controller, guard, emit, wait, now, cancelled):
    """Injectable clock/backend seam exercises timing, cancellation and cleanup."""
    origin = now()
    try:
        for event in scenario["events"]:
            deadline = origin + int(event["at_ms"] * 1000)
            while now() < deadline:
                if cancelled():
                    raise RuntimeError("cancelled")
                guard()
                wait(min(0.02, max(0, (deadline - now()) / 1e6)))
            if cancelled():
                raise RuntimeError("cancelled")
            guard()
            actual = now()
            if actual - deadline > scenario["timing_tolerance_ms"] * 1000:
                raise RuntimeError("dispatch missed timing tolerance")
            action = event.get("action", "state")
            if action == "disconnect":
                controller.close()
            elif action == "connect":
                controller.connect()
            else:
                for kind, key in ((3, "axes"), (1, "buttons")):
                    for code, value in event.get(key, {}).items():
                        controller.write(kind, int(code), value)
            completed = now()
            emit(dict(event, planned_boottime_us=deadline, actual_boottime_us=actual,
                      completed_boottime_us=completed, utc_us=time.time_ns() // 1000))
            if completed - deadline > scenario["timing_tolerance_ms"] * 1000:
                raise RuntimeError("dispatch completion missed timing tolerance")
        end = origin + int(scenario["deadline_ms"] * 1000)
        while now() < end:
            if cancelled():
                raise RuntimeError("cancelled")
            guard()
            wait(min(0.02, max(0, (end - now()) / 1e6)))
    finally:
        controller.close()


def check_receipts(scenario, rows, start, end, expected_pid=None):
    scoped = [row for row in rows if start <= row.get("boottime_us", row.get("received_boottime_us", -1)) <= end
              and (expected_pid is None or row.get("pid") == expected_pid)]
    results = []
    for assertion in scenario.get("assertions", []):
        matches = sum(all(row.get(key) == value for key, value in assertion["match"].items()) for row in scoped)
        results.append({"assertion": assertion, "matches": matches, "passed": matches >= assertion["min_count"]})
    if not results or not all(result["passed"] for result in results):
        raise RuntimeError(f"observable outcome assertions failed: {results}")
    return results


def stop_receiver(receiver, start):
    if receiver and receiver.poll() is None and identity(receiver.pid) == start:
        receiver.terminate()
        try:
            receiver.wait(timeout=2)
        except subprocess.TimeoutExpired:
            if identity(receiver.pid) == start:
                receiver.kill()
            receiver.wait(timeout=2)


def run(args):
    scenario_bytes = args.scenario.read_bytes()
    scenario = validate(json.loads(scenario_bytes))
    if args.fixture and scenario["route"] != "direct":
        raise ValueError("headless receiver cannot validate Steam's route")
    if not args.fixture and not args.ready_receipt:
        raise ValueError("game adapter readiness receipt is required")
    args.out.mkdir(parents=True, exist_ok=False)
    result = dict(schema_version=1, passed=False, scenario_sha256=hashlib.sha256(scenario_bytes).hexdigest(),
                  route=scenario["route"], mode="diagnostic", fixture=args.fixture,
                  instrumentation=["synthetic uinput", "process guard", "dispatch receipts"],
                  limitations=["not physical-controller acceptance", "not a performance trial",
                               "foreign-game detection covers Steam IDs; unknown non-Steam games require explicit device reservation"])
    receiver = None
    receiver_start = None
    controller = None
    cancelled = False
    def cancel(*_):
        nonlocal cancelled
        cancelled = True
    handlers = {sig: signal.signal(sig, cancel) for sig in (signal.SIGTERM, signal.SIGINT)}
    try:
        with ExitStack() as cleanup:
            lease = cleanup.enter_context(Lease(args.owner, args.project, "synthetic controller scenario", args.game_id, args.lease))
            guard = Guard(args.pid, args.window, args.expected_sha256, args.game_id, scenario["route"], args.fixture)
            result["ownership"] = dict(lease.record, target_pid=args.pid, target_start=guard.start)
            if not args.fixture:
                ready = json.loads(args.ready_receipt.read_text())
                if not (ready.get("ready") is True and ready.get("pid") == args.pid
                        and ready.get("process_start") == guard.start and ready.get("build_sha256") == args.expected_sha256):
                    raise RuntimeError("readiness receipt does not match target process/build")
                if not scenario.get("expected_render_size") or ready.get("render_size") != scenario["expected_render_size"]:
                    raise RuntimeError("render resolution is missing or does not match scenario")
                result["readiness"] = ready
            controller = LinuxController(scenario["controller"])
            cleanup.callback(controller.close)
            result["owned_device"] = controller.pad.device.path
            if args.fixture:
                # The receiver opens only the node returned by this UInput instance.
                receiver = subprocess.Popen([sys.executable, str(Path(__file__).with_name("receiver.py")),
                    "--device", controller.pad.device.path, "--output", str(args.out / "received.jsonl"),
                    "--ready", str(args.out / "ready.json")], stdin=subprocess.DEVNULL)
                receiver_start = identity(receiver.pid)
                cleanup.callback(stop_receiver, receiver, receiver_start)
                deadline = time.monotonic() + 5
                while not (args.out / "ready.json").is_file():
                    guard.check()
                    if cancelled or receiver.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError("receiver failed readiness")
                    time.sleep(0.02)
            def guarded():
                guard.check()
                if args.fixture and receiver.poll() is not None:
                    raise RuntimeError("receiver disconnected")
                if time.monotonic() - lease.last_heartbeat > 1:
                    lease.heartbeat("dispatch")
            start = boottime_us()
            with (args.out / "injected.jsonl").open("w") as events:
                def emit(event):
                    events.write(json.dumps(event) + "\n")
                    events.flush()
                dispatch(scenario, controller, guarded, emit, time.sleep, boottime_us, lambda: cancelled)
            # Allow the independently scheduled receiver to flush the release events.
            time.sleep(0.1)
            end = boottime_us()
            receipts = args.out / "received.jsonl" if args.fixture else args.telemetry
            if receipts is None or not receipts.is_file():
                raise RuntimeError("game-received action telemetry is required for acceptance")
            rows = [json.loads(line) for line in receipts.read_text().splitlines() if line.strip()]
            if not args.fixture:
                rows = [row for row in rows if row.get("process_start") == guard.start and row.get("build_sha256") == args.expected_sha256]
            result["assertions"] = check_receipts(scenario, rows, start, end, None if args.fixture else args.pid)
            if cancelled:
                raise RuntimeError("cancelled")
            result.update(passed=True, start_boottime_us=start, end_boottime_us=end,
                          target_build_sha256=args.expected_sha256)
    except (Exception, KeyboardInterrupt) as error:
        result["failure"] = str(error)
    finally:
        if controller:
            controller.close()
        stop_receiver(receiver, receiver_start)
        for sig, handler in handlers.items():
            signal.signal(sig, handler)
        atomic_json(args.out / "report.json", result)
    print(json.dumps(result, indent=2))
    return 0 if result["passed"] else 1


def lease_command(args):
    command = args.exec_command
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        raise ValueError("lease-run requires a command after --")
    with Lease(args.owner, args.project, args.purpose, args.game_id, args.lease) as lease:
        child = subprocess.Popen(command)
        child_start = identity(child.pid)
        def forward(sig, _):
            if child.poll() is None and identity(child.pid) == child_start:
                child.send_signal(sig)
        handlers = {sig: signal.signal(sig, forward) for sig in (signal.SIGINT, signal.SIGTERM)}
        try:
            while child.poll() is None:
                lease.heartbeat("command")
                time.sleep(0.5)
            return child.returncode
        finally:
            if child.poll() is None and identity(child.pid) == child_start:
                child.terminate()
                child.wait(timeout=5)
            for sig, handler in handlers.items():
                signal.signal(sig, handler)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    lease = commands.add_parser("lease-status")
    lease.add_argument("--lease", type=Path)
    lease_run = commands.add_parser("lease-run", help="hold the shared lease and heartbeat while a command runs")
    lease_run.add_argument("--owner", required=True)
    lease_run.add_argument("--project", required=True)
    lease_run.add_argument("--purpose", required=True)
    lease_run.add_argument("--game-id")
    lease_run.add_argument("--lease", type=Path)
    lease_run.add_argument("exec_command", nargs=argparse.REMAINDER)
    replay = commands.add_parser("from-recording", help="convert one verified device track; does not inject")
    replay.add_argument("session", type=Path)
    replay.add_argument("--game-id", required=True)
    replay.add_argument("--device", required=True)
    replay.add_argument("--template", type=Path, required=True)
    replay.add_argument("--deadzone", type=float, default=0.15)
    replay.add_argument("--output", type=Path, required=True)
    runner = commands.add_parser("run")
    runner.add_argument("scenario", type=Path)
    runner.add_argument("--out", type=Path, required=True)
    runner.add_argument("--owner", required=True)
    runner.add_argument("--project", required=True)
    runner.add_argument("--lease", type=Path)
    runner.add_argument("--game-id")
    runner.add_argument("--pid", type=int)
    runner.add_argument("--window")
    runner.add_argument("--expected-sha256")
    runner.add_argument("--ready-receipt", type=Path)
    runner.add_argument("--telemetry", type=Path)
    runner.add_argument("--fixture", action="store_true", help="headless owned evdev receiver only; no game or Steam-route claim")
    args = parser.parse_args(argv)
    if args.command == "lease-status":
        print(json.dumps(status(args.lease), indent=2))
        return 0
    try:
        if args.command == "lease-run":
            return lease_command(args)
        if args.command == "from-recording":
            from .replay import from_recording
            scenario = from_recording(args.session, args.game_id, args.device,
                json.loads(args.template.read_text()), args.deadzone)
            with args.output.open("x") as output:
                json.dump(scenario, output, indent=2)
                output.write("\n")
            return 0
        return run(args)
    except (OSError, ValueError, KeyError) as error:
        parser.error(str(error))
