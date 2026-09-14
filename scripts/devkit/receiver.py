"""Minimal application receiver: reads only the test runner's owned event node."""
import argparse
from contextlib import closing
import json
import signal
import time
from pathlib import Path


def main():
    from evdev import InputDevice
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--device", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--ready", type=Path, required=True)
    args = parser.parse_args()
    running = True
    def stop(*_):
        nonlocal running
        running = False
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    import select
    with closing(InputDevice(args.device)) as device, args.output.open("a") as output:
        if not device.name.startswith("RetroFeel test "):
            raise RuntimeError("receiver refuses a device it does not own")
        args.ready.write_text(json.dumps({"pid": __import__("os").getpid(), "device": args.device, "name": device.name}))
        while running:
            if not select.select([device.fd], [], [], 0.05)[0]:
                continue
            try:
                events = device.read()
                for event in events:
                    output.write(json.dumps({"received_boottime_us": time.clock_gettime_ns(time.CLOCK_BOOTTIME) // 1000,
                        "type": event.type, "code": event.code, "value": event.value}) + "\n")
                    output.flush()
            except OSError:
                break


if __name__ == "__main__":
    main()
