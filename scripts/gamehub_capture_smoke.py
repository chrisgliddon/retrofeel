#!/usr/bin/env python3
"""Launch a GameHub title, record it with RetroFeel, and validate the session.

This is a macOS hardware smoke test. It drives only GameHub's visible UI,
never invokes GameHub's private runtime APIs, and never kills GameHub or the
game. The RetroFeel process started by this script is stopped after recording
finalization. Derived reports and images are written outside the source
session.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import queue
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path


@dataclass(frozen=True)
class Game:
    app_id: int
    title: str
    library_title: str
    process_names: tuple[str, ...]


def load_game_profile(path: Path) -> Game:
    """Load one explicit profile; no installed title is selected by default."""
    data = json.loads(path.read_text())
    required = {"app_id", "title", "library_title", "process_names"}
    if not isinstance(data, dict) or set(data) != required:
        raise ValueError("game profile must contain exactly app_id, title, library_title, process_names")
    if type(data["app_id"]) is not int or not 0 < data["app_id"] <= 2**32 - 1:
        raise ValueError("app_id must be a positive 32-bit integer")
    for key in ("title", "library_title"):
        if not isinstance(data[key], str) or not data[key].strip() or any(ord(c) < 32 for c in data[key]):
            raise ValueError(f"{key} must be nonempty text without control characters")
    names = data["process_names"]
    if not isinstance(names, list) or not names or any(
        not isinstance(name, str) or not name.strip() or name.startswith("-")
        or any(ord(c) < 32 for c in name) or "/" in name or "\\" in name
        for name in names
    ):
        raise ValueError("process_names must be a nonempty list of executable filenames")
    return Game(data["app_id"], data["title"], data["library_title"], tuple(names))


@dataclass(frozen=True)
class OcrWord:
    text: str
    left: int
    top: int
    width: int
    height: int
    line: tuple[int, int, int, int]


class FocusMonitor:
    """Record frontmost-app and target-window state throughout an attach."""

    def __init__(self, quartz, game_pid: int, path: Path):
        self.quartz = quartz
        self.game_pid = game_pid
        self.path = path
        self.started_at = time.monotonic()
        self.stop_event = threading.Event()
        self.lock = threading.Lock()
        self.phase = "before-retrofeel"
        self.retrofeel_pid: int | None = None
        self.thread = threading.Thread(target=self._run, name="focus-monitor", daemon=True)

    def start(self) -> None:
        self.thread.start()

    def set_retrofeel_pid(self, pid: int) -> None:
        with self.lock:
            self.retrofeel_pid = pid
            self.phase = "attaching"

    def set_phase(self, phase: str) -> None:
        with self.lock:
            self.phase = phase

    def stop(self) -> None:
        self.stop_event.set()
        self.thread.join(timeout=2)

    def _run(self) -> None:
        from AppKit import NSWorkspace
        from objc import autorelease_pool

        workspace = NSWorkspace.sharedWorkspace()
        with self.path.open("w") as output:
            while not self.stop_event.is_set():
                with autorelease_pool():
                    application = workspace.frontmostApplication()
                    windows = self.quartz.CGWindowListCopyWindowInfo(
                        self.quartz.kCGWindowListOptionOnScreenOnly,
                        self.quartz.kCGNullWindowID,
                    )
                    game_windows = [
                        window
                        for window in windows
                        if int(window.get("kCGWindowOwnerPID", -1)) == self.game_pid
                        and int(window.get("kCGWindowLayer", -1)) == 0
                    ]
                    with self.lock:
                        phase = self.phase
                        retrofeel_pid = self.retrofeel_pid
                    sample = {
                        "elapsed_ms": round((time.monotonic() - self.started_at) * 1000),
                        "phase": phase,
                        "frontmost_name": str(application.localizedName() or ""),
                        "frontmost_pid": int(application.processIdentifier()),
                        "frontmost_bundle_id": str(application.bundleIdentifier() or ""),
                        "game_pid": self.game_pid,
                        "game_window_visible": bool(game_windows),
                        "game_window_titles": [
                            str(window.get("kCGWindowName") or "") for window in game_windows
                        ],
                        "retrofeel_pid": retrofeel_pid,
                    }
                    output.write(json.dumps(sample, separators=(",", ":")) + "\n")
                    output.flush()
                self.stop_event.wait(0.1)


def focus_summary(path: Path) -> dict:
    samples = [json.loads(line) for line in path.read_text().splitlines() if line]
    retrofeel_frontmost = any(
        sample.get("retrofeel_pid") is not None
        and sample.get("frontmost_pid") == sample.get("retrofeel_pid")
        for sample in samples
    )
    game_frontmost_after_attach = any(
        sample.get("phase") == "recording"
        and sample.get("frontmost_pid") == sample.get("game_pid")
        for sample in samples
    )
    return {
        "path": str(path),
        "sample_count": len(samples),
        "retrofeel_became_frontmost": retrofeel_frontmost,
        "game_frontmost_during_recording": game_frontmost_after_attach,
    }


def normalized_words(value: str) -> list[str]:
    return re.findall(r"[a-z0-9]+", value.lower().replace("™", "").replace("®", ""))


def line_box(words: list[OcrWord], expected: str) -> tuple[float, float] | None:
    expected_words = normalized_words(expected)
    grouped: dict[tuple[int, int, int, int], list[OcrWord]] = {}
    for word in words:
        grouped.setdefault(word.line, []).append(word)
    for line in grouped.values():
        ordered = sorted(line, key=lambda word: word.left)
        actual = [token for word in ordered for token in normalized_words(word.text)]
        if not all(token in actual for token in expected_words):
            continue
        left = min(word.left for word in ordered)
        right = max(word.left + word.width for word in ordered)
        top = min(word.top for word in ordered)
        bottom = max(word.top + word.height for word in ordered)
        return ((left + right) / 2, (top + bottom) / 2)
    return None


def find_play_button(image) -> tuple[float, float] | None:
    import cv2

    height, width = image.shape[:2]
    mask = cv2.inRange(image, (220, 220, 220), (255, 255, 255))
    contours, _ = cv2.findContours(mask, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_SIMPLE)
    candidates = []
    for contour in contours:
        x, y, candidate_width, candidate_height = cv2.boundingRect(contour)
        aspect = candidate_width / max(candidate_height, 1)
        center_x = x + candidate_width / 2
        center_y = y + candidate_height / 2
        if not 0.07 <= candidate_width / width <= 0.16:
            continue
        if not 0.03 <= candidate_height / height <= 0.08:
            continue
        if not 2.2 <= aspect <= 4.5:
            continue
        if not 0.20 <= center_x / width <= 0.70:
            continue
        if not 0.35 <= center_y / height <= 0.80:
            continue
        candidates.append((candidate_width * candidate_height, center_x, center_y))
    if not candidates:
        return None
    _, center_x, center_y = max(candidates)
    return center_x, center_y


def require_command(name: str) -> str:
    path = shutil.which(name)
    if path is None:
        raise RuntimeError(f"required executable is not on PATH: {name}")
    return path


def run_checked(arguments: list[str], **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(arguments, check=True, text=True, **kwargs)


def import_macos_frameworks():
    if sys.platform != "darwin":
        raise RuntimeError("GameHub capture smoke tests require macOS")
    try:
        import Quartz
    except ImportError as error:
        raise RuntimeError("Python's PyObjC Quartz bindings are required") from error
    return Quartz


def activate_gamehub() -> None:
    subprocess.run(["open", "-a", "GameHub"], check=True)
    script = """
tell application "System Events"
  tell process "GameHub"
    set visible to true
    if (count windows) > 0 then
      set value of attribute "AXMinimized" of window 1 to false
      perform action "AXRaise" of window 1
    end if
    set frontmost to true
  end tell
end tell
"""
    run_checked(["osascript", "-e", script], stdout=subprocess.DEVNULL)


def activate_process(pid: int) -> None:
    from AppKit import NSApplicationActivateAllWindows, NSRunningApplication

    application = NSRunningApplication.runningApplicationWithProcessIdentifier_(pid)
    if application is None or not application.activateWithOptions_(
        NSApplicationActivateAllWindows
    ):
        raise RuntimeError(f"macOS refused to activate game PID {pid}")


def gamehub_window(quartz):
    windows = quartz.CGWindowListCopyWindowInfo(
        quartz.kCGWindowListOptionOnScreenOnly, quartz.kCGNullWindowID
    )
    candidates = [
        window
        for window in windows
        if window.get("kCGWindowOwnerName") == "GameHub"
        and window.get("kCGWindowLayer") == 0
        and window.get("kCGWindowAlpha", 1) > 0
    ]
    return max(candidates, key=lambda window: window.get("kCGWindowMemoryUsage", 0), default=None)


def wait_for_gamehub_window(quartz, timeout: float):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        window = gamehub_window(quartz)
        if window is not None:
            return window
        time.sleep(0.25)
    raise RuntimeError(f"GameHub did not expose a visible window within {timeout:g}s")


def capture_window(quartz, window, path: Path) -> tuple[int, int]:
    image = quartz.CGWindowListCreateImage(
        quartz.CGRectNull,
        quartz.kCGWindowListOptionIncludingWindow,
        window["kCGWindowNumber"],
        quartz.kCGWindowImageBoundsIgnoreFraming,
    )
    if image is None:
        raise RuntimeError("could not capture the GameHub window")
    encoded = os.fsencode(path)
    url = quartz.CFURLCreateFromFileSystemRepresentation(None, encoded, len(encoded), False)
    destination = quartz.CGImageDestinationCreateWithURL(url, "public.png", 1, None)
    quartz.CGImageDestinationAddImage(destination, image, None)
    if not quartz.CGImageDestinationFinalize(destination):
        raise RuntimeError(f"could not write GameHub screenshot: {path}")
    return quartz.CGImageGetWidth(image), quartz.CGImageGetHeight(image)


def ocr_words(path: Path, tesseract: str) -> list[OcrWord]:
    output = subprocess.check_output(
        [tesseract, str(path), "stdout", "--psm", "11", "tsv"],
        text=True,
        stderr=subprocess.DEVNULL,
    )
    words = []
    for row in csv.DictReader(output.splitlines(), delimiter="\t"):
        text = (row.get("text") or "").strip()
        if not text:
            continue
        try:
            confidence = float(row.get("conf", "-1"))
        except ValueError:
            continue
        if confidence < 20:
            continue
        words.append(
            OcrWord(
                text=text,
                left=int(row["left"]),
                top=int(row["top"]),
                width=int(row["width"]),
                height=int(row["height"]),
                line=(
                    int(row["page_num"]),
                    int(row["block_num"]),
                    int(row["par_num"]),
                    int(row["line_num"]),
                ),
            )
        )
    return words


def click_image_point(window, image_size: tuple[int, int], point: tuple[float, float]) -> None:
    bounds = window["kCGWindowBounds"]
    image_width, image_height = image_size
    screen_x = float(bounds["X"]) + point[0] * float(bounds["Width"]) / image_width
    screen_y = float(bounds["Y"]) + point[1] * float(bounds["Height"]) / image_height
    script = (
        'tell application "System Events" to tell process "GameHub" '
        f"to click at {{{round(screen_x)}, {round(screen_y)}}}"
    )
    run_checked(["osascript", "-e", script], stdout=subprocess.DEVNULL)


def navigate_to_game(
    quartz, game: Game, output: Path, tesseract: str, click_play: bool = True
) -> None:
    activate_gamehub()
    window = wait_for_gamehub_window(quartz, 15)
    initial_path = output / "gamehub-initial.png"
    image_size = capture_window(quartz, window, initial_path)

    # The four-square My Games icon is stable in GameHub's left navigation.
    click_image_point(window, image_size, (image_size[0] * 0.043, image_size[1] * 0.392))
    deadline = time.monotonic() + 15
    library_box = None
    while time.monotonic() < deadline:
        time.sleep(0.5)
        window = wait_for_gamehub_window(quartz, 2)
        library_path = output / "gamehub-library.png"
        image_size = capture_window(quartz, window, library_path)
        library_box = line_box(ocr_words(library_path, tesseract), game.library_title)
        if library_box is not None:
            break
    if library_box is None:
        raise RuntimeError(
            f"GameHub's installed library does not visibly contain {game.library_title!r}; "
            f"see {output / 'gamehub-library.png'}"
        )
    click_image_point(window, image_size, library_box)

    deadline = time.monotonic() + 15
    play_point = None
    while time.monotonic() < deadline:
        time.sleep(0.5)
        window = wait_for_gamehub_window(quartz, 2)
        detail_path = output / "gamehub-detail.png"
        image_size = capture_window(quartz, window, detail_path)
        words = ocr_words(detail_path, tesseract)
        if line_box(words, game.title) is None:
            continue
        import cv2

        play_point = find_play_button(cv2.imread(str(detail_path)))
        if play_point is not None:
            break
    if play_point is None:
        raise RuntimeError(
            f"GameHub opened {game.title!r}, but no Play button was detected; "
            f"see {output / 'gamehub-detail.png'}"
        )
    if click_play:
        click_image_point(window, image_size, play_point)


def process_for_game(game: Game) -> tuple[int, str] | None:
    result = subprocess.run(
        ["ps", "ax", "-o", "pid=,command="], capture_output=True, text=True, check=True
    )
    for line in result.stdout.splitlines():
        command = line.strip()
        if not any(name.casefold() in command.casefold() for name in game.process_names):
            continue
        match = re.match(r"(\d+)\s+(.*)", command)
        if match:
            return int(match.group(1)), match.group(2)
    return None


def wait_for_game_process(game: Game, timeout: float) -> tuple[int, str]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        process = process_for_game(game)
        if process is not None:
            return process
        time.sleep(0.5)
    raise RuntimeError(
        f"GameHub did not launch any of {', '.join(game.process_names)} within {timeout:g}s"
    )


def retrofeel_recordings_dir() -> Path:
    default_root = (
        Path.home()
        / "Library"
        / "Application Support"
        / "dev.retrofeel.retrofeel"
    )
    database = default_root / "retrofeel.db"
    if database.is_file():
        try:
            with sqlite3.connect(database) as connection:
                row = connection.execute(
                    "SELECT value_json FROM config WHERE key = 'paths'"
                ).fetchone()
            if row:
                configured = json.loads(row[0]).get("recordings")
                if configured:
                    return Path(configured).expanduser()
        except (sqlite3.Error, json.JSONDecodeError):
            pass
    return default_root / "recordings"


def session_dirs(recordings: Path) -> set[Path]:
    if not recordings.is_dir():
        return set()
    return {path.resolve() for path in recordings.glob("session-*") if path.is_dir()}


def pump_output(process: subprocess.Popen, log, messages: queue.Queue[str]) -> None:
    assert process.stdout is not None
    for line in process.stdout:
        log.write(line)
        log.flush()
        print(line, end="", flush=True)
        messages.put(line)


def input_probe(quartz) -> None:
    # Keycode 105 is deliberately unmapped by RetroFeel (recorded as
    # "Unknown") and is ignored by some games in ordinary gameplay.
    down = quartz.CGEventCreateKeyboardEvent(None, 105, True)
    up = quartz.CGEventCreateKeyboardEvent(None, 105, False)
    quartz.CGEventPost(quartz.kCGHIDEventTap, down)
    time.sleep(0.30)
    quartz.CGEventPost(quartz.kCGHIDEventTap, up)


def stop_retrofeel(process: subprocess.Popen) -> None:
    if process.poll() is not None:
        return
    process.send_signal(signal.SIGINT)
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def record_session(
    quartz,
    game: Game,
    frames: int,
    timeout: float,
    output: Path,
    recordings: Path,
    input_probe_enabled: bool,
    focus_monitor: FocusMonitor,
) -> Path:
    retrofeel = require_command("retrofeel")
    before = session_dirs(recordings)
    log_path = output / "retrofeel.log"
    environment = os.environ.copy()
    environment.setdefault("RUST_LOG", "info")
    command = [
        retrofeel,
        "gamehub",
        "--appid",
        str(game.app_id),
        "--record-frames",
        str(frames),
    ]
    print("+", " ".join(command), flush=True)
    with log_path.open("w") as log:
        process = subprocess.Popen(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
            env=environment,
        )
        focus_monitor.set_retrofeel_pid(process.pid)
        messages: queue.Queue[str] = queue.Queue()
        reader = threading.Thread(
            target=pump_output, args=(process, log, messages), daemon=True
        )
        reader.start()
        deadline = time.monotonic() + timeout
        recording_started_at = None
        probe_sent = False
        finalized = False
        try:
            while time.monotonic() < deadline:
                try:
                    line = messages.get(timeout=0.25)
                    if "steam: recording started" in line:
                        recording_started_at = time.monotonic()
                        focus_monitor.set_phase("recording")
                    if "steam: recording stopped" in line:
                        finalized = True
                        break
                except queue.Empty:
                    pass
                if (
                    input_probe_enabled
                    and not probe_sent
                    and recording_started_at is not None
                    and time.monotonic() - recording_started_at >= 3
                ):
                    input_probe(quartz)
                    probe_sent = True
                    print("sent harmless synthetic input probe (Unknown key)", flush=True)
                if process.poll() is not None:
                    break
        finally:
            focus_monitor.set_phase("stopping")
            stop_retrofeel(process)
            reader.join(timeout=2)
    if not finalized:
        raise RuntimeError(f"RetroFeel did not finalize the recording; see {log_path}")
    new_sessions = session_dirs(recordings) - before
    if not new_sessions:
        raise RuntimeError(f"RetroFeel finalized but no new session appeared in {recordings}")
    return max(new_sessions, key=lambda path: path.stat().st_mtime_ns)


def validate_session(
    repo: Path, session: Path, output: Path, input_probe_enabled: bool
) -> dict:
    analyzer = repo / "skills/review-retrofeel-feedback/scripts/recording_analysis.py"
    summary_path = output / "summary.json"
    run_checked(
        [sys.executable, str(analyzer), "summary", str(session), "--out", str(summary_path)],
        stdout=subprocess.DEVNULL,
    )
    summary = json.loads(summary_path.read_text())
    checks = list(summary.get("checks", []))
    if input_probe_enabled:
        input_path = session / "input.json"
        frames = json.loads(input_path.read_text()) if input_path.is_file() else []
        probe_found = any(
            "Unknown" in ((frame.get("raw_host") or {}).get("keyboard_keys") or [])
            for frame in frames
        )
        checks.append({"name": "synthetic input probe captured", "ok": probe_found})
    decoded = int(summary.get("decoded_video_frames") or 0)
    fps = float((summary.get("video_timing") or {}).get("output_fps") or 60)
    duration = max(decoded / max(fps, 1), 1)
    sheet = output / "contact-sheet.png"
    run_checked(
        [
            sys.executable,
            str(analyzer),
            "contact-sheet",
            str(session),
            "--start",
            "0",
            "--end",
            f"{duration:.6f}",
            "--every",
            f"{max(duration / 16, 1):.6f}",
            "--out",
            str(sheet),
        ],
        stdout=subprocess.DEVNULL,
    )
    summary["checks"] = checks
    summary["smoke_passed"] = bool(checks) and all(check.get("ok") for check in checks)
    summary["contact_sheet"] = str(sheet)
    summary_path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    return summary


def default_output() -> Path:
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    return Path("/private/tmp") / f"retrofeel-gamehub-smoke-{stamp}"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", type=Path, required=True, help="JSON game profile; see scripts/profiles/fictional.json")
    parser.add_argument(
        "--frames",
        type=int,
        default=8400,
        help="CFR frames to record (8400 is 2m20s at the default 60 fps)",
    )
    parser.add_argument("--launch-timeout", type=float, default=120)
    parser.add_argument("--record-timeout", type=float, default=240)
    parser.add_argument("--settle-seconds", type=float, default=8)
    parser.add_argument("--out", type=Path, default=None)
    parser.add_argument("--recordings-dir", type=Path, default=None)
    parser.add_argument("--no-launch", action="store_true", help="attach to an already running game")
    parser.add_argument("--no-input-probe", action="store_true")
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="navigate to and verify the GameHub Play button without clicking it",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.frames <= 0:
        raise RuntimeError("--frames must be positive")
    game = load_game_profile(args.profile)
    output = (args.out or default_output()).expanduser().resolve()
    output.mkdir(parents=True, exist_ok=False)
    quartz = import_macos_frameworks()
    tesseract = require_command("tesseract")
    require_command("ffmpeg")
    require_command("ffprobe")
    repo = Path(__file__).resolve().parent.parent
    recordings = (args.recordings_dir or retrofeel_recordings_dir()).expanduser().resolve()
    recordings.mkdir(parents=True, exist_ok=True)

    running = process_for_game(game)
    if running is None and not args.no_launch:
        if args.dry_run:
            navigate_to_game(quartz, game, output, tesseract, click_play=False)
            print(
                "Dry run located the requested title and Play button without launching: "
                f"{output / 'gamehub-detail.png'}"
            )
            return 0
        navigate_to_game(quartz, game, output, tesseract)
        running = wait_for_game_process(game, args.launch_timeout)
    elif running is None:
        raise RuntimeError(f"{game.title} is not running and --no-launch was supplied")
    print(f"Game process ready: pid={running[0]} {running[1]}", flush=True)
    focus_path = output / "focus-events.jsonl"
    focus_monitor = FocusMonitor(quartz, running[0], focus_path)
    focus_monitor.start()
    try:
        activate_process(running[0])
        time.sleep(max(args.settle_seconds, 0))
        session = record_session(
            quartz,
            game,
            args.frames,
            args.record_timeout,
            output,
            recordings,
            not args.no_input_probe,
            focus_monitor,
        )
    finally:
        focus_monitor.stop()
    summary = validate_session(repo, session, output, not args.no_input_probe)
    focus = focus_summary(focus_path)
    focus_checks = [
        {
            "name": "RetroFeel never became frontmost",
            "ok": not focus["retrofeel_became_frontmost"],
            "path": str(focus_path),
        },
        {
            "name": "game was frontmost during recording",
            "ok": focus["game_frontmost_during_recording"],
            "path": str(focus_path),
        },
    ]
    summary["checks"].extend(focus_checks)
    summary["focus"] = focus
    summary["smoke_passed"] = all(check.get("ok") for check in summary["checks"])
    (output / "summary.json").write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n"
    )
    result = {
        "app_id": game.app_id,
        "game": game.title,
        "game_pid": running[0],
        "session": str(session),
        "output": str(output),
        "smoke_passed": summary["smoke_passed"],
        "focus": focus,
        "game_left_running": True,
    }
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2), flush=True)
    return 0 if summary["smoke_passed"] else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (RuntimeError, subprocess.CalledProcessError) as error:
        print(f"gamehub smoke: ERROR: {error}", file=sys.stderr)
        raise SystemExit(1)
    except KeyboardInterrupt:
        print("gamehub smoke: interrupted; RetroFeel was stopped", file=sys.stderr)
        raise SystemExit(130)
