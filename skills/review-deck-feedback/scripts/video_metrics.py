#!/usr/bin/env python3
"""Estimate visual gameplay primitives from a RetroFeel session video."""

from __future__ import annotations

import argparse
import bisect
import csv
import json
import math
import shutil
import statistics
import subprocess
import sys
from pathlib import Path
from typing import Any, Iterable, Sequence

SCRIPT_DIRECTORY = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPT_DIRECTORY))
import session_inputs  # noqa: E402


class VideoError(RuntimeError):
    pass


def require_cv() -> tuple[Any, Any]:
    try:
        import cv2
        import numpy
    except ImportError as error:
        raise VideoError(
            "video analysis requires NumPy and OpenCV; create a virtual "
            "environment and install requirements.txt"
        ) from error
    return cv2, numpy


def resolve_video(value: str | Path) -> tuple[Path, Path]:
    path = Path(value).expanduser().resolve()
    if not path.exists():
        raise VideoError(f"path does not exist: {path}")
    if path.is_file() and path.suffix.lower() in {".mkv", ".mp4", ".mov", ".webm"}:
        return path.parent, path

    directory = path if path.is_dir() else path.parent
    manifest_path = directory / "manifest.json"
    video_name = "video.mkv"
    if manifest_path.exists():
        try:
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            if isinstance(manifest.get("video"), str):
                video_name = manifest["video"]
        except (OSError, json.JSONDecodeError) as error:
            raise VideoError(f"cannot read {manifest_path}: {error}") from error
    video = directory / video_name
    if not video.exists():
        raise VideoError(f"session video does not exist: {video}")
    return directory, video


def run_ffprobe(video: Path, frames: bool = False) -> dict[str, Any]:
    ffprobe = shutil.which("ffprobe")
    if not ffprobe:
        raise VideoError("ffprobe is required but was not found on PATH")
    if frames:
        command = [
            ffprobe,
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=best_effort_timestamp_time",
            "-of",
            "json",
            str(video),
        ]
    else:
        command = [
            ffprobe,
            "-v",
            "error",
            "-count_frames",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
            str(video),
        ]
    result = subprocess.run(command, check=False, capture_output=True, text=True)
    if result.returncode != 0:
        raise VideoError(f"ffprobe failed: {result.stderr.strip()}")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise VideoError("ffprobe returned invalid JSON") from error


def video_frame_times(
    directory: Path, video: Path, expected_frames: int
) -> list[float]:
    input_path = directory / "input.json"
    if input_path.exists():
        try:
            frames = json.loads(input_path.read_text(encoding="utf-8"))
            times = [
                float(frame["elapsed_us"]) / 1_000_000.0
                for frame in frames
                if isinstance(frame, dict)
                and isinstance(frame.get("elapsed_us"), (int, float))
            ]
            if len(times) == expected_frames:
                return times
        except (OSError, json.JSONDecodeError, KeyError, TypeError):
            pass

    probe = run_ffprobe(video, frames=True)
    times = []
    for frame in probe.get("frames") or []:
        value = frame.get("best_effort_timestamp_time")
        if value is not None:
            times.append(float(value))
    if not times:
        raise VideoError("video contains no readable frame timestamps")
    origin = times[0]
    return [value - origin for value in times]


def parse_box(value: str) -> tuple[int, int, int, int]:
    try:
        parts = tuple(int(part.strip()) for part in value.split(","))
    except ValueError as error:
        raise argparse.ArgumentTypeError(
            "box must contain four integers: X,Y,W,H"
        ) from error
    if len(parts) != 4 or parts[2] <= 0 or parts[3] <= 0:
        raise argparse.ArgumentTypeError("box must be X,Y,W,H with positive W and H")
    return parts


def parse_times(value: str) -> list[float]:
    try:
        values = [float(part.strip()) for part in value.split(",") if part.strip()]
    except ValueError as error:
        raise argparse.ArgumentTypeError(
            "times must be comma-separated numbers"
        ) from error
    if not values or any(time < 0 for time in values):
        raise argparse.ArgumentTypeError("times must contain non-negative values")
    return values


def open_capture(video: Path) -> tuple[Any, int]:
    cv2, _ = require_cv()
    capture = cv2.VideoCapture(str(video))
    if not capture.isOpened():
        raise VideoError(f"OpenCV could not open {video}")
    frame_count = int(capture.get(cv2.CAP_PROP_FRAME_COUNT))
    # OpenCV/FFmpeg can overflow CAP_PROP_FRAME_COUNT for variable-rate
    # Matroska files without a container duration. ffprobe's decoded count is
    # authoritative for these Steam remuxes.
    if frame_count <= 0 or frame_count > 1_000_000_000:
        data = run_ffprobe(video)
        stream = next(
            (
                item
                for item in data.get("streams") or []
                if item.get("codec_type") == "video"
            ),
            {},
        )
        try:
            frame_count = int(stream.get("nb_read_frames"))
        except (TypeError, ValueError) as error:
            capture.release()
            raise VideoError(
                "video reports no usable frame count through OpenCV or ffprobe"
            ) from error
    return capture, frame_count


def decode_to_frame(capture: Any, index: int) -> Any:
    """Decode from the beginning through index without random seeking.

    Random CAP_PROP_POS_FRAMES/POS_MSEC seeking is not trustworthy for some
    variable-rate Steam Matroska remuxes. Sequential decode preserves the
    one-input-sample-per-encoded-frame contract.
    """
    frame = None
    for current in range(index + 1):
        ok, frame = capture.read()
        if not ok:
            raise VideoError(f"could not decode video frame {current}")
    return frame


def contact_sheet(args: argparse.Namespace) -> dict[str, Any]:
    cv2, numpy = require_cv()
    directory, video = resolve_video(args.session)
    capture, count = open_capture(video)
    times = video_frame_times(directory, video, count)
    if args.times:
        requested = args.times
    else:
        end = args.end if args.end is not None else times[-1]
        requested = []
        value = args.start
        while value <= end + 1e-9:
            requested.append(value)
            value += args.every
    if len(requested) > 100:
        capture.release()
        raise VideoError("contact sheet is limited to 100 frames")

    requested_indices = [
        min(bisect.bisect_left(times, requested_time), count - 1)
        for requested_time in requested
    ]
    target_indices = set(requested_indices)
    decoded: dict[int, Any] = {}
    for index in range(max(target_indices) + 1):
        ok, frame = capture.read()
        if not ok:
            capture.release()
            raise VideoError(f"could not decode video frame {index}")
        if index in target_indices:
            decoded[index] = frame.copy()
    capture.release()

    cells = []
    actual_times = []
    for index in requested_indices:
        frame = decoded[index]
        height, width = frame.shape[:2]
        scale = args.cell_width / width
        cell = cv2.resize(
            frame,
            (args.cell_width, max(1, round(height * scale))),
            interpolation=cv2.INTER_AREA,
        )
        cv2.rectangle(cell, (0, 0), (195, 31), (0, 0, 0), -1)
        cv2.putText(
            cell,
            f"{times[index]:.3f}s  frame {index}",
            (8, 22),
            cv2.FONT_HERSHEY_SIMPLEX,
            0.55,
            (255, 255, 255),
            1,
            cv2.LINE_AA,
        )
        cells.append(cell)
        actual_times.append(times[index])

    columns = min(args.columns, len(cells))
    rows = math.ceil(len(cells) / columns)
    cell_height = max(cell.shape[0] for cell in cells)
    sheet = numpy.zeros(
        (rows * cell_height, columns * args.cell_width, 3), dtype=numpy.uint8
    )
    for index, cell in enumerate(cells):
        row, column = divmod(index, columns)
        sheet[
            row * cell_height : row * cell_height + cell.shape[0],
            column * args.cell_width : (column + 1) * args.cell_width,
        ] = cell

    output = Path(args.out).expanduser().resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    if not cv2.imwrite(str(output), sheet):
        raise VideoError(f"could not write contact sheet: {output}")
    return {
        "video": str(video),
        "output": str(output),
        "frames": len(cells),
        "times": actual_times,
    }


def template_location(
    gray: Any,
    template: Any,
    previous_box: tuple[int, int, int, int],
    search_radius: int,
) -> tuple[tuple[int, int, int, int], float]:
    cv2, _ = require_cv()
    x, y, width, height = previous_box
    frame_height, frame_width = gray.shape[:2]
    left = max(0, x - search_radius)
    top = max(0, y - search_radius)
    right = min(frame_width, x + width + search_radius)
    bottom = min(frame_height, y + height + search_radius)
    search = gray[top:bottom, left:right]
    if search.shape[1] < width or search.shape[0] < height:
        raise VideoError("search region is smaller than the tracking template")
    result = cv2.matchTemplate(search, template, cv2.TM_CCOEFF_NORMED)
    _, confidence, _, location = cv2.minMaxLoc(result)
    return (left + location[0], top + location[1], width, height), float(confidence)


def background_shift(previous: Any, current: Any) -> tuple[float, float, float]:
    cv2, numpy = require_cv()
    if previous.shape != current.shape:
        raise VideoError("background ROI shape changed")
    height, width = previous.shape[:2]
    window = cv2.createHanningWindow((width, height), cv2.CV_32F)
    shift, confidence = cv2.phaseCorrelate(
        previous.astype(numpy.float32),
        current.astype(numpy.float32),
        window,
    )
    return float(shift[0]), float(shift[1]), float(confidence)


def _gray(frame: Any) -> Any:
    cv2, _ = require_cv()
    return cv2.cvtColor(frame, cv2.COLOR_BGR2GRAY)


def _crop(gray: Any, box: tuple[int, int, int, int]) -> Any:
    x, y, width, height = box
    frame_height, frame_width = gray.shape[:2]
    if x < 0 or y < 0 or x + width > frame_width or y + height > frame_height:
        raise VideoError(
            f"box {box} is outside the {frame_width}x{frame_height} video frame"
        )
    return gray[y : y + height, x : x + width]


TRACK_FIELDS = [
    "frame",
    "time",
    "screen_x_px",
    "screen_y_px",
    "confidence",
    "background_dx_px",
    "background_dy_px",
    "background_confidence",
    "background_x_px",
    "background_y_px",
    "world_x_px",
    "world_y_px",
    "screen_vx_px_s",
    "screen_vy_px_s",
    "world_vx_px_s",
    "world_vy_px_s",
    "world_speed_px_s",
    "world_ax_px_s2",
    "world_ay_px_s2",
    "world_acceleration_px_s2",
    "world_speed_units_s",
]


def write_csv(
    path: Path, rows: Sequence[dict[str, Any]], fields: Sequence[str]
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)


def track(args: argparse.Namespace) -> dict[str, Any]:
    cv2, _ = require_cv()
    directory, video = resolve_video(args.session)
    capture, count = open_capture(video)
    times = video_frame_times(directory, video, count)
    start_index = min(bisect.bisect_left(times, args.start), count - 1)
    end_time = args.end if args.end is not None else times[-1]
    end_index = min(bisect.bisect_right(times, end_time), count)
    if end_index - start_index < 2:
        capture.release()
        raise VideoError("tracking interval contains fewer than two frames")

    first = decode_to_frame(capture, start_index)
    gray = _gray(first)
    box = args.bbox
    template = _crop(gray, box).copy()
    background_box = args.background_roi
    previous_background = (
        _crop(gray, background_box).copy() if background_box is not None else None
    )
    background_x = 0.0
    background_y = 0.0
    rows: list[dict[str, Any]] = []
    previous_output: dict[str, Any] | None = None

    def add_row(
        index: int,
        current_box: tuple[int, int, int, int],
        confidence: float,
        shift_x: float,
        shift_y: float,
        background_confidence: float | None,
    ) -> None:
        nonlocal previous_output
        x, y, width, height = current_box
        screen_x = x + width / 2.0
        screen_y = y + height / 2.0
        world_x = screen_x - background_x
        world_y = screen_y - background_y
        row: dict[str, Any] = {
            "frame": index,
            "time": times[index],
            "screen_x_px": screen_x,
            "screen_y_px": screen_y,
            "confidence": confidence,
            "background_dx_px": shift_x,
            "background_dy_px": shift_y,
            "background_confidence": ""
            if background_confidence is None
            else background_confidence,
            "background_x_px": background_x,
            "background_y_px": background_y,
            "world_x_px": world_x,
            "world_y_px": world_y,
            "screen_vx_px_s": 0.0,
            "screen_vy_px_s": 0.0,
            "world_vx_px_s": 0.0,
            "world_vy_px_s": 0.0,
            "world_speed_px_s": 0.0,
            "world_ax_px_s2": 0.0,
            "world_ay_px_s2": 0.0,
            "world_acceleration_px_s2": 0.0,
            "world_speed_units_s": 0.0,
        }
        if previous_output is not None:
            delta = row["time"] - previous_output["time"]
            if delta > 0:
                row["screen_vx_px_s"] = (
                    screen_x - previous_output["screen_x_px"]
                ) / delta
                row["screen_vy_px_s"] = (
                    screen_y - previous_output["screen_y_px"]
                ) / delta
                row["world_vx_px_s"] = (world_x - previous_output["world_x_px"]) / delta
                row["world_vy_px_s"] = (world_y - previous_output["world_y_px"]) / delta
                row["world_speed_px_s"] = math.hypot(
                    row["world_vx_px_s"], row["world_vy_px_s"]
                )
                row["world_ax_px_s2"] = (
                    row["world_vx_px_s"] - previous_output["world_vx_px_s"]
                ) / delta
                row["world_ay_px_s2"] = (
                    row["world_vy_px_s"] - previous_output["world_vy_px_s"]
                ) / delta
                row["world_acceleration_px_s2"] = math.hypot(
                    row["world_ax_px_s2"], row["world_ay_px_s2"]
                )
                row["world_speed_units_s"] = (
                    row["world_speed_px_s"] * args.units_per_pixel
                )
        rows.append(row)
        previous_output = row

    add_row(start_index, box, 1.0, 0.0, 0.0, None)
    low_confidence = 0
    for index in range(start_index + 1, end_index):
        ok, frame = capture.read()
        if not ok:
            capture.release()
            raise VideoError(f"video decode stopped at frame {index}")
        gray = _gray(frame)
        box, confidence = template_location(gray, template, box, args.search_radius)
        if confidence < args.minimum_confidence:
            low_confidence += 1

        shift_x = shift_y = 0.0
        bg_confidence: float | None = None
        if background_box is not None and previous_background is not None:
            current_background = _crop(gray, background_box)
            shift_x, shift_y, bg_confidence = background_shift(
                previous_background, current_background
            )
            previous_background = current_background.copy()
            if bg_confidence >= args.minimum_background_confidence:
                background_x += shift_x
                background_y += shift_y
            else:
                shift_x = shift_y = 0.0

        if args.template_update > 0 and confidence >= args.minimum_confidence:
            observed = _crop(gray, box)
            template = cv2.addWeighted(
                template,
                1.0 - args.template_update,
                observed,
                args.template_update,
                0.0,
            )
        if (index - start_index) % args.sample_every == 0 or index == end_index - 1:
            add_row(index, box, confidence, shift_x, shift_y, bg_confidence)
    capture.release()

    output = Path(args.out).expanduser().resolve()
    write_csv(output, rows, TRACK_FIELDS)
    usable_speeds = [
        float(row["world_speed_px_s"])
        for row in rows[1:]
        if float(row["confidence"]) >= args.minimum_confidence
    ]
    usable_vx = [
        float(row["world_vx_px_s"])
        for row in rows[1:]
        if float(row["confidence"]) >= args.minimum_confidence
    ]
    usable_vy = [
        float(row["world_vy_px_s"])
        for row in rows[1:]
        if float(row["confidence"]) >= args.minimum_confidence
    ]
    return {
        "video": str(video),
        "output": str(output),
        "start": times[start_index],
        "end": times[end_index - 1],
        "samples": len(rows),
        "tracked_frames": end_index - start_index,
        "low_confidence_frames": low_confidence,
        "minimum_confidence": args.minimum_confidence,
        "camera_compensated": background_box is not None,
        "median_world_speed_px_s": statistics.median(usable_speeds)
        if usable_speeds
        else None,
        "median_world_vx_px_s": statistics.median(usable_vx) if usable_vx else None,
        "median_world_vy_px_s": statistics.median(usable_vy) if usable_vy else None,
        "p95_world_speed_px_s": session_inputs._percentile(usable_speeds, 0.95),
        "units_per_pixel": args.units_per_pixel,
        "median_world_speed_units_s": (
            statistics.median(usable_speeds) * args.units_per_pixel
            if usable_speeds
            else None
        ),
    }


MOTION_FIELDS = [
    "frame",
    "time",
    "dx_px",
    "dy_px",
    "confidence",
    "background_x_px",
    "background_y_px",
    "vx_px_s",
    "vy_px_s",
    "speed_px_s",
]


def background_motion(args: argparse.Namespace) -> dict[str, Any]:
    cv2, _ = require_cv()
    directory, video = resolve_video(args.session)
    capture, count = open_capture(video)
    times = video_frame_times(directory, video, count)
    start_index = min(bisect.bisect_left(times, args.start), count - 1)
    end_time = args.end if args.end is not None else times[-1]
    end_index = min(bisect.bisect_right(times, end_time), count)
    first = decode_to_frame(capture, start_index)
    previous = _crop(_gray(first), args.roi).copy()
    position_x = position_y = 0.0
    rows: list[dict[str, Any]] = []
    previous_output_time = times[start_index]
    previous_output_x = previous_output_y = 0.0
    for index in range(start_index + 1, end_index):
        ok, frame = capture.read()
        if not ok:
            break
        current = _crop(_gray(frame), args.roi)
        dx, dy, confidence = background_shift(previous, current)
        previous = current.copy()
        if confidence < args.minimum_confidence:
            dx = dy = 0.0
        position_x += dx
        position_y += dy
        if (index - start_index) % args.sample_every:
            continue
        delta = times[index] - previous_output_time
        interval_dx = position_x - previous_output_x
        interval_dy = position_y - previous_output_y
        vx = interval_dx / delta if delta > 0 else 0.0
        vy = interval_dy / delta if delta > 0 else 0.0
        rows.append(
            {
                "frame": index,
                "time": times[index],
                "dx_px": interval_dx,
                "dy_px": interval_dy,
                "confidence": confidence,
                "background_x_px": position_x,
                "background_y_px": position_y,
                "vx_px_s": vx,
                "vy_px_s": vy,
                "speed_px_s": math.hypot(vx, vy),
            }
        )
        previous_output_time = times[index]
        previous_output_x = position_x
        previous_output_y = position_y
    capture.release()
    if args.out:
        output = Path(args.out).expanduser().resolve()
        write_csv(output, rows, MOTION_FIELDS)
    else:
        output = None
    confident = [
        row["speed_px_s"]
        for row in rows
        if row["confidence"] >= args.minimum_confidence
    ]
    return {
        "video": str(video),
        "roi": args.roi,
        "start": times[start_index],
        "end": times[min(end_index - 1, len(times) - 1)],
        "samples": len(rows),
        "output": str(output) if output else None,
        "background_displacement_px": [position_x, position_y],
        "median_speed_px_s": statistics.median(confident) if confident else None,
    }


def load_track(path: str | Path) -> list[dict[str, float]]:
    rows: list[dict[str, float]] = []
    with Path(path).expanduser().open(newline="", encoding="utf-8") as handle:
        for raw in csv.DictReader(handle):
            try:
                rows.append(
                    {
                        key: float(value)
                        for key, value in raw.items()
                        if value not in (None, "")
                    }
                )
            except ValueError as error:
                raise VideoError(
                    f"non-numeric value in tracking CSV: {error}"
                ) from error
    if not rows:
        raise VideoError("tracking CSV contains no samples")
    return rows


def detect_jumps(
    rows: Sequence[dict[str, float]],
    press_times: Iterable[float],
    y_column: str,
    pre: float,
    maximum_duration: float,
    minimum_height: float,
    landing_tolerance: float,
    stable_samples: int,
    vertical_sign: str,
) -> list[dict[str, Any]]:
    results: list[dict[str, Any]] = []
    ordered_presses = sorted(press_times)
    for press_index, press in enumerate(ordered_presses):
        next_press = (
            ordered_presses[press_index + 1]
            if press_index + 1 < len(ordered_presses)
            else None
        )
        window_end = press + maximum_duration
        if next_press is not None:
            window_end = min(window_end, next_press)
        window = [
            row
            for row in rows
            if press - pre <= row["time"] < window_end and y_column in row
        ]
        before = [row[y_column] for row in window if row["time"] <= press]
        after = [row for row in window if row["time"] >= press]
        if not before or not after:
            continue
        baseline = statistics.median(before)

        def height(row: dict[str, float]) -> float:
            delta = baseline - row[y_column]
            return delta if vertical_sign == "up" else -delta

        takeoff_index = next(
            (index for index, row in enumerate(after) if height(row) >= minimum_height),
            None,
        )
        if takeoff_index is None:
            results.append(
                {
                    "press": press,
                    "detected": False,
                    "reason": "minimum height not reached",
                }
            )
            continue
        apex_index = max(
            range(takeoff_index, len(after)), key=lambda index: height(after[index])
        )
        landing_index: int | None = None
        for index in range(apex_index + 1, len(after)):
            stable = after[index : index + stable_samples]
            if len(stable) < stable_samples:
                break
            if all(abs(height(row)) <= landing_tolerance for row in stable):
                landing_index = index
                break

        takeoff = after[takeoff_index]["time"]
        apex = after[apex_index]["time"]
        landing = after[landing_index]["time"] if landing_index is not None else None
        results.append(
            {
                "press": press,
                "detected": True,
                "baseline_y": baseline,
                "takeoff": takeoff,
                "input_to_takeoff": takeoff - press,
                "apex": apex,
                "time_to_apex": apex - takeoff,
                "height": height(after[apex_index]),
                "landing": landing,
                "airborne_duration": landing - takeoff if landing is not None else None,
                "complete": landing is not None,
            }
        )
    return results


def jumps(args: argparse.Namespace) -> dict[str, Any]:
    rows = load_track(args.track_csv)
    try:
        session = session_inputs.load_session(args.session, args.port)
    except session_inputs.SessionError as error:
        raise VideoError(str(error)) from error
    spans = session_inputs.button_spans(session, args.button)
    start = rows[0]["time"]
    end = rows[-1]["time"]
    press_times = [
        float(span["start"]) for span in spans if start <= float(span["start"]) <= end
    ]
    results = detect_jumps(
        rows,
        press_times,
        args.y_column,
        args.pre,
        args.max_duration,
        args.min_height,
        args.landing_tolerance,
        args.stable_samples,
        args.vertical_sign,
    )
    complete = [result for result in results if result.get("complete")]
    durations = [float(result["airborne_duration"]) for result in complete]
    latencies = [float(result["input_to_takeoff"]) for result in complete]
    heights = [float(result["height"]) for result in complete]
    return {
        "session_id": session.session_id,
        "button": args.button,
        "track_csv": str(Path(args.track_csv).expanduser().resolve()),
        "presses_in_trace": len(press_times),
        "detected": sum(bool(result.get("detected")) for result in results),
        "complete": len(complete),
        "median_airborne_duration": statistics.median(durations) if durations else None,
        "median_input_to_takeoff": statistics.median(latencies) if latencies else None,
        "median_height": statistics.median(heights) if heights else None,
        "results": results,
    }


def probe(args: argparse.Namespace) -> dict[str, Any]:
    directory, video = resolve_video(args.session)
    data = run_ffprobe(video)
    streams = data.get("streams") or []
    video_stream = next(
        (stream for stream in streams if stream.get("codec_type") == "video"), {}
    )
    audio_streams = [
        stream for stream in streams if stream.get("codec_type") == "audio"
    ]
    duration = (data.get("format") or {}).get("duration") or video_stream.get(
        "duration"
    )
    result: dict[str, Any] = {
        "video": str(video),
        "bytes": video.stat().st_size,
        "codec": video_stream.get("codec_name"),
        "width": video_stream.get("width"),
        "height": video_stream.get("height"),
        "average_frame_rate": video_stream.get("avg_frame_rate"),
        "frames": video_stream.get("nb_read_frames"),
        "duration": duration,
        "audio_streams": [
            {
                "codec": stream.get("codec_name"),
                "sample_rate": stream.get("sample_rate"),
                "channels": stream.get("channels"),
            }
            for stream in audio_streams
        ],
        "input_frames": None,
        "frame_count_matches": None,
    }
    input_path = directory / "input.json"
    if input_path.exists():
        try:
            input_data = json.loads(input_path.read_text(encoding="utf-8"))
            input_frames = len(input_data)
            result["input_frames"] = input_frames
            result["frame_count_matches"] = str(input_frames) == str(result["frames"])
            if result["duration"] is None and input_data:
                elapsed = input_data[-1].get("elapsed_us")
                if isinstance(elapsed, (int, float)):
                    result["duration"] = str(float(elapsed) / 1_000_000.0)
        except (OSError, json.JSONDecodeError, TypeError):
            pass
    if args.decode:
        ffmpeg = shutil.which("ffmpeg")
        if not ffmpeg:
            raise VideoError("ffmpeg is required for --decode")
        decoded = subprocess.run(
            [
                ffmpeg,
                "-v",
                "warning",
                "-i",
                str(video),
                "-map",
                "0:v:0",
                "-f",
                "null",
                "-",
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        result["decode_ok"] = decoded.returncode == 0
        result["decode_warnings"] = [
            line for line in decoded.stderr.splitlines() if line.strip()
        ]
    return result


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Probe and estimate visual metrics from RetroFeel video."
    )
    commands = parser.add_subparsers(dest="command", required=True)

    probe_parser = commands.add_parser("probe", help="inspect streams and frame counts")
    probe_parser.add_argument("session")
    probe_parser.add_argument("--json", action="store_true", help="emit JSON only")
    probe_parser.add_argument(
        "--decode", action="store_true", help="decode the full video"
    )

    sheet = commands.add_parser(
        "contact-sheet", help="export timestamped sample frames"
    )
    sheet.add_argument("session")
    sheet.add_argument("--json", action="store_true", help="emit JSON only")
    sheet.add_argument("--start", type=float, default=0.0)
    sheet.add_argument("--end", type=float)
    sheet.add_argument("--every", type=float, default=5.0)
    sheet.add_argument("--times", type=parse_times)
    sheet.add_argument("--cell-width", type=int, default=480)
    sheet.add_argument("--columns", type=int, default=3)
    sheet.add_argument("--out", required=True)

    tracking = commands.add_parser("track", help="track an actor from an initial box")
    tracking.add_argument("session")
    tracking.add_argument("--json", action="store_true", help="emit JSON only")
    tracking.add_argument("--start", type=float, required=True)
    tracking.add_argument("--end", type=float)
    tracking.add_argument("--bbox", type=parse_box, required=True)
    tracking.add_argument("--background-roi", type=parse_box)
    tracking.add_argument("--search-radius", type=int, default=80)
    tracking.add_argument("--sample-every", type=int, default=1)
    tracking.add_argument("--template-update", type=float, default=0.02)
    tracking.add_argument("--minimum-confidence", type=float, default=0.45)
    tracking.add_argument("--minimum-background-confidence", type=float, default=0.08)
    tracking.add_argument("--units-per-pixel", type=float, default=1.0)
    tracking.add_argument("--out", required=True)

    motion = commands.add_parser(
        "background-motion", help="measure dominant translation in a background ROI"
    )
    motion.add_argument("session")
    motion.add_argument("--json", action="store_true", help="emit JSON only")
    motion.add_argument("--start", type=float, required=True)
    motion.add_argument("--end", type=float)
    motion.add_argument("--roi", type=parse_box, required=True)
    motion.add_argument("--minimum-confidence", type=float, default=0.08)
    motion.add_argument("--sample-every", type=int, default=1)
    motion.add_argument("--out")

    jump_parser = commands.add_parser(
        "jumps", help="estimate jumps from input edges and an actor tracking CSV"
    )
    jump_parser.add_argument("session")
    jump_parser.add_argument("--json", action="store_true", help="emit JSON only")
    jump_parser.add_argument("--track-csv", required=True)
    jump_parser.add_argument("--button", required=True)
    jump_parser.add_argument("--port", type=int, default=0)
    jump_parser.add_argument("--y-column", default="world_y_px")
    jump_parser.add_argument("--pre", type=float, default=0.10)
    jump_parser.add_argument("--max-duration", type=float, default=2.0)
    jump_parser.add_argument("--min-height", type=float, default=3.0)
    jump_parser.add_argument("--landing-tolerance", type=float, default=2.0)
    jump_parser.add_argument("--stable-samples", type=int, default=3)
    jump_parser.add_argument("--vertical-sign", choices=("up", "down"), default="up")
    return parser


def print_result(command: str, result: dict[str, Any], as_json: bool) -> None:
    if as_json:
        json.dump(result, sys.stdout, indent=2)
        print()
        return
    if command == "probe":
        print(
            f"{result['video']}: {result['width']}x{result['height']} "
            f"{result['codec']}, {result['frames']} frames, "
            f"{result['duration']}s, audio streams={len(result['audio_streams'])}"
        )
        print(
            f"Input frames: {result['input_frames']}; "
            f"count matches: {result['frame_count_matches']}"
        )
        if "decode_ok" in result:
            print(
                f"Full decode: {'ok' if result['decode_ok'] else 'failed'}; "
                f"warnings: {len(result['decode_warnings'])}"
            )
    elif command == "contact-sheet":
        print(
            f"Wrote {result['frames']} frames from {result['video']} "
            f"to {result['output']}"
        )
    elif command == "track":
        print(
            f"Wrote {result['samples']} samples ({result['tracked_frames']} frames) "
            f"to {result['output']}"
        )
        print(
            f"Low-confidence frames: {result['low_confidence_frames']}; "
            f"median world speed: {result['median_world_speed_px_s']} px/s; "
            f"median vx/vy: {result['median_world_vx_px_s']}/"
            f"{result['median_world_vy_px_s']} px/s; "
            f"camera compensated: {result['camera_compensated']}"
        )
    elif command == "background-motion":
        print(
            f"Background displacement: {result['background_displacement_px']} px; "
            f"median speed: {result['median_speed_px_s']} px/s"
        )
        if result["output"]:
            print(f"Wrote samples to {result['output']}")
    elif command == "jumps":
        print(
            f"Button {result['button']}: {result['presses_in_trace']} presses, "
            f"{result['detected']} takeoffs, {result['complete']} complete jumps"
        )
        print(
            f"Median airborne={result['median_airborne_duration']}s; "
            f"input-to-takeoff={result['median_input_to_takeoff']}s; "
            f"height={result['median_height']}px"
        )
        for jump in result["results"]:
            if jump.get("complete"):
                print(
                    f"  press {jump['press']:.3f}s -> takeoff "
                    f"{jump['takeoff']:.3f}s, airborne "
                    f"{jump['airborne_duration']:.3f}s, height "
                    f"{jump['height']:.2f}px"
                )


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        if args.command == "probe":
            result = probe(args)
        elif args.command == "contact-sheet":
            if args.every <= 0 or args.columns <= 0 or args.cell_width <= 0:
                raise VideoError(
                    "contact-sheet dimensions and interval must be positive"
                )
            result = contact_sheet(args)
        elif args.command == "track":
            if args.sample_every <= 0 or args.search_radius < 0:
                raise VideoError(
                    "sample interval must be positive and radius non-negative"
                )
            if not 0 <= args.template_update <= 1:
                raise VideoError("template update must be between 0 and 1")
            result = track(args)
        elif args.command == "background-motion":
            if args.sample_every <= 0:
                raise VideoError("sample interval must be positive")
            result = background_motion(args)
        elif args.command == "jumps":
            if args.stable_samples <= 0:
                raise VideoError("stable samples must be positive")
            result = jumps(args)
        else:
            raise VideoError(f"unknown command: {args.command}")
        print_result(args.command, result, args.json)
        return 0
    except (VideoError, session_inputs.SessionError) as error:
        parser.error(str(error))
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
