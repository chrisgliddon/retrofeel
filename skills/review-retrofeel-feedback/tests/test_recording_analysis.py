import importlib.util
import json
import sys
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[1] / "scripts" / "recording_analysis.py"
SPEC = importlib.util.spec_from_file_location("recording_analysis", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class RecordingAnalysisTests(unittest.TestCase):
    def test_jsonl_matches_retrofeel_struct_field_order(self):
        transition = {
            "frame": 1,
            "elapsed_us": 16_667,
            "state": {"buttons": 0, "analog_l": {"x": 0, "y": 0}},
            "raw_host": {"keyboard_keys": ["A"], "gamepad_buttons": []},
        }

        encoded = MODULE.jsonl([transition])

        self.assertEqual(encoded, json.dumps(transition, separators=(",", ":")) + "\n")
        self.assertTrue(encoded.startswith('{"frame":1,"elapsed_us":16667,"state":'))


if __name__ == "__main__":
    unittest.main()
