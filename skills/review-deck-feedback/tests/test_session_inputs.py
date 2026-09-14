from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT_DIRECTORY = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPT_DIRECTORY))
import session_inputs  # noqa: E402


class SessionInputTests(unittest.TestCase):
    def make_session(self) -> tuple[tempfile.TemporaryDirectory[str], Path]:
        temporary = tempfile.TemporaryDirectory()
        directory = Path(temporary.name)
        manifest = {
            "core": {"name": "Synthetic Game"},
            "frame_count": 8,
            "dropped_frames": 0,
            "timing": {"fps": 10.0},
            "external_capture": {
                "recording_id": "fg_synthetic",
                "status": "complete",
            },
            "video": "video.mkv",
        }
        frames = []
        for index in range(8):
            buttons = []
            if index in (1, 2, 4, 5):
                buttons.append("East")
            if index in (2, 3, 4):
                buttons.append("DPadRight")
            axis = 0.75 if index in (5, 6) else 0.0
            frames.append(
                {
                    "frame": index,
                    "elapsed_us": index * 100_000,
                    "raw_host": {
                        "gamepad_buttons": [],
                        "gamepad_axes": {},
                        "gamepads": [
                            {
                                "device_id": "virtual-pad",
                                "port": 0,
                                "buttons": buttons,
                                "axes": {"LeftStickX": axis},
                            },
                            {
                                "device_id": "physical-pad",
                                "port": 0,
                                "buttons": ["South"] if index in (3, 4) else [],
                                "axes": {"LeftStickX": 0.0},
                            }
                        ],
                    },
                }
            )
        (directory / "manifest.json").write_text(json.dumps(manifest))
        (directory / "input.json").write_text(json.dumps(frames))
        return temporary, directory

    def test_loads_per_port_state_and_summarizes(self) -> None:
        temporary, directory = self.make_session()
        self.addCleanup(temporary.cleanup)
        session = session_inputs.load_session(directory)
        summary = session_inputs.session_summary(session)
        self.assertEqual(summary["session_id"], "fg_synthetic")
        self.assertEqual(summary["buttons"]["East"]["presses"], 2)
        self.assertEqual(summary["frames"], 8)
        self.assertEqual(summary["sampling"]["median_interval_ms"], 100.0)
        self.assertEqual(
            summary["available_device_ids"], ["physical-pad", "virtual-pad"]
        )

    def test_selects_an_explicit_device_when_sources_share_a_port(self) -> None:
        temporary, directory = self.make_session()
        self.addCleanup(temporary.cleanup)

        session = session_inputs.load_session(directory, device_id="physical-pad")
        summary = session_inputs.session_summary(session)

        self.assertEqual(summary["device_id"], "physical-pad")
        self.assertEqual(summary["buttons"]["South"]["presses"], 1)
        self.assertNotIn("East", summary["buttons"])

    def test_extracts_digital_and_analog_runs(self) -> None:
        temporary, directory = self.make_session()
        self.addCleanup(temporary.cleanup)
        session = session_inputs.load_session(directory)
        right = session_inputs.button_spans(session, "DPadRight")
        self.assertEqual(len(right), 1)
        self.assertAlmostEqual(right[0]["start"], 0.2)
        self.assertAlmostEqual(right[0]["duration"], 0.3)

        _, predicate = session_inputs.control_state("LeftStickX+", 0.5)
        analog = session_inputs.extract_spans(session.samples, predicate)
        self.assertEqual(len(analog), 1)
        self.assertAlmostEqual(analog[0]["start"], 0.5)
        self.assertAlmostEqual(analog[0]["duration"], 0.2)
        self.assertAlmostEqual(analog[0]["peak_value"], 0.75)

    def test_computes_cadence_without_calling_it_animation_duration(self) -> None:
        temporary, directory = self.make_session()
        self.addCleanup(temporary.cleanup)
        session = session_inputs.load_session(directory)
        spans = session_inputs.button_spans(session, "East")
        cadence = session_inputs.cadence_for_window(spans, "test", 0.0, 0.7)
        self.assertEqual(cadence["presses"], 2)
        self.assertAlmostEqual(cadence["mean_interval"], 0.3)
        self.assertAlmostEqual(cadence["frequency_hz"], 3.333333)
        self.assertAlmostEqual(cadence["median_hold"], 0.2)


if __name__ == "__main__":
    unittest.main()

class CaptureAuditTests(unittest.TestCase):
    make_session = SessionInputTests.make_session
    def test_disconnected_selected_device_does_not_inherit_legacy_buttons(self):
        frame = {"raw_host": {"gamepads": [{"device_id": "other", "buttons": ["South"]}],
                              "gamepad_buttons": ["South"]}}
        self.assertEqual(session_inputs._raw_state(frame, 0, "absent"), (frozenset(), {}))

    def test_raw_device_activity_survives_neutral_canonical_summary(self):
        temporary, directory = self.make_session()
        self.addCleanup(temporary.cleanup)
        (directory / "input-events.jsonl").write_text(json.dumps({
            "boottime_us": 10000000, "device_id": "physical-pad", "event_type": 1,
            "code": 304, "value": 1}) + "\n")
        rows = session_inputs.device_coverage(session_inputs.load_session(directory))
        self.assertEqual(next(x for x in rows if x["device_id"] == "physical-pad")["raw_key_presses"], 1)
        self.assertEqual(len(rows), 2)

    def test_audit_rejects_wrong_identity_and_accepts_reordered_transition_keys(self):
        temporary, directory = self.make_session()
        self.addCleanup(temporary.cleanup)
        session = session_inputs.load_session(directory)
        session.manifest["external_capture"].update(game_id="42", source_video="/clips/clip_42/video/fg_wrong/session.mpd")
        transitions = session_inputs.canonical_transitions(session.frames)
        (directory / "input-transitions.jsonl").write_text("\n".join(
            json.dumps(dict(reversed(list(row.items())))) for row in transitions))
        report = session_inputs.audit_session(session, "42")
        self.assertIn("wrong_recording_segment", report["findings"])
        self.assertTrue(report["canonical_transitions_match"])
        with self.assertRaises(session_inputs.SessionError):
            session_inputs.audit_session(session, "43")

    def test_latest_filters_game_before_timestamp(self):
        with tempfile.TemporaryDirectory() as root:
            for game, stamp in [("42", 10), ("43", 20)]:
                directory = Path(root) / game
                directory.mkdir()
                (directory / "manifest.json").write_text(json.dumps({
                    "external_capture": {"game_id": game}, "timing": {"start_timestamp": stamp}}))
            self.assertEqual(session_inputs.scoped_latest(root, "42").name, "42")

    def test_empty_layout_is_partial_even_when_another_layout_has_bindings(self):
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            (directory / "controller-map.json").write_text(json.dumps({"layouts": [
                {"bindings": [{"input": "a"}]}, {"bindings": []}]}))
            self.assertEqual(session_inputs.mapping_coverage(directory)["status"], "partial")


class PartialCaptureTests(unittest.TestCase):
    def test_partial_capture_can_be_audited_without_canonical_input(self):
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            (directory / "capture.json").write_text(json.dumps({"game_id": "42", "recording_id": "fg_42_partial"}))
            session = session_inputs.load_audit_session(directory)
            report = session_inputs.audit_session(session, "42")
            self.assertIn("canonical_input_missing", report["findings"])
            self.assertFalse(report["healthy"])
