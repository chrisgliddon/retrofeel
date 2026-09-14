"""Inspect only process identity and Steam IDs; never export process environments."""
import hashlib
from pathlib import Path
import re
import subprocess

STEAM_KEYS = (b"SteamAppId", b"SteamGameId", b"STEAM_COMPAT_APP_ID")


def identity(pid, root=Path("/proc")):
    try:
        return (root / str(pid) / "stat").read_text().rsplit(")", 1)[1].split()[19]
    except (OSError, IndexError):
        return None


def steam_games(root=Path("/proc")):
    games = {}
    for process in root.glob("[0-9]*"):
        try:
            stat = process / "stat"
            if stat.exists() and stat.read_text().rsplit(")", 1)[1].split()[0] == "Z":
                continue
            ids = set()
            # Read ID values, discard everything else without logging it.
            for entry in (process / "environ").read_bytes().split(b"\0"):
                key, separator, value = entry.partition(b"=")
                if separator and key in STEAM_KEYS and value.isdigit() and int(value) > 0:
                    ids.add(str(int(value)))
            command = (process / "cmdline").read_bytes()
            ids.update(value.decode() for value in re.findall(rb"SteamLaunch AppId=(\d+)", command) if int(value) > 0)
            if ids:
                games[int(process.name)] = ids
        except (FileNotFoundError, ProcessLookupError):
            continue
        except PermissionError:
            # Another UID is not a playable game in this user's Steam session.
            command = (process / "cmdline").read_bytes().split(b"\0")
            system_helper = (command[:2] == [b"/usr/lib/systemd/systemd", b"--user"]
                or command[0] == b"(sd-pam)"
                or command[0] == b"gamescope" and b"--generate-drm-mode" in command
                or command[0].startswith(b"sshd-session:")
                or command[0] == b"CSS Loader (/home/deck/homebrew/plugins/SDH-CssLoader/main.py)")
            if process.stat().st_uid == __import__("os").getuid() and not system_helper:
                raise RuntimeError(f"cannot inspect process ownership for PID {process.name}") from None
    return games


def executable_hash(pid):
    digest = hashlib.sha256()
    with Path(f"/proc/{pid}/exe").open("rb") as source:
        for data in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(data)
    return digest.hexdigest()


class Guard:
    def __init__(self, pid=None, window=None, expected_hash=None, game_id=None, route="direct", fixture=False):
        self.pid, self.window, self.game_id, self.route = pid, window, game_id, route
        self.fixture = fixture
        self.start = identity(pid) if pid else None
        self.allowed_ids = {str(game_id)} if game_id else set()
        if game_id and int(game_id) & 0xffffffff == 0x02000000:
            self.allowed_ids.add(str(int(game_id) >> 32))
        if not fixture:
            if not pid or not window or not expected_hash or self.start is None:
                raise RuntimeError("game tests require PID, focus window and expected executable SHA256")
            if executable_hash(pid) != expected_hash:
                raise RuntimeError("wrong executable build")
            window_pid = subprocess.check_output(["xdotool", "getwindowpid", str(window)], text=True, timeout=2).strip()
            if window_pid != str(pid):
                raise RuntimeError("focus window does not belong to target PID")
        self.check()

    def check(self):
        games = steam_games()
        if self.fixture:
            if games:
                raise RuntimeError("foreign game is running")
            return
        if identity(self.pid) != self.start:
            raise RuntimeError("target exited or PID was reused")
        for pid, ids in games.items():
            if pid == self.pid:
                continue
            if not ids.issubset(self.allowed_ids):
                raise RuntimeError("foreign native/Proton Steam game is running")
        if self.route == "steam" and not (games.get(self.pid, set()) & self.allowed_ids):
            raise RuntimeError("target has no matching Steam launch identity")
        focus = subprocess.check_output(["xdotool", "getwindowfocus"], text=True, timeout=2).strip()
        if focus != str(self.window):
            raise RuntimeError("target lost focus")
