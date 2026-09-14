#!/usr/bin/env python3
"""Local-first validation and evidence extraction for RetroFeel sessions."""

import argparse
import json
import math
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


def resolve_session(value: str) -> tuple[Path, Path | None]:
    path = Path(value).expanduser().resolve()
    if path.is_dir():
        return path, (path / "manifest.json" if (path / "manifest.json").is_file() else None)
    if path.name in {"manifest.json", "manifest.ron", "input.json", "video.mkv"}:
        return path.parent, path if path.name.startswith("manifest") else path.parent / "manifest.json"
    raise SystemExit(f"not a RetroFeel session, manifest, input log, or video: {path}")


def load_manifest(session: Path, hint: Path | None) -> dict:
    path = hint if hint and hint.name == "manifest.json" else session / "manifest.json"
    if not path.is_file():
        return {}
    with path.open() as source:
        return json.load(source)


def artifact(session: Path, manifest: dict, key: str, fallback: str) -> Path:
    raw = manifest.get(key) or fallback
    path = Path(raw)
    return path if path.is_absolute() else session / path


def command(args: list[str], text: bool = True) -> str:
    try:
        return subprocess.check_output(args, text=text, stderr=subprocess.PIPE)
    except FileNotFoundError:
        raise SystemExit(f"required executable is not on PATH: {args[0]}")
    except subprocess.CalledProcessError as error:
        message = error.stderr if text else error.stderr.decode(errors="replace")
        raise SystemExit(f"command failed ({' '.join(args)}): {message.strip()}")


def ffprobe_frames(video: Path) -> list[float]:
    raw = command([
        "ffprobe", "-v", "error", "-select_streams", "v:0", "-show_frames",
        "-show_entries", "frame=best_effort_timestamp_time,pkt_duration_time",
        "-of", "json", str(video),
    ])
    frames = json.loads(raw).get("frames", [])
    return [float(frame.get("best_effort_timestamp_time", 0.0)) for frame in frames]


def input_frames(path: Path) -> list[dict]:
    if not path.is_file():
        return []
    with path.open() as source:
        return json.load(source)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def transitions(frames: list[dict]) -> list[dict]:
    result = []
    before = object()
    for frame in frames:
        held = {"state": frame.get("state"), "raw_host": frame.get("raw_host")}
        if held != before:
            result.append({
                "frame": frame.get("frame"),
                **({"elapsed_us": frame["elapsed_us"]} if frame.get("elapsed_us") is not None else {}),
                **held,
            })
            before = held
    return result


def jsonl(items: list[dict]) -> str:
    # Match serde's struct field order exactly. Sorting keys makes equivalent
    # transitions look different byte-for-byte from RetroFeel's deterministic
    # `input-transitions.jsonl` artifact.
    return "".join(json.dumps(item, separators=(",", ":")) + "\n" for item in items)


def validate(session: Path, manifest: dict) -> dict:
    input_path = artifact(session, manifest, "input_log", "input.json")
    video_path = artifact(session, manifest, "video", "video.mkv")
    map_info = manifest.get("frame_map") or {}
    map_path = artifact(session, map_info, "path", "frame-map.json") if map_info else None
    transition_path = artifact(session, manifest, "input_transitions", "input-transitions.jsonl")
    frames = input_frames(input_path)
    result = {
        "session": str(session),
        "provenance": manifest.get("capture_provenance"),
        "video_timing": manifest.get("video_timing"),
        "track_alignment": manifest.get("track_alignment"),
        "manifest_frame_count": manifest.get("frame_count"),
        "input_frames": len(frames),
        "video": str(video_path) if video_path.is_file() else None,
        "checks": [],
    }
    result["checks"].append({"name": "manifest/input count", "ok": manifest.get("frame_count") in (None, len(frames)), "actual": len(frames)})
    elapsed = [frame.get("elapsed_us") for frame in frames if frame.get("elapsed_us") is not None]
    result["checks"].append({"name": "input elapsed monotonic", "ok": elapsed == sorted(elapsed), "samples": len(elapsed)})
    regenerated = jsonl(transitions(frames))
    transition_ok = True
    if transition_path.is_file():
        transition_ok = transition_path.read_text() == regenerated
    result["checks"].append({"name": "deterministic input transitions", "ok": transition_ok, "path": str(transition_path)})
    if video_path.is_file():
        pts = ffprobe_frames(video_path)
        result["decoded_video_frames"] = len(pts)
        result["checks"].append({"name": "decoded video/input count", "ok": len(pts) == len(frames), "video": len(pts), "input": len(frames)})
        timing = (manifest.get("video_timing") or {}).get("kind")
        if timing == "cfr_resampled_sck" and elapsed:
            result["checks"].append({
                "name": "clean CFR master path",
                "ok": video_path.name == "video.mkv",
                "path": str(video_path),
            })
            fps = (manifest.get("video_timing") or {}).get("output_fps") or manifest.get("timing", {}).get("fps", 60)
            tick_us = math.ceil(1_000_000 / max(float(fps), 1.0))
            deltas = [abs(round(pts[index] * 1_000_000) - elapsed[index]) for index in range(min(len(pts), len(elapsed)))]
            result["checks"].append({"name": "CFR decoded PTS/input elapsed", "ok": bool(deltas) and max(deltas) <= tick_us, "max_delta_us": max(deltas, default=None), "tolerance_us": tick_us})
    if map_path and map_path.is_file():
        frame_map = json.loads(map_path.read_text())
        ok = len(frame_map) == len(frames) and all(
            entry.get("encoded_frame") == frame.get("frame") and entry.get("encoded_pts_us") == frame.get("elapsed_us")
            for entry, frame in zip(frame_map, frames)
        )
        result["checks"].append({"name": "frame map/input consistency", "ok": ok, "entries": len(frame_map)})
        result["frame_map_quality"] = {
            "source_frames_discarded": map_info.get("source_frames_discarded", 0),
            "grid_duplicates": map_info.get("grid_duplicates", 0),
            "writer_dupes": sum(bool(entry.get("writer_dupe")) for entry in frame_map),
        }
    transcript = artifact(session, manifest, "transcript_json", "transcript.json")
    result["transcript"] = str(transcript) if transcript.is_file() else None
    return result


def safe_output(path: str | None, name: str, session: Path) -> Path:
    if path:
        destination = Path(path).expanduser().resolve()
        if destination == session or session in destination.parents:
            raise SystemExit(
                f"derived output must be outside the source session: {destination}"
            )
        destination.parent.mkdir(parents=True, exist_ok=True)
        return destination
    root = Path(tempfile.mkdtemp(prefix="retrofeel-analysis-"))
    return root / name


def transition_spans(frames: list[dict]) -> list[dict]:
    changes = transitions(frames)
    spans = []
    for index, change in enumerate(changes):
        following = changes[index + 1] if index + 1 < len(changes) else None
        start_us = change.get("elapsed_us")
        end_us = following.get("elapsed_us") if following else None
        spans.append({
            "start_frame": change.get("frame"),
            "end_frame_exclusive": following.get("frame") if following else None,
            "start_us": start_us,
            "end_us": end_us,
            "state": change.get("state"),
            "raw_host": change.get("raw_host"),
        })
    return spans


def transcript_evidence(session: Path, manifest: dict) -> list[dict]:
    transcript = artifact(session, manifest, "transcript_json", "transcript.json")
    if not transcript.is_file():
        return []
    try:
        document = json.loads(transcript.read_text())
    except json.JSONDecodeError:
        return []
    return [
        {
            "start_seconds": segment.get("start_seconds"),
            "end_seconds": segment.get("end_seconds"),
            "text": segment.get("text", ""),
        }
        for segment in document.get("segments", [])
    ]


def require_video(session: Path, manifest: dict) -> Path:
    video = artifact(session, manifest, "video", "video.mkv")
    if not video.is_file():
        raise SystemExit(f"clean video.mkv is unavailable: {video}")
    return video


def make_contact_sheet(video: Path, start: float, end: float, every: float, output: Path) -> None:
    duration = max(end - start, every)
    fps = 1.0 / max(every, 0.001)
    command([
        "ffmpeg", "-y", "-ss", str(start), "-t", str(duration), "-i", str(video),
        "-vf", f"fps={fps},scale=320:-2,tile=4x4", "-frames:v", "1", str(output),
    ])


def extract_frame(video: Path, at: float, output: Path) -> None:
    command(["ffmpeg", "-y", "-ss", str(at), "-i", str(video), "-frames:v", "1", str(output)])


def ass_time(seconds: float) -> str:
    centiseconds = max(round(seconds * 100), 0)
    hours, remainder = divmod(centiseconds, 360_000)
    minutes, remainder = divmod(remainder, 6_000)
    return f"{hours}:{minutes:02d}:{remainder // 100:02d}.{remainder % 100:02d}"


def ass_escape(text: str) -> str:
    return text.replace("\\", "\\\\").replace("{", "\\{").replace("}", "\\}").replace("\n", " ")


def render_overlay_video(session: Path, manifest: dict, output: Path) -> None:
    """Create a derivative, never modifying the clean master."""
    video = require_video(session, manifest)
    frames = input_frames(artifact(session, manifest, "input_log", "input.json"))
    spans = transition_spans(frames)
    ass = output.with_suffix(".overlay.ass")
    lines = [
        "[Script Info]",
        "ScriptType: v4.00+",
        "[V4+ Styles]",
        "Format: Name,Fontname,Fontsize,PrimaryColour,OutlineColour,BackColour,Bold,Italic,BorderStyle,Outline,Shadow,Alignment,MarginL,MarginR,MarginV,Encoding",
        "Style: Input,Arial,16,&H00E8F7FF,&H00101820,&H99000000,0,0,1,1,0,7,20,20,100,1",
        "[Events]",
        "Format: Layer,Start,End,Style,Name,MarginL,MarginR,MarginV,Effect,Text",
    ]
    duration = (frames[-1].get("elapsed_us") or 0) / 1_000_000 if frames else 0
    for span in spans:
        start = (span.get("start_us") or 0) / 1_000_000
        end = (span.get("end_us") or duration)
        raw = span.get("raw_host") or {}
        chips = list(raw.get("keyboard_keys") or [])[:3]
        chips += list(raw.get("gamepad_buttons") or [])[:2]
        label = "INPUT  " + (" · ".join(chips) if chips else "idle")
        lines.append(
            f"Dialogue: 0,{ass_time(start)},{ass_time(max(end, start + 0.01))},Input,,0,0,0,,{ass_escape(label)}"
        )
    ass.write_text("\n".join(lines) + "\n")
    fps = (manifest.get("video_timing") or {}).get("output_fps") or manifest.get("timing", {}).get("fps", 0)
    label = f"RETROFEEL DERIVED OVERLAY  ·  {fps:g} fps"
    mic = artifact(session, manifest, "mic_audio", "mic.wav")
    if mic.is_file():
        filters = (
            f"[1:a]showwaves=s=300x42:mode=line:colors=0x45D8FFFF[wave];"
            f"[0:v][wave]overlay=20:48,drawtext=text='{label}':x=20:y=16:fontsize=16:fontcolor=white,subtitles='{ass}'[out]"
        )
        args = ["ffmpeg", "-y", "-i", str(video), "-i", str(mic), "-filter_complex", filters,
                "-map", "[out]", "-map", "0:a?", "-c:v", "libx264", "-c:a", "copy", str(output)]
    else:
        filters = f"drawtext=text='{label}':x=20:y=16:fontsize=16:fontcolor=white,subtitles='{ass}'"
        args = ["ffmpeg", "-y", "-i", str(video), "-vf", filters, "-map", "0:v:0", "-map", "0:a?",
                "-c:v", "libx264", "-c:a", "copy", str(output)]
    command(args)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    for name in ("summary", "transitions"):
        item = sub.add_parser(name)
        item.add_argument("session")
        item.add_argument("--out")
    spans = sub.add_parser("transition-spans")
    spans.add_argument("session")
    spans.add_argument("--out")
    evidence = sub.add_parser("transcript-evidence")
    evidence.add_argument("session")
    evidence.add_argument("--out")
    sheet = sub.add_parser("contact-sheet")
    sheet.add_argument("session")
    sheet.add_argument("--start", type=float, default=0)
    sheet.add_argument("--end", type=float, required=True)
    sheet.add_argument("--every", type=float, default=5)
    sheet.add_argument("--out", required=True)
    frame = sub.add_parser("frame")
    frame.add_argument("session")
    frame.add_argument("--at", type=float, required=True)
    frame.add_argument("--out", required=True)
    pack = sub.add_parser("llm-pack")
    pack.add_argument("session")
    pack.add_argument("--out", required=True)
    pack.add_argument("--max-frames", type=int, default=12)
    overlay = sub.add_parser("overlay-video")
    overlay.add_argument("session")
    overlay.add_argument("--out", required=True)
    args = parser.parse_args()
    session, hint = resolve_session(args.session)
    manifest = load_manifest(session, hint)
    if args.action == "summary":
        report = validate(session, manifest)
        text = json.dumps(report, indent=2, sort_keys=True) + "\n"
        if args.out:
            output = safe_output(args.out, "summary.json", session)
            output.write_text(text)
            print(output)
        else:
            print(text, end="")
        return
    if args.action == "transitions":
        frames = input_frames(artifact(session, manifest, "input_log", "input.json"))
        text = jsonl(transitions(frames))
        if args.out:
            output = safe_output(args.out, "input-transitions.jsonl", session)
            output.write_text(text)
            print(output)
        else:
            print(text, end="")
        return
    if args.action == "transition-spans":
        frames = input_frames(artifact(session, manifest, "input_log", "input.json"))
        text = json.dumps(transition_spans(frames), indent=2, sort_keys=True) + "\n"
        if args.out:
            output = safe_output(args.out, "transition-spans.json", session)
            output.write_text(text)
            print(output)
        else:
            print(text, end="")
        return
    if args.action == "transcript-evidence":
        text = json.dumps(transcript_evidence(session, manifest), indent=2, sort_keys=True) + "\n"
        if args.out:
            output = safe_output(args.out, "transcript-evidence.json", session)
            output.write_text(text)
            print(output)
        else:
            print(text, end="")
        return
    video = require_video(session, manifest)
    if args.action == "contact-sheet":
        output = safe_output(args.out, "contact-sheet.png", session)
        make_contact_sheet(video, args.start, args.end, args.every, output)
        print(output)
        return
    if args.action == "frame":
        output = safe_output(args.out, "frame.png", session)
        extract_frame(video, args.at, output)
        print(output)
        return
    if args.action == "overlay-video":
        output = safe_output(args.out, "video-overlay.mkv", session)
        if output.exists():
            raise SystemExit(f"refusing to overwrite derived overlay video: {output}")
        render_overlay_video(session, manifest, output)
        print(output)
        return
    output = Path(args.out).expanduser().resolve()
    if output == session or session in output.parents:
        raise SystemExit(f"derived output must be outside the source session: {output}")
    if output.exists():
        raise SystemExit(f"refusing to overwrite LLM pack: {output}")
    output.mkdir(parents=True)
    report = validate(session, manifest)
    (output / "summary.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    frames = input_frames(artifact(session, manifest, "input_log", "input.json"))
    (output / "input-transitions.jsonl").write_text(jsonl(transitions(frames)))
    (output / "transition-spans.json").write_text(
        json.dumps(transition_spans(frames), indent=2, sort_keys=True) + "\n"
    )
    transcript = artifact(session, manifest, "transcript_json", "transcript.json")
    if transcript.is_file():
        shutil.copy2(transcript, output / "transcript.json")
        (output / "transcript-evidence.json").write_text(
            json.dumps(transcript_evidence(session, manifest), indent=2, sort_keys=True) + "\n"
        )
    map_info = manifest.get("frame_map") or {}
    if map_info:
        source = artifact(session, map_info, "path", "frame-map.json")
        if source.is_file():
            shutil.copy2(source, output / "frame-map.json")
    duration = max((frames[-1].get("elapsed_us") or 0) / 1_000_000 if frames else 0, 1)
    selected = min(max(args.max_frames, 0), 12)
    for index in range(selected):
        at = duration * index / max(selected - 1, 1)
        extract_frame(video, at, output / f"frame-{index:02d}-{at:.3f}s.png")
    print(output)


if __name__ == "__main__":
    main()
