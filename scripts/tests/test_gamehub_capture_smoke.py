import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

import cv2
import numpy as np


SCRIPT = Path(__file__).parents[1] / "gamehub_capture_smoke.py"
SPEC = importlib.util.spec_from_file_location("gamehub_capture_smoke", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class GameHubCaptureSmokeTests(unittest.TestCase):
    def test_ocr_line_box_matches_title_tokens(self):
        words = [
            MODULE.OcrWord("Meadow", 100, 50, 60, 20, (1, 2, 1, 1)),
            MODULE.OcrWord("of", 170, 50, 20, 20, (1, 2, 1, 1)),
            MODULE.OcrWord("Lanterns", 200, 50, 70, 20, (1, 2, 1, 1)),
            MODULE.OcrWord("Unrelated", 10, 10, 80, 20, (1, 1, 1, 1)),
        ]

        self.assertEqual(MODULE.line_box(words, "Meadow of Lanterns"), (185.0, 60.0))
        self.assertIsNone(MODULE.line_box(words, "CLOCKWORK VALLEY"))

    def test_profile_is_explicit_and_preserves_game_metadata(self):
        profile = SCRIPT.parent / "profiles/fictional.json"
        game = MODULE.load_game_profile(profile)
        self.assertEqual(game.app_id, 42)
        self.assertEqual(game.title, "Meadow of Lanterns")
        self.assertEqual(game.library_title, "Meadow of Lanterns")
        self.assertEqual(game.process_names, ("MeadowOfLanterns.exe",))
        import subprocess
        result = subprocess.run([sys.executable, str(SCRIPT)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("--profile", result.stderr)

    def test_invalid_profiles_fail_before_launch(self):
        good = json.loads((SCRIPT.parent / "profiles/fictional.json").read_text())
        bad = [[], {}, {**good, "extra": 1}, {**good, "app_id": True},
               {**good, "app_id": -1}, {**good, "title": ""},
               {**good, "process_names": []}, {**good, "process_names": ["../game.exe"]}]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "profile.json"
            for value in bad:
                path.write_text(json.dumps(value))
                with self.assertRaises(ValueError):
                    MODULE.load_game_profile(path)

    def test_play_button_detector_finds_white_pill(self):
        image = np.zeros((740, 1080, 3), dtype=np.uint8)
        cv2.rectangle(image, (350, 450), (500, 495), (245, 245, 245), -1)

        point = MODULE.find_play_button(image)

        self.assertIsNotNone(point)
        self.assertAlmostEqual(point[0], 425, delta=2)
        self.assertAlmostEqual(point[1], 472.5, delta=2)

    def test_focus_summary_detects_focus_theft_and_game_focus(self):
        samples = [
            {
                "phase": "attaching",
                "frontmost_pid": 20,
                "game_pid": 10,
                "retrofeel_pid": 20,
            },
            {
                "phase": "recording",
                "frontmost_pid": 10,
                "game_pid": 10,
                "retrofeel_pid": 20,
            },
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "focus-events.jsonl"
            path.write_text("".join(json.dumps(sample) + "\n" for sample in samples))

            summary = MODULE.focus_summary(path)

        self.assertTrue(summary["retrofeel_became_frontmost"])
        self.assertTrue(summary["game_frontmost_during_recording"])
        self.assertEqual(summary["sample_count"], 2)


if __name__ == "__main__":
    unittest.main()
