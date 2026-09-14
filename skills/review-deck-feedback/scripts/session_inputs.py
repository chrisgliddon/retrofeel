#!/usr/bin/env python3
"""Inspect frame-aligned RetroFeel controller input without guessing game state."""

from __future__ import annotations

import argparse
import json
import math
import statistics
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Sequence


class SessionError(RuntimeError):
    pass


@dataclass(frozen=True)
class Sample:
    frame: int
    time: float
    buttons: frozenset[str]
    axes: dict[str, float]


@dataclass
class Session:
    directory: Path
    manifest: dict[str, Any]
    frames: list[dict[str, Any]]
    samples: list[Sample]
    device_id: str | None = None

    @property
    def session_id(self) -> str:
        external = self.manifest.get("external_capture") or {}
        return str(external.get("recording_id") or self.directory.name)

    @property
    def game(self) -> str:
        core = self.manifest.get("core") or {}
        return str(core.get("name") or "unknown")

    @property
    def duration(self) -> float:
        return self.samples[-1].time if self.samples else 0.0


def _read_json(path: Path) -> Any:
    try:
        with path.open("r", encoding="utf-8") as handle:
            return json.load(handle)
    except FileNotFoundError as error:
        raise SessionError(f"missing file: {path}") from error
    except json.JSONDecodeError as error:
        raise SessionError(f"invalid JSON in {path}: {error}") from error


def resolve_session_path(value: str | Path) -> tuple[Path, Path, Path]:
    path = Path(value).expanduser().resolve()
    if not path.exists():
        raise SessionError(f"session path does not exist: {path}")
    directory = path if path.is_dir() else path.parent
    manifest_path = directory / "manifest.json"
    input_path = directory / "input.json"
    if path.name == "manifest.json":
        manifest_path = path
    elif path.name == "input.json":
        input_path = path
    return directory, manifest_path, input_path


def _frame_time(frame: dict[str, Any], fps: float) -> float:
    elapsed = frame.get("elapsed_us")
    if isinstance(elapsed, (int, float)):
        return float(elapsed) / 1_000_000.0
    index = frame.get("frame")
    if isinstance(index, (int, float)) and fps > 0:
        return float(index) / fps
    raise SessionError("input frame has neither elapsed_us nor a usable frame index")


def _raw_state(
    frame: dict[str, Any], port: int, device_id: str | None = None
) -> tuple[frozenset[str], dict[str, float]]:
    raw = frame.get("raw_host") or {}
    gamepads = raw.get("gamepads") or []
    selected: dict[str, Any] | None = None
    for gamepad in gamepads:
        if device_id is not None and gamepad.get("device_id") == device_id:
            selected = gamepad
            break
        if device_id is None and gamepad.get("port") == port:
            selected = gamepad
            break
    if selected is None and device_id is None and port == 0 and gamepads:
        selected = gamepads[0]

    if selected is not None:
        buttons = selected.get("buttons") or []
        axes = selected.get("axes") or {}
    elif device_id is not None:
        # A disconnected explicit device must never fall back to a different track.
        buttons, axes = [], {}
    else:
        buttons = raw.get("gamepad_buttons") or []
        axes = raw.get("gamepad_axes") or {}

    clean_axes: dict[str, float] = {}
    for name, value in axes.items():
        if isinstance(value, (int, float)) and math.isfinite(float(value)):
            clean_axes[str(name)] = float(value)
    return frozenset(str(button) for button in buttons), clean_axes


def load_session(
    value: str | Path, port: int = 0, device_id: str | None = None
) -> Session:
    directory, manifest_path, input_path = resolve_session_path(value)
    manifest = _read_json(manifest_path) if manifest_path.exists() else {}
    frames = _read_json(input_path)
    if not isinstance(frames, list) or not frames:
        raise SessionError(f"input log must be a non-empty JSON array: {input_path}")
    if device_id is not None and not any(
        any(
            gamepad.get("device_id") == device_id
            for gamepad in ((frame.get("raw_host") or {}).get("gamepads") or [])
        )
        for frame in frames
        if isinstance(frame, dict)
    ):
        raise SessionError(f"input device is not present in the session: {device_id}")

    timing = manifest.get("timing") or {}
    fps = float(timing.get("fps") or 60.0)
    samples: list[Sample] = []
    previous_time = -math.inf
    for fallback_index, frame in enumerate(frames):
        if not isinstance(frame, dict):
            raise SessionError(f"input frame {fallback_index} is not an object")
        time = _frame_time(frame, fps)
        if time < previous_time:
            raise SessionError(
                f"input time moved backwards at frame {fallback_index}: "
                f"{time:.6f} < {previous_time:.6f}"
            )
        previous_time = time
        buttons, axes = _raw_state(frame, port, device_id)
        samples.append(
            Sample(
                frame=int(frame.get("frame", fallback_index)),
                time=time,
                buttons=buttons,
                axes=axes,
            )
        )
    return Session(directory, manifest, frames, samples, device_id)


def _percentile(values: Sequence[float], percentile: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    position = (len(ordered) - 1) * percentile
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    fraction = position - lower
    return ordered[lower] * (1.0 - fraction) + ordered[upper] * fraction


def _number(value: float | None, digits: int = 6) -> float | None:
    return round(value, digits) if value is not None else None


def extract_spans(
    samples: Sequence[Sample],
    state: Callable[[Sample], tuple[bool, float]],
    minimum_duration: float = 0.0,
) -> list[dict[str, Any]]:
    spans: list[dict[str, Any]] = []
    start: Sample | None = None
    values: list[float] = []
    for sample in samples:
        active, value = state(sample)
        if active and start is None:
            start = sample
            values = [value]
        elif active:
            values.append(value)
        elif start is not None:
            duration = sample.time - start.time
            if duration >= minimum_duration:
                spans.append(
                    {
                        "start": _number(start.time),
                        "end": _number(sample.time),
                        "duration": _number(duration),
                        "start_frame": start.frame,
                        "end_frame": sample.frame,
                        "samples": len(values),
                        "mean_value": _number(statistics.fmean(values)),
                        "peak_value": _number(max(values)),
                    }
                )
            start = None
            values = []
    if start is not None and samples:
        end = samples[-1]
        duration = end.time - start.time
        if duration >= minimum_duration:
            spans.append(
                {
                    "start": _number(start.time),
                    "end": _number(end.time),
                    "duration": _number(duration),
                    "start_frame": start.frame,
                    "end_frame": end.frame,
                    "samples": len(values),
                    "mean_value": _number(statistics.fmean(values)),
                    "peak_value": _number(max(values)),
                    "open_at_end": True,
                }
            )
    return spans


def button_spans(
    session: Session, button: str, minimum_duration: float = 0.0
) -> list[dict[str, Any]]:
    return extract_spans(
        session.samples,
        lambda sample: (button in sample.buttons, 1.0),
        minimum_duration,
    )


def control_state(
    control: str, threshold: float
) -> tuple[str, Callable[[Sample], tuple[bool, float]]]:
    if control.endswith("+") and len(control) > 1:
        axis = control[:-1]
        return (
            "axis",
            lambda sample: (
                sample.axes.get(axis, 0.0) >= threshold,
                max(sample.axes.get(axis, 0.0), 0.0),
            ),
        )
    if control.endswith("-") and len(control) > 1:
        axis = control[:-1]
        return (
            "axis",
            lambda sample: (
                sample.axes.get(axis, 0.0) <= -threshold,
                max(-sample.axes.get(axis, 0.0), 0.0),
            ),
        )
    return (
        "button",
        lambda sample: (control in sample.buttons, 1.0),
    )


def trim_samples(
    samples: Sequence[Sample], start: float | None, end: float | None
) -> list[Sample]:
    return [
        sample
        for sample in samples
        if (start is None or sample.time >= start)
        and (end is None or sample.time <= end)
    ]


def discovered_controls(session: Session, threshold: float) -> list[str]:
    buttons = sorted(
        {button for sample in session.samples for button in sample.buttons}
    )
    axis_names = sorted({name for sample in session.samples for name in sample.axes})
    axes: list[str] = []
    for name in axis_names:
        values = [sample.axes.get(name, 0.0) for sample in session.samples]
        if max(values, default=0.0) >= threshold:
            axes.append(f"{name}+")
        if min(values, default=0.0) <= -threshold:
            axes.append(f"{name}-")
    return buttons + axes


def cadence_for_window(
    spans: Sequence[dict[str, Any]], label: str, start: float, end: float
) -> dict[str, Any]:
    selected = [span for span in spans if start <= float(span["start"]) <= end]
    starts = [float(span["start"]) for span in selected]
    holds = [float(span["duration"]) for span in selected]
    intervals = [later - earlier for earlier, later in zip(starts, starts[1:])]
    mean_interval = statistics.fmean(intervals) if intervals else None
    return {
        "label": label,
        "start": start,
        "end": end,
        "presses": len(selected),
        "first_press": _number(starts[0]) if starts else None,
        "last_press": _number(starts[-1]) if starts else None,
        "mean_interval": _number(mean_interval),
        "median_interval": _number(statistics.median(intervals)) if intervals else None,
        "frequency_hz": _number(1.0 / mean_interval)
        if mean_interval and mean_interval > 0
        else None,
        "mean_hold": _number(statistics.fmean(holds)) if holds else None,
        "median_hold": _number(statistics.median(holds)) if holds else None,
        "minimum_hold": _number(min(holds)) if holds else None,
        "maximum_hold": _number(max(holds)) if holds else None,
    }


def device_coverage(session: Session) -> list[dict[str, Any]]:
    """Report each observation layer independently, including events outside video."""
    path = session.directory / "input-devices.json"
    metadata = {d["device_id"]: d for d in (_read_json(path) if path.exists() else [])}
    tracks: dict[str, dict[str, Any]] = {}
    def track(device_id: str) -> dict[str, Any]:
        if device_id not in tracks:
            device = metadata.get(device_id, {})
            tracks[device_id] = {"device_id": device_id, "name": device.get("name"),
                "source": device.get("source", "unknown"), "port": device.get("port"),
                "canonical_frames": 0, "active_frames": 0, "raw_events": 0,
                "raw_key_presses": 0, "first_boottime_us": None, "last_boottime_us": None,
                "raw_axes": {}, "lifecycle": []}
        return tracks[device_id]
    for device_id in metadata:
        track(device_id)
    for frame in session.frames:
        for pad in (frame.get("raw_host") or {}).get("gamepads", []):
            row = track(pad["device_id"])
            row["port"] = pad.get("port")
            row["canonical_frames"] += 1
            row["active_frames"] += bool(pad.get("buttons") or any(
                abs(value) > 0.15 for value in pad.get("axes", {}).values()))
    raw = session.directory / "input-events.jsonl"
    if raw.exists():
        with raw.open() as source:
            for line in source:
                if not line.strip():
                    continue
                event = json.loads(line)
                row = track(event["device_id"])
                row["raw_events"] += 1
                row["raw_key_presses"] += event.get("event_type") == 1 and event.get("value") == 1
                time = event["boottime_us"]
                row["first_boottime_us"] = min(time, row["first_boottime_us"] or time)
                row["last_boottime_us"] = max(time, row["last_boottime_us"] or time)
                if event.get("lifecycle"):
                    row["lifecycle"].append({"boottime_us": time, "event": event["lifecycle"]})
                if event.get("event_type") == 3:
                    bounds = row["raw_axes"].setdefault(str(event["code"]), [event["value"], event["value"]])
                    bounds[0] = min(bounds[0], event["value"])
                    bounds[1] = max(bounds[1], event["value"])
    return [tracks[key] for key in sorted(tracks)]


def mapping_coverage(directory: Path) -> dict[str, Any]:
    path = directory / "controller-map.json"
    layouts = (_read_json(path).get("layouts") or []) if path.exists() else []
    return {"status": "partial" if not layouts or any(not x.get("bindings") for x in layouts) else "parsed_layouts",
        "note": "Parsed layouts do not prove the active physical-to-virtual mapping.",
        "layouts": [{"controller": x.get("controller"), "controller_type": x.get("controller_type"),
            "bindings": len(x.get("bindings") or []), "source_file": x.get("source_file")} for x in layouts]}


def canonical_transitions(frames: list[dict[str, Any]]) -> list[dict[str, Any]]:
    transitions = []
    previous = None
    for frame in frames:
        state = (frame.get("state"), frame.get("raw_host"))
        if previous != state:
            transitions.append({key: frame.get(key) for key in ("frame", "elapsed_us", "state", "raw_host")})
        previous = state
    return transitions


def load_audit_session(value: str | Path, port: int = 0, device_id: str | None = None) -> Session:
    directory, manifest_path, input_path = resolve_session_path(value)
    if input_path.is_file() and _read_json(input_path):
        return load_session(value, port, device_id)
    capture_path = directory / "capture.json"
    capture = _read_json(capture_path) if capture_path.is_file() else {}
    manifest = _read_json(manifest_path) if manifest_path.is_file() else {
        "core": {"name": f"Steam game {capture.get('game_id', 'unknown')}"},
        "external_capture": {"game_id": capture.get("game_id"), "recording_id": capture.get("recording_id"), "status": "partial"}}
    return Session(directory, manifest, [], [], device_id)


def audit_session(session: Session, game_id: str, probe_media: bool = False, recorder: str | None = None) -> dict[str, Any]:
    import hashlib
    import subprocess
    external = session.manifest.get("external_capture") or {}
    if str(external.get("game_id")) != game_id:
        raise SessionError("session game ID does not match the explicit audit target")
    report = session_summary(session)
    report.update(schema_version=1, game_id=game_id, findings=[], media=None)
    findings = report["findings"]
    if not session.frames:
        findings.append("canonical_input_missing")
    source = external.get("source_video")
    if not source:
        findings.append("missing_source_video")
    else:
        source = Path(source)
        if source.name == "session.mpd" and source.parent.name != session.session_id:
            findings.append("wrong_recording_segment")
        if external.get("clip_id") and source.name == "session.mpd" and source.parent.parent.parent.name != external["clip_id"]:
            findings.append("wrong_clip_identity")
        if not source.is_file():
            findings.append("source_media_unavailable_on_this_host")
    if not report["frame_count_matches"]:
        findings.append("manifest_input_frame_count_mismatch")
    if external.get("status") != "complete":
        findings.append("capture_not_complete")
    if report["mapping"]["status"] == "partial":
        findings.append("partial_controller_mapping")
    clock = session.directory / "archive-clock.json"
    if not clock.is_file():
        findings.append("archive_clock_mapping_unverified")
    else:
        receipt = _read_json(clock)
        if (receipt.get("schema_version") != 1 or receipt.get("recording_id") != session.session_id
            or receipt.get("method") != "whole_segment_zero_origin"
            or receipt.get("first_frame_boottime_us") != (external.get("video_clock") or {}).get("video_pts_zero_boottime_us")):
            findings.append("archive_clock_receipt_mismatch")
        if source and source.is_file() and source.name == "session.mpd":
            clip_path = source.parent.parent.parent / "clip.pb"
            if not clip_path.is_file() or hashlib.sha256(clip_path.read_bytes()).hexdigest() != receipt.get("clip_sha256"):
                findings.append("clip_metadata_changed_or_missing")
    media_receipt_path = session.directory / "media-validation.json"
    if media_receipt_path.is_file():
        media_receipt = _read_json(media_receipt_path)
        pts = media_receipt.get("frame_pts_us") or []
        if not pts or [frame.get("elapsed_us") for frame in session.frames] != [value - pts[0] for value in pts]:
            findings.append("validated_media_input_timeline_mismatch")
        if source and source.is_file():
            for name, expected in media_receipt.get("source_sha256", {}).items():
                if Path(name).name != name:
                    findings.append("invalid_media_receipt_path")
                    continue
                path = source.parent / name
                if not path.is_file():
                    findings.append("validated_media_file_missing")
                    continue
                digest = hashlib.sha256()
                with path.open("rb") as handle:
                    for chunk in iter(lambda: handle.read(65536), b""):
                        digest.update(chunk)
                if digest.hexdigest() != expected:
                    findings.append("validated_media_file_changed")
    else:
        findings.append("media_validation_receipt_missing")
    if session.manifest.get("dropped_frames"):
        findings.append("dropped_frames")
    transitions_path = session.directory / (session.manifest.get("input_transitions") or "input-transitions.jsonl")
    report["canonical_transition_count"] = len(canonical_transitions(session.frames))
    if transitions_path.is_file():
        saved = [json.loads(line) for line in transitions_path.read_text().splitlines() if line.strip()]
        expected = canonical_transitions(session.frames)
        # Compare parsed objects; JSON field ordering is immaterial.
        report["canonical_transitions_match"] = saved == expected
        if saved != expected:
            findings.append("canonical_transitions_mismatch")
    else:
        report["canonical_transitions_match"] = None
        findings.append("transition_index_missing")
    if probe_media and source and source.is_file() and recorder:
        try:
            process = subprocess.run([recorder, "validate-media", "--source", str(source)],
                capture_output=True, text=True, timeout=240)
            if process.returncode:
                report["media"] = {"error": process.stderr.strip(), "exit_code": process.returncode}
                findings.append("strict_media_validation_failed")
            else:
                validated = json.loads(process.stdout)
                report["media"] = {"validation_method": validated["validation_method"],
                    "decoded_frames": len(validated["frame_pts_us"]), "packets": validated["packet_count"],
                    "audio": validated["has_audio"], "audio_frames_without_pts": validated["audio_frames_without_pts"],
                    "frame_end_pts_us": validated["frame_end_pts_us"], "declared_duration_us": validated["declared_duration_us"]}
                if len(validated["frame_pts_us"]) != len(session.frames):
                    findings.append("decoded_input_frame_count_mismatch")
                presence = ((session.manifest.get("track_alignment") or {}).get("game_audio") or {}).get("presence")
                if validated["has_audio"] and presence in ("unavailable", "not_captured"):
                    findings.append("audio_presence_metadata_mismatch")
        except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError) as error:
            report["media"] = {"error": str(error)}
            findings.append("media_probe_failed")
    elif probe_media and source and source.is_file():
        command = ["ffprobe", "-v", "warning", "-count_frames", "-count_packets",
            "-show_entries", "stream=codec_type,width,height,nb_read_frames,nb_read_packets,start_time,duration",
            "-of", "json", str(source)]
        try:
            process = subprocess.run(command, capture_output=True, text=True, timeout=120)
            media = json.loads(process.stdout or "{}")
            report["media"] = {"streams": media.get("streams", []), "diagnostics": process.stderr.strip(),
                "exit_code": process.returncode}
            if process.returncode or process.stderr.strip():
                findings.append("media_decode_diagnostics")
            videos = [stream for stream in media.get("streams", []) if stream.get("codec_type") == "video"]
            if len(videos) != 1:
                findings.append("missing_or_ambiguous_video_stream")
            else:
                stream = videos[0]
                counts = (stream.get("nb_read_frames"), stream.get("nb_read_packets"))
                if counts[0] in (None, "N/A") or counts[0] != counts[1]:
                    findings.append("decoded_packet_frame_count_mismatch")
                if counts[0] != str(len(session.frames)):
                    findings.append("decoded_input_frame_count_mismatch")
            audio = any(stream.get("codec_type") == "audio" for stream in media.get("streams", []))
            presence = ((session.manifest.get("track_alignment") or {}).get("game_audio") or {}).get("presence")
            if audio and presence in ("unavailable", "not_captured"):
                findings.append("audio_presence_metadata_mismatch")
        except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError) as error:
            report["media"] = {"error": str(error)}
            findings.append("media_probe_failed")
    elif not probe_media:
        findings.append("media_decode_not_checked")
    transcript_name = external.get("audio_transcript_json")
    transcript_status = external.get("audio_transcription_status")
    if isinstance(transcript_status, dict):
        transcript_status = transcript_status.get("state")
    if transcript_status == "complete":
        if not transcript_name or not (session.directory / transcript_name).is_file():
            findings.append("completed_transcript_artifact_missing")
        # Completion of inference is not evidence that narration spans the video.
        report["transcript_coverage"] = "not_proven_by_job_completion"
        if "wrong_recording_segment" in findings:
            findings.append("transcript_source_identity_untrusted")
    report["source_hashes"] = {name: hashlib.sha256((session.directory / name).read_bytes()).hexdigest()
        for name in ("manifest.json", "capture.json", "input.json", "input-events.jsonl", "controller-map.json")
        if (session.directory / name).is_file()}
    report["healthy"] = not findings
    return report


def scoped_latest(root: str, game_id: str) -> Path:
    matches = []
    for path in Path(root).expanduser().glob("*/manifest.json"):
        manifest = _read_json(path)
        if str((manifest.get("external_capture") or {}).get("game_id")) == game_id:
            matches.append((float((manifest.get("timing") or {}).get("start_timestamp") or 0), path.parent))
    if not matches:
        raise SessionError(f"no sessions for game ID {game_id}")
    return max(matches)[1]


def session_summary(session: Session) -> dict[str, Any]:
    times = [sample.time for sample in session.samples]
    intervals = [later - earlier for earlier, later in zip(times, times[1:])]
    buttons = sorted(
        {button for sample in session.samples for button in sample.buttons}
    )
    button_data: dict[str, Any] = {}
    for button in buttons:
        spans = button_spans(session, button)
        holds = [float(span["duration"]) for span in spans]
        button_data[button] = {
            "presses": len(spans),
            "median_hold": _number(statistics.median(holds)) if holds else None,
            "total_held": _number(sum(holds)),
        }

    axis_names = sorted({name for sample in session.samples for name in sample.axes})
    axis_data: dict[str, Any] = {}
    for name in axis_names:
        values = [sample.axes.get(name, 0.0) for sample in session.samples]
        axis_data[name] = {
            "minimum": _number(min(values)),
            "maximum": _number(max(values)),
            "mean_absolute": _number(statistics.fmean(abs(value) for value in values)),
        }

    external = session.manifest.get("external_capture") or {}
    manifest_frames = session.manifest.get("frame_count")
    video_name = session.manifest.get("video")
    video_path = session.directory / video_name if isinstance(video_name, str) else None
    available_device_ids = sorted(
        {
            str(gamepad["device_id"])
            for frame in session.frames
            for gamepad in ((frame.get("raw_host") or {}).get("gamepads") or [])
            if gamepad.get("device_id") is not None
        }
    )
    return {
        "session_id": session.session_id,
        "game": session.game,
        "game_id": external.get("game_id"),
        "device_coverage": device_coverage(session),
        "mapping": mapping_coverage(session.directory),
        "selected_track_representative": "not_established",
        "directory": str(session.directory),
        "device_id": session.device_id,
        "available_device_ids": available_device_ids,
        "status": external.get("status"),
        "frames": len(session.samples),
        "manifest_frames": manifest_frames,
        "frame_count_matches": manifest_frames in (None, len(session.samples)),
        "duration_seconds": _number(session.duration),
        "dropped_frames": session.manifest.get("dropped_frames"),
        "sampling": {
            "median_interval_ms": _number(
                statistics.median(intervals) * 1000.0 if intervals else None,
                3,
            ),
            "p95_interval_ms": _number(
                (_percentile(intervals, 0.95) or 0.0) * 1000.0 if intervals else None,
                3,
            ),
            "minimum_interval_ms": _number(
                min(intervals) * 1000.0 if intervals else None, 3
            ),
            "maximum_interval_ms": _number(
                max(intervals) * 1000.0 if intervals else None, 3
            ),
            "monotonic": all(delta >= 0 for delta in intervals),
        },
        "video": {
            "path": str(video_path) if video_path else None,
            "exists": video_path.exists() if video_path else False,
            "bytes": video_path.stat().st_size
            if video_path and video_path.exists()
            else None,
        },
        "buttons": button_data,
        "axes": axis_data,
    }


def print_summary_text(data: dict[str, Any]) -> None:
    print(f"Session: {data['session_id']}")
    print(f"Game: {data['game']}")
    print(f"Game ID: {data['game_id']}")
    print("Device coverage (separate observation layers; raw events include pre-roll):")
    print("  Device | Source | Port | Frames/active | Raw events/key presses | Raw span (s)")
    for row in data["device_coverage"]:
        span = ((row["last_boottime_us"] or 0) - (row["first_boottime_us"] or 0)) / 1e6
        print(f"  {row['device_id']} | {row['source']} | {row['port']} | "
              f"{row['canonical_frames']}/{row['active_frames']} | "
              f"{row['raw_events']}/{row['raw_key_presses']} | {span:.3f}")
    if len(data["device_coverage"]) > 1:
        print("WARNING: selected-track activity is not representative of every device; shared ports do not establish mappings.")
    print(f"Controller mapping: {data['mapping']['status']}")
    print(f"Input track: {data['device_id'] or 'first matching port'}")
    print(f"Available tracks: {', '.join(data['available_device_ids']) or 'none'}")
    print(
        f"Status: {data['status'] or 'unknown'}; "
        f"frames: {data['frames']}; duration: {data['duration_seconds']:.3f}s"
    )
    sampling = data["sampling"]
    print(
        "Sampling: "
        f"median {sampling['median_interval_ms']}ms; "
        f"p95 {sampling['p95_interval_ms']}ms; "
        f"range {sampling['minimum_interval_ms']}-"
        f"{sampling['maximum_interval_ms']}ms"
    )
    print(
        f"Frame count matches manifest: {data['frame_count_matches']}; "
        f"dropped frames: {data['dropped_frames']}"
    )
    video = data["video"]
    print(f"Video: {video['path']} ({'present' if video['exists'] else 'missing'})")
    print("Buttons:")
    for name, values in data["buttons"].items():
        print(
            f"  {name:16s} presses={values['presses']:4d} "
            f"median_hold={values['median_hold']:.3f}s "
            f"total={values['total_held']:.3f}s"
        )
    print("Axes:")
    for name, values in data["axes"].items():
        print(
            f"  {name:16s} min={values['minimum']:+.3f} "
            f"max={values['maximum']:+.3f} "
            f"mean_abs={values['mean_absolute']:.3f}"
        )


def parse_window(value: str) -> tuple[str, float, float]:
    parts = value.rsplit(":", 2)
    if len(parts) != 3:
        raise argparse.ArgumentTypeError("window must be LABEL:START:END")
    label, start, end = parts
    try:
        start_value = float(start)
        end_value = float(end)
    except ValueError as error:
        raise argparse.ArgumentTypeError("window times must be numbers") from error
    if end_value <= start_value:
        raise argparse.ArgumentTypeError("window END must be greater than START")
    return label, start_value, end_value


def emit(value: Any, as_json: bool) -> None:
    if as_json:
        json.dump(value, sys.stdout, indent=2)
        print()


def add_common(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("session", help="session directory or a file within it")
    parser.add_argument("--port", type=int, default=0, help="gamepad port (default: 0)")
    parser.add_argument(
        "--device",
        dest="device_id",
        help="exact gamepad device_id; overrides --port when tracks share a port",
    )
    parser.add_argument(
        "--json", action="store_true", help="emit machine-readable JSON"
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Measure frame-aligned controller input in a RetroFeel session."
    )
    commands = parser.add_subparsers(dest="command", required=True)

    summary = commands.add_parser("summary", help="validate and summarize a session")
    add_common(summary)

    audit = commands.add_parser("audit", help="read-only capture health audit; exits 1 for findings")
    add_common(audit)
    audit.add_argument("--game-id", required=True, help="exact Steam game ID")
    audit.add_argument("--latest", action="store_true", help="session argument is a root; select latest only within game ID")
    audit.add_argument("--recorder", help="recorder executable with validate-media; decode exact declared fragments")
    audit.add_argument("--probe-media", action="store_true", help="decode referenced media on this host (up to 120 seconds)")

    events = commands.add_parser("events", help="list button press/release edges")
    add_common(events)
    events.add_argument(
        "--button", action="append", help="button to include; repeatable"
    )
    events.add_argument("--start", type=float)
    events.add_argument("--end", type=float)

    runs = commands.add_parser("runs", help="list digital or analog held runs")
    add_common(runs)
    runs.add_argument(
        "--control",
        action="append",
        help="button or signed axis such as DPadRight or LeftStickX+; repeatable",
    )
    runs.add_argument("--threshold", type=float, default=0.5)
    runs.add_argument("--min-duration", type=float, default=0.0)
    runs.add_argument("--start", type=float)
    runs.add_argument("--end", type=float)

    cadence = commands.add_parser(
        "cadence", help="compare button cadence and hold time in labeled windows"
    )
    add_common(cadence)
    cadence.add_argument("--button", required=True)
    cadence.add_argument(
        "--window",
        action="append",
        type=parse_window,
        metavar="LABEL:START:END",
        help="repeat to compare intervals; defaults to the entire session",
    )
    return parser


def command_events(session: Session, args: argparse.Namespace) -> list[dict[str, Any]]:
    selected = set(args.button or [])
    events: list[dict[str, Any]] = []
    previous: frozenset[str] = frozenset()
    if args.start is not None:
        prior = [sample for sample in session.samples if sample.time < args.start]
        if prior:
            previous = prior[-1].buttons
    for sample in trim_samples(session.samples, args.start, args.end):
        names = (sample.buttons | previous) if not selected else selected
        for name in sorted(names):
            was_down = name in previous
            is_down = name in sample.buttons
            if was_down != is_down:
                events.append(
                    {
                        "time": _number(sample.time),
                        "frame": sample.frame,
                        "button": name,
                        "event": "press" if is_down else "release",
                    }
                )
        previous = sample.buttons
    return events


def command_runs(session: Session, args: argparse.Namespace) -> list[dict[str, Any]]:
    samples = trim_samples(session.samples, args.start, args.end)
    controls = args.control or discovered_controls(session, args.threshold)
    result: list[dict[str, Any]] = []
    for control in controls:
        kind, predicate = control_state(control, args.threshold)
        for span in extract_spans(samples, predicate, args.min_duration):
            result.append({"control": control, "kind": kind, **span})
    result.sort(key=lambda item: (float(item["start"]), str(item["control"])))
    return result


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        if args.command == "audit" and args.latest:
            args.session = scoped_latest(args.session, args.game_id)
        loader = load_audit_session if args.command == "audit" else load_session
        session = loader(args.session, args.port, args.device_id)
        if args.command == "audit":
            data = audit_session(session, args.game_id, args.probe_media, args.recorder)
            if args.json:
                emit(data, True)
            else:
                print_summary_text(data)
                print("Findings: " + (", ".join(data["findings"]) or "none"))
            return 0 if data["healthy"] else 1
        if args.command == "summary":
            data = session_summary(session)
            if args.json:
                emit(data, True)
            else:
                print_summary_text(data)
        elif args.command == "events":
            data = command_events(session, args)
            if args.json:
                emit(data, True)
            else:
                for event in data:
                    print(
                        f"{event['time']:10.3f}s  frame {event['frame']:6d}  "
                        f"{event['event']:7s}  {event['button']}"
                    )
        elif args.command == "runs":
            data = command_runs(session, args)
            if args.json:
                emit(data, True)
            else:
                for run in data:
                    print(
                        f"{run['start']:10.3f}-{run['end']:10.3f}s  "
                        f"{run['duration']:8.3f}s  {run['control']:18s} "
                        f"mean={run['mean_value']:.3f} peak={run['peak_value']:.3f}"
                    )
        elif args.command == "cadence":
            spans = button_spans(session, args.button)
            windows = args.window or [("all", 0.0, session.duration)]
            data = {
                "session_id": session.session_id,
                "button": args.button,
                "windows": [
                    cadence_for_window(spans, label, start, end)
                    for label, start, end in windows
                ],
            }
            if args.json:
                emit(data, True)
            else:
                print(f"Session: {session.session_id}; button: {args.button}")
                for window in data["windows"]:
                    print(
                        f"{window['label']:16s} {window['start']:8.3f}-"
                        f"{window['end']:8.3f}s presses={window['presses']:3d} "
                        f"mean_gap={window['mean_interval']!s:>8} "
                        f"median_gap={window['median_interval']!s:>8} "
                        f"rate={window['frequency_hz']!s:>8}Hz "
                        f"median_hold={window['median_hold']!s:>8}s"
                    )
        return 0
    except (SessionError, OSError, ValueError) as error:
        parser.error(str(error))
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
