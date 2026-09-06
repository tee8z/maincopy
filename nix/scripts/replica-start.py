#!/usr/bin/env python3
"""Defer native replication until Maincopy consumes offline restore acceptance."""
import argparse
import ctypes
import os
from pathlib import Path
import select
import sys
import time


def restoration_pending(marker):
    guard = marker.with_name(marker.name.removesuffix(".restore.json") + ".restore-pending")
    # Acceptance publishes the marker before removing the guard. Observe them
    # in that order's reverse so that transition cannot look like two absences.
    for path in (guard, marker):
        try:
            # A broken symlink or interrupted acceptance guard still blocks
            # native writes. Only Maincopy may accept either condition.
            path.lstat()
        except FileNotFoundError:
            continue
        return True
    return False


def wait_for_acceptance(marker, timeout_seconds):
    deadline = time.monotonic() + timeout_seconds
    libc = ctypes.CDLL(None, use_errno=True)
    libc.inotify_init1.argtypes = [ctypes.c_int]
    libc.inotify_add_watch.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_uint32]
    notification = libc.inotify_init1(os.O_CLOEXEC | os.O_NONBLOCK)
    if notification < 0:
        raise OSError(ctypes.get_errno(), "restore acceptance notification failed")
    try:
        # Watch the containing directory before checking the marker, so a
        # concurrent consume cannot fall between inspection and watch setup.
        # DELETE, MOVED_FROM, MOVED_TO, CREATE, DELETE_SELF, MOVE_SELF; require
        # a real directory and do not follow a symlink for the watched root.
        events = 0x200 | 0x40 | 0x80 | 0x100 | 0x400 | 0x800 | 0x1000000 | 0x2000000
        if libc.inotify_add_watch(notification, os.fsencode(marker.parent), events) < 0:
            raise OSError(ctypes.get_errno(), "restore acceptance watch failed")
        while True:
            if not restoration_pending(marker):
                return
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([notification], [], [], remaining)[0]:
                raise TimeoutError
            # Events only trigger a fresh lstat; no event payload authorizes
            # native replication. Bound every read, including queue overflow.
            os.read(notification, 4096)
    finally:
        os.close(notification)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--marker", type=Path, required=True)
    parser.add_argument("--timeout-seconds", type=int, choices=range(1, 181), default=180)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    arguments = parser.parse_args()
    command = arguments.command
    if command and command[0] == "--":
        command = command[1:]
    if not command or not Path(command[0]).is_absolute():
        parser.error("an absolute native replica command is required")
    try:
        wait_for_acceptance(arguments.marker, arguments.timeout_seconds)
    except TimeoutError:
        print("maincopy replica start: restore acceptance is still pending", file=sys.stderr)
        return 1
    except OSError:
        print("maincopy replica start: restore acceptance cannot be inspected", file=sys.stderr)
        return 1
    try:
        os.execv(command[0], command)
    except OSError:
        print("maincopy replica start: native replica could not start", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
