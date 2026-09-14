from __future__ import annotations

import argparse
import csv
import json
import sys
import tempfile
import unittest
from pathlib import Path

import cv2
import numpy

SCRIPT_DIRECTORY = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPT_DIRECTORY))
import video_metrics  # noqa: E402


class VideoMetricTests(unittest.TestCase):
    def test_template_location_finds_translated_pattern(self) -> None:
        template = numpy.zeros((8, 8), dtype=numpy.uint8)
        template[1:4, 1:3] = 180
        template[4:7, 4:7] = 255
        frame = numpy.zeros((60, 80), dtype=numpy.uint8)
        frame[24:32, 31:39] = template
        box, confidence = video_metrics.template_location(
            frame, template, (27, 20, 8, 8), 12
        )
        self.assertEqual(box, (31, 24, 8, 8))
        self.assertGreater(confidence, 0.99)

    def test_phase_correlation_detects_background_translation(self) -> None:
        generator = numpy.random.default_rng(7)
        previous = generator.normal(size=(64, 96)).astype(numpy.float32)
        matrix = numpy.float32([[1, 0, 3], [0, 1, -2]])
        current = cv2.warpAffine(
            previous,
            matrix,
            (previous.shape[1], previous.shape[0]),
            flags=cv2.INTER_LINEAR,
            borderMode=cv2.BORDER_WRAP,
        )
        dx, dy, confidence = video_metrics.background_shift(previous, current)
        self.assertAlmostEqual(dx, 3.0, delta=0.25)
        self.assertAlmostEqual(dy, -2.0, delta=0.25)
        self.assertGreater(confidence, 0.5)

    def test_jump_detection_separates_input_latency_and_airborne_time(self) -> None:
        heights = [0, 0, 0, 5, 10, 5, 0, 0, 0]
        rows = [
            {"time": index / 10.0, "world_y_px": 100.0 - height}
            for index, height in enumerate(heights)
        ]
        results = video_metrics.detect_jumps(
            rows=rows,
            press_times=[0.2],
            y_column="world_y_px",
            pre=0.1,
            maximum_duration=1.0,
            minimum_height=3.0,
            landing_tolerance=1.0,
            stable_samples=2,
            vertical_sign="up",
        )
        self.assertEqual(len(results), 1)
        jump = results[0]
        self.assertTrue(jump["complete"])
        self.assertAlmostEqual(jump["input_to_takeoff"], 0.1)
        self.assertAlmostEqual(jump["time_to_apex"], 0.1)
        self.assertAlmostEqual(jump["airborne_duration"], 0.3)
        self.assertAlmostEqual(jump["height"], 10.0)

    def test_tracking_integration_reports_velocity_and_acceleration(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            video = directory / "video.mp4"
            writer = cv2.VideoWriter(
                str(video),
                cv2.VideoWriter_fourcc(*"mp4v"),
                10.0,
                (120, 80),
            )
            if not writer.isOpened():
                self.skipTest("OpenCV MP4 writer is unavailable")
            pattern = numpy.zeros((10, 10, 3), dtype=numpy.uint8)
            pattern[1:5, 1:4] = (100, 180, 240)
            pattern[5:9, 5:9] = (240, 80, 160)
            frames = []
            for index in range(10):
                frame = numpy.zeros((80, 120, 3), dtype=numpy.uint8)
                x = 10 + index * 2
                frame[30:40, x : x + 10] = pattern
                writer.write(frame)
                frames.append(
                    {
                        "frame": index,
                        "elapsed_us": index * 100_000,
                        "raw_host": {"gamepad_buttons": [], "gamepad_axes": {}},
                    }
                )
            writer.release()
            (directory / "manifest.json").write_text(
                json.dumps(
                    {
                        "frame_count": 10,
                        "video": "video.mp4",
                        "timing": {"fps": 10.0},
                    }
                )
            )
            (directory / "input.json").write_text(json.dumps(frames))
            output = directory / "track.csv"
            result = video_metrics.track(
                argparse.Namespace(
                    session=str(directory),
                    start=0.0,
                    end=0.9,
                    bbox=(10, 30, 10, 10),
                    background_roi=None,
                    search_radius=6,
                    sample_every=1,
                    template_update=0.0,
                    minimum_confidence=0.45,
                    minimum_background_confidence=0.08,
                    units_per_pixel=1.0,
                    out=str(output),
                )
            )
            self.assertEqual(result["tracked_frames"], 10)
            self.assertAlmostEqual(result["median_world_vx_px_s"], 20.0, delta=0.1)
            with output.open(newline="") as handle:
                rows = list(csv.DictReader(handle))
            self.assertEqual(len(rows), 10)
            self.assertIn("world_acceleration_px_s2", rows[0])


if __name__ == "__main__":
    unittest.main()
