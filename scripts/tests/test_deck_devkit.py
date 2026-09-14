import json
import multiprocessing
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from devkit.lease import Lease, status
from devkit.guard import Guard, identity, steam_games
from devkit.runner import dispatch, check_receipts
from devkit.scenario import validate


def contender(path, result):
    try:
        with Lease("second", "test", "contention", "42", path):
            result.put("acquired")
    except RuntimeError:
        result.put("busy")


def crash_owner(path):
    lease = Lease("crash", "test", "crash recovery", "42", path)
    lease.__enter__()
    os._exit(0)


class FakeController:
    def __init__(self):
        self.state = {}
        self.closed = False
    def write(self, kind, code, value):
        self.state[kind, code] = value
    def close(self):
        self.state = {key: 0 for key in self.state}
        self.closed = True
    def connect(self):
        self.closed = False


class DevkitTests(unittest.TestCase):
    def scenario(self):
        return json.loads((Path(__file__).resolve().parents[1] / "devkit/scenarios/receiver-smoke.json").read_text())

    def test_two_processes_cannot_share_a_lease_even_with_expired_metadata(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "lease"
            with Lease("first", "test", "test", "42", path):
                Path(str(path) + ".json").write_text('{"expires_monotonic":0}')
                context = multiprocessing.get_context("spawn")
                result = context.Queue()
                process = context.Process(target=contender, args=(path, result))
                process.start()
                process.join(5)
                self.assertEqual(result.get(timeout=1), "busy")
                self.assertTrue(status(path)["active"])
            with Lease("next", "test", "recovery", "42", path):
                self.assertTrue(status(path)["active"])
            self.assertFalse(status(path)["active"])

    def test_crashed_owner_leaves_metadata_but_releases_kernel_lease(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "lease"
            process = multiprocessing.get_context("spawn").Process(target=crash_owner, args=(path,))
            process.start()
            process.join(5)
            self.assertEqual(process.exitcode, 0)
            self.assertEqual(status(path)["status"], "owned")
            self.assertFalse(status(path)["active"])
            with Lease("recovery", "test", "recovery", "42", path):
                self.assertTrue(status(path)["active"])

    def test_native_and_proton_game_without_wrapper_are_detected(self):
        with tempfile.TemporaryDirectory() as root:
            for pid, environment, command in [(1, b"SteamAppId=42\0SECRET=not-exported", b"native\0"),
                    (2, b"STEAM_COMPAT_APP_ID=43\0", b"ClockworkValley_EU.exe\0"),
                    (3, b"SteamAppId=0\0", b"steamwebhelper\0")]:
                directory = Path(root) / str(pid)
                directory.mkdir()
                (directory / "environ").write_bytes(environment)
                (directory / "cmdline").write_bytes(command)
            self.assertEqual(steam_games(Path(root)), {1: {"42"}, 2: {"43"}})

    def test_guard_rejects_focus_loss_and_pid_reuse(self):
        with patch("devkit.guard.identity", return_value="start"), patch("devkit.guard.executable_hash", return_value="hash"), \
             patch("devkit.guard.steam_games", return_value={10: {"42"}}), \
             patch("devkit.guard.subprocess.check_output", side_effect=["10", "123", "456"]):
            guard = Guard(10, "123", "hash", "42", "steam")
            with self.assertRaisesRegex(RuntimeError, "focus"):
                guard.check()
            with patch("devkit.guard.identity", return_value="reused"):
                with self.assertRaisesRegex(RuntimeError, "reused"):
                    guard.check()

    def test_cancellation_and_foreign_guard_failure_neutralize_owned_input(self):
        for failure in ("cancel", "foreign"):
            clock = [0]
            controller = FakeController()
            def wait(seconds):
                clock[0] += round(seconds * 1e6)
            def guard():
                if failure == "foreign" and clock[0] >= 200000:
                    raise RuntimeError("foreign game")
            with self.assertRaises(RuntimeError):
                dispatch(self.scenario(), controller, guard, lambda _: None, wait, lambda: clock[0],
                         lambda: failure == "cancel" and clock[0] >= 200000)
            self.assertTrue(controller.closed)
            self.assertEqual(controller.state[3, 0], 0)

    def test_dispatch_records_planned_and_actual_clocks_and_rejects_lateness(self):
        clock = [0]
        rows = []
        controller = FakeController()
        dispatch(self.scenario(), controller, lambda: None, rows.append,
                 lambda seconds: clock.__setitem__(0, clock[0] + round(seconds * 1e6)), lambda: clock[0], lambda: False)
        self.assertEqual(rows[0]["planned_boottime_us"], 100000)
        self.assertEqual(rows[0]["actual_boottime_us"], 100000)
        with self.assertRaisesRegex(RuntimeError, "tolerance"):
            dispatch(self.scenario(), controller, lambda: None, rows.append,
                     lambda _: clock.__setitem__(0, clock[0] + 1000000), lambda: clock[0], lambda: False)
        self.assertTrue(controller.closed)

    def test_fast_but_no_actions_and_stale_receipts_fail(self):
        for rows in ([], [{"received_boottime_us": 0, "type": 3, "code": 0, "value": 16000}]):
            with self.assertRaisesRegex(RuntimeError, "assertions"):
                check_receipts(self.scenario(), rows, 100, 1000)

    def test_replay_selects_exact_track_and_rejects_unverified_timeline(self):
        from devkit.replay import from_recording
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            (directory / "manifest.json").write_text(json.dumps({"frame_count": 2,
                "external_capture": {"status": "complete", "game_id": "42", "recording_id": "fg_42", "video_clock": {"video_pts_zero_boottime_us": 10}}}))
            (directory / "archive-clock.json").write_text(json.dumps({"schema_version": 1, "recording_id": "fg_42", "method": "whole_segment_zero_origin", "first_frame_boottime_us": 10}))
            (directory / "media-validation.json").write_text(json.dumps({"packet_count": 2, "frame_pts_us": [0, 100000]}))
            frames = [{"frame": index, "elapsed_us": index * 100000, "raw_host": {"gamepads": [
                {"device_id": "idle", "port": 0, "buttons": [], "axes": {"LeftStickX": 0}},
                {"device_id": "active", "port": 0, "buttons": ["South"], "axes": {"LeftStickX": 1}}]}} for index in range(2)]
            (directory / "input.json").write_text(json.dumps(frames))
            replay = from_recording(directory, "42", "active", self.scenario())
            self.assertEqual(replay["events"][0]["axes"]["0"], 32767)
            self.assertEqual(replay["events"][0]["buttons"]["304"], 1)
            (directory / "archive-clock.json").write_text("{}")
            with self.assertRaises(ValueError):
                from_recording(directory, "42", "active", self.scenario())

    def test_scenario_rejects_out_of_range_and_implicit_performance_claim(self):
        scenario = self.scenario()
        validate(scenario)
        scenario["events"][0]["axes"]["0"] = 999999
        with self.assertRaises(ValueError):
            validate(scenario)
        scenario = self.scenario()
        scenario["controller"]["buttons"].append(272)
        with self.assertRaises(ValueError):
            validate(scenario)
        scenario = self.scenario()
        scenario["mode"] = "performance"
        with self.assertRaises(ValueError):
            validate(scenario)


if __name__ == "__main__":
    unittest.main()
