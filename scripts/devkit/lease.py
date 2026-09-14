"""Process-lifetime advisory lease; stale metadata never authorizes stealing a lock."""
import fcntl
import json
import os
from pathlib import Path
import tempfile
import time
import uuid


def default_path():
    return Path(os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")) / "retrofeel-deck.lease"


def atomic_json(path, value):
    path = Path(path)
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as out:
        temporary = Path(out.name)
        json.dump(value, out, indent=2)
        out.write("\n")
    try:
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


class Lease:
    def __init__(self, owner, project, purpose, game_id, path=None):
        self.path = Path(path) if path else default_path()
        self.file = None
        self.record = dict(schema_version=1, token=str(uuid.uuid4()), owner=owner,
            project=project, purpose=purpose, target_game=game_id, pid=os.getpid(),
            process_start=Path("/proc/self/stat").read_text().rsplit(")", 1)[1].split()[19])
        self.last_heartbeat = 0

    def __enter__(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        # The lock inode is permanent. Unlinking it creates a two-owner race.
        self.file = self.path.open("a+")
        try:
            fcntl.flock(self.file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            self.file.close()
            self.file = None
            raise RuntimeError(f"Deck is leased: {status(self.path)}") from None
        self.heartbeat("acquired")
        return self

    def heartbeat(self, phase):
        if self.file is None:
            raise RuntimeError("lease is not held")
        now = time.monotonic()
        self.record.update(phase=phase, heartbeat_monotonic=now, expires_monotonic=now + 5,
                           heartbeat_utc=time.time(), status="owned")
        atomic_json(str(self.path) + ".json", self.record)
        self.last_heartbeat = now

    def __exit__(self, *_):
        if self.file is not None:
            try:
                self.record.update(status="released", phase="finished")
                atomic_json(str(self.path) + ".json", self.record)
            finally:
                self.file.close()
                self.file = None


def status(path=None):
    path = Path(path) if path else default_path()
    if not path.exists():
        return {"active": False}
    with path.open("a+") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            active = False
        except BlockingIOError:
            active = True
        metadata = Path(str(path) + ".json")
        try:
            result = json.loads(metadata.read_text())
        except (OSError, ValueError):
            result = {}
        result["active"] = active
        return result
