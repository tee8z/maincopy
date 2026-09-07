#!/usr/bin/env python3
"""Publish a complete Litestream checkpoint through standard rclone crypt to B2."""
import argparse
import datetime
import errno
import fcntl
import json
import os
import re
from pathlib import Path
import shutil
import signal
import time
import stat
import struct
import tempfile
import uuid

from backup_common import (BackupFailure, CANDIDATE_NAME, CHECKPOINT_NAME, LTX_NAME, MAX_BYTES,
                           MAX_FILES, UTC, add_common_arguments, atomic_json, b2_destination, checkpoint_lock, cleanup_staging, encoded_paths,
                           manifest_inventory, previous_success, protected_directory,
                           protected_file, rclone, run, runtime_config, write_report)

from backup_epochs import (PUBLICATION_ALLOWANCE_SECONDS, SELECTION_FORMAT, load_epoch,
                           sync_directory, validate_lifecycle)


MAX_ARTIFACT_BYTES = 1024 * 1024 * 1024


def copy_exact_input(incoming, output, expected):
    remaining = expected
    while remaining:
        chunk = incoming.read(min(1024 * 1024, remaining))
        if not chunk:
            raise BackupFailure("immutable input changed during capture")
        output.write(chunk)
        remaining -= len(chunk)
    # Read one extra byte to detect growth, but never write past the budget.
    if incoming.read(1):
        raise BackupFailure("immutable input changed during capture")


def copy_pinned_input(incoming, output, expected):
    # FICLONERANGE clones only the admitted byte range, even if the source grows
    # after fstat. A zero range means whole-file clone, so admission rejects it.
    clone_range = struct.pack("=qQQQ", incoming.fileno(), 0, expected, 0)
    try:
        fcntl.ioctl(output.fileno(), 0x4020940D, clone_range)
    except OSError as error:
        if error.errno not in (errno.EXDEV, errno.EOPNOTSUPP, errno.ENOTTY, errno.EINVAL):
            raise
        output.truncate(0)
        output.seek(0)
        incoming.seek(0)
        copy_exact_input(incoming, output, expected)
    output.flush()
    if os.fstat(incoming.fileno()).st_size != expected or os.fstat(output.fileno()).st_size != expected:
        raise BackupFailure("immutable input changed during capture")


def pin_file(source, target, expected=None, *, remaining=MAX_BYTES):
    # A read-only bind mount cannot hardlink into another mount. Pin the open FD
    # against replica GC, use Linux reflink when available, and copy otherwise.
    descriptor = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb", buffering=0) as incoming:
        metadata = os.fstat(incoming.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o077 or metadata.st_uid not in (0, os.getuid()) or not 0 < metadata.st_size <= MAX_BYTES:
            raise BackupFailure("immutable input validation")
        if metadata.st_size > remaining:
            raise BackupFailure("checkpoint remaining byte limit")
        if expected is not None and metadata.st_size != expected:
            raise BackupFailure("replica plan size validation")
        # All limits use the opened source FD and are checked before creating
        # the destination or attempting any copy/reflink allocation.
        target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        with target.open("xb", buffering=0) as output:
            try:
                copy_pinned_input(incoming, output, metadata.st_size)
                os.utime(target, ns=(metadata.st_atime_ns, metadata.st_mtime_ns))
            except (OSError, BackupFailure):
                target.unlink(missing_ok=True)
                raise
        return metadata.st_size


def confirm_replica(args):
    confirmation = json.loads(run([args.litestream, "sync", "-wait", "-json", "-timeout", "30",
                                   "-socket", args.socket, args.database], "live replica synchronization", 35, maximum=1024))
    txid, replicated = confirmation.get("txid"), confirmation.get("replica_txid")
    if confirmation.get("db_path") != args.database or type(txid) is not int or type(replicated) is not int:
        raise BackupFailure("live replica identity validation")
    if not 0 < txid <= replicated <= 0xffffffffffffffff:
        raise BackupFailure("live replica cutoff confirmation")
    confirmed_at = datetime.datetime.now(UTC).isoformat(timespec="seconds").replace("+00:00", "Z")
    return replicated, confirmed_at


def stage_plan(args, staging, confirmed):
    replica = Path(args.replica)
    raw_plan = run([args.litestream, "restore", "-dry-run", "-json", "-o", str(staging / "planned.sqlite3"), f"file://{replica}"], "replica restore planning", 60)
    plan = json.loads(raw_plan)
    cutoff = plan.get("max_txid")
    if not isinstance(cutoff, str) or not re.fullmatch(r"[0-9a-f]{16}", cutoff) or int(cutoff, 16) < confirmed:
        raise BackupFailure("replica plan precedes confirmed cutoff")
    files = plan.get("files")
    if not isinstance(files, list) or not files or len(files) > MAX_FILES:
        raise BackupFailure("replica plan file limit")
    total = 0
    seen = set()
    for item in files:
        level, name, size = item.get("level"), item.get("name"), item.get("size")
        if type(level) is not int or level not in range(10) or not isinstance(name, str) or not LTX_NAME.fullmatch(name):
            raise BackupFailure("replica plan path validation")
        if type(size) is not int or not 0 < size <= MAX_BYTES:
            raise BackupFailure("replica plan size limit")
        relative = Path("ltx") / str(level) / name
        if relative in seen:
            raise BackupFailure("replica plan duplicate file")
        seen.add(relative)
        total += size
        if total > MAX_BYTES:
            raise BackupFailure("replica plan byte limit")
        pin_file(replica / relative, staging / relative, size)
    (staging / "plan.json").write_bytes(raw_plan)
    return total, cutoff


def stage_artifacts(args, staging, total):
    target = staging / "content-candidates"
    protected_directory(target)
    source = Path(args.artifacts)
    if not source.exists():
        return
    # Candidate archives precede their committed SQLite references and are
    # immutable. Capture the inventory after the LTX cutoff has been pinned.
    with os.scandir(source) as entries:
        count = 0
        artifact_bytes = 0
        for entry in entries:
            if not entry.name.endswith(".candidate"):
                continue
            count += 1
            if count > 4096 or not CANDIDATE_NAME.fullmatch(entry.name):
                raise BackupFailure("candidate inventory validation")
            remaining = min(MAX_ARTIFACT_BYTES - artifact_bytes, MAX_BYTES - total)
            size = pin_file(Path(entry.path), target / entry.name, remaining=remaining)
            artifact_bytes += size
            total += size


def capture(args, staging, confirmed):
    # GC can unlink a planned file before open(). A bounded new plan is safe;
    # an already open source remains available through copy/reflink completion.
    for attempt in range(3):
        captured = staging / f"capture-{attempt}"
        protected_directory(captured)
        try:
            total, cutoff = stage_plan(args, captured, confirmed)
            stage_artifacts(args, captured, total)
            database = captured / "database.sqlite3"
            run([args.litestream, "restore", "-txid", cutoff, "-integrity-check", "full",
                 "-o", str(database), "file://" + str(captured)], "checkpoint reference replay", 180)
            manifest = captured / "checkpoint.json"
            run([args.maincopyd, "--config", args.config, "checkpoint-manifest",
                 "--database-file", str(database), "--plan-file", str(captured / "plan.json"),
                 "--ltx-root", str(captured / "ltx"),
                 "--artifact-root", str(captured / "content-candidates"), "--output", str(manifest)],
                "checkpoint manifest validation", 180)
            inventory = manifest_inventory(manifest)[1]
            for plaintext in captured.glob("database.sqlite3*"):
                plaintext.unlink()
            return captured, inventory
        except FileNotFoundError:
            shutil.rmtree(captured)
    raise BackupFailure("replica changed during checkpoint capture")


def encrypt_objects(args, config, captured, inventory, staging):
    objects = staging / "plain-objects"
    protected_directory(objects)
    for relative, identity in inventory:
        destination = objects / identity["digest"]
        if not destination.exists():
            os.link(captured / relative, destination)
    # Immutable digest paths reuse ciphertext. A complete cryptcheck verifies
    # cached ciphertext against pinned plaintext before that ciphertext is used.
    rclone(args, config, "copy", "--immutable", "--size-only", str(objects), "localcrypt:objects", timeout=180)
    rclone(args, config, "cryptcheck", str(objects), "localcrypt:objects", "--one-way", timeout=180)
    return ["objects/" + name.name for name in objects.iterdir()]


def send_objects(args, config, cache, encoded, staging, epoch):
    listing = staging / "encrypted-files"
    listing.write_text("\n".join(encoded) + "\n", encoding="ascii")
    rclone(args, config, "copy", "--ignore-existing", "--checksum", "--files-from", str(listing),
           str(cache), b2_destination(args) + "/epochs/" + epoch, timeout=180)


def retain_checkpoint(directory, name, epoch, cache, encrypted_objects, encrypted_manifest, latest_name):
    retained = directory / "checkpoints"
    protected_directory(retained)
    pending = retained / (".pending-" + name)
    protected_directory(pending)
    try:
        for relative in [*encrypted_objects, encrypted_manifest]:
            source = cache / relative
            protected_file(source)
            with source.open("rb") as contents:
                os.fsync(contents.fileno())
            target = pending / "epochs" / epoch / relative
            target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            os.link(source, target)
        os.link(cache / latest_name, pending / latest_name)
        with (pending / latest_name).open("rb") as contents:
            os.fsync(contents.fileno())
        for nested in sorted((path for path in pending.rglob("*") if path.is_dir()), reverse=True):
            sync_directory(nested)
        sync_directory(pending)
        os.rename(pending, retained / name)
        sync_directory(retained)
        sync_directory(directory)
    finally:
        if pending.exists():
            shutil.rmtree(pending)


def publish(args, directory):
    runtime = Path(args.runtime_directory)
    protected_directory(runtime)
    epoch_path = Path(args.replica).parent / "epoch.json"
    epoch = load_epoch(epoch_path, datetime.datetime.now(UTC), PUBLICATION_ALLOWANCE_SECONDS)
    cache = directory / "epochs" / epoch
    protected_directory(cache)
    # Bulk capture is shared with local expiration; credentials must remain in
    # this unit's private runtime mount, hidden from every peer service.
    with tempfile.TemporaryDirectory(prefix=".capture-", dir=directory) as temporary, \
            tempfile.TemporaryDirectory(prefix=".config-", dir=runtime) as private:
        staging = Path(temporary)
        config = runtime_config(args, Path(private), cache, epoch)
        validate_lifecycle(args, config)
        confirmed, confirmed_at = confirm_replica(args)
        captured, inventory = capture(args, staging, confirmed)
        logical = encrypt_objects(args, config, captured, inventory, staging)
        encoded = encoded_paths(args, config, logical)
        send_objects(args, config, cache, encoded, staging, epoch)
        # Existing remote ciphertext can have a different valid nonce (for
        # example after local cache eviction). Verify plaintext equivalence
        # using the remote nonce and B2 hash before selecting this checkpoint.
        rclone(args, config, "cryptcheck", str(staging / "plain-objects"),
               "offsitecrypt:objects", "--one-way", timeout=180)
        name = datetime.datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ-") + str(uuid.uuid4())
        manifest_name = "checkpoints/" + name + ".json"
        rclone(args, config, "copyto", "--immutable", str(captured / "checkpoint.json"), "localcrypt:" + manifest_name)
        manifest_path, latest_path = encoded_paths(args, config, [manifest_name, "latest.json"])
        destination = b2_destination(args) + "/epochs/" + epoch + "/"
        # B2 publishes a completed upload atomically. Publish the versioned
        # manifest and then replace latest only after every object succeeded.
        rclone(args, config, "copyto", "--immutable", "--checksum", str(cache / manifest_path), destination + manifest_path)
        # The root selector has no subscriber data. Its authenticated contents
        # select one complete epoch; objects never cross that epoch boundary.
        selector = staging / "selection.json"
        atomic_json(selector, {"format": SELECTION_FORMAT, "epoch": epoch, "checkpoint": name})
        rclone(args, config, "copyto", str(selector), "localcrypt:latest.json")
        # Recheck wall-clock expiry immediately before publishing a selector.
        if load_epoch(epoch_path, datetime.datetime.now(UTC)) != epoch:
            raise BackupFailure("backup epoch changed during publication")
        retain_checkpoint(directory, name, epoch, cache, encoded, manifest_path, latest_path)
        rclone(args, config, "copyto", "--checksum", str(cache / latest_path), b2_destination(args) + "/" + latest_path)
        # Retained checkpoint hardlinks preserve old ciphertext independently.
        keep = set(encoded)
        for path in cache.rglob("*"):
            if path.is_file() and str(path.relative_to(cache)) not in keep:
                path.unlink()
        return confirmed_at


def interrupted(_signal, _frame):
    raise BackupFailure("checkpoint shutdown")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    add_common_arguments(parser)
    for name in ("directory", "runtime-directory", "replica", "artifacts", "status-file", "socket", "database"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--cleanup-only", action="store_true")
    parser.add_argument("--remote-retention-days", type=int, default=9)
    args = parser.parse_args()
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    signal.signal(signal.SIGALRM, interrupted)
    signal.alarm(PUBLICATION_ALLOWANCE_SECONDS)
    started = time.monotonic()
    os.umask(0o077)
    directory = Path(args.directory)
    report = Path(args.status_file)
    protected_directory(report.parent)
    try:
        protected_directory(directory)
        with checkpoint_lock(directory):
            cleanup_staging(directory)
            if args.cleanup_only:
                return
            previous = previous_success(report)
            try:
                success = publish(args, directory)
            except (OSError, ValueError, KeyError, TypeError, AttributeError, BackupFailure) as error:
                write_report(report, False, previous)
                stage = str(error) if isinstance(error, BackupFailure) else "protected checkpoint preparation"
                parser.exit(1, f"Maincopy checkpoint failed during {stage}.\n")
            write_report(report, True, success)
            print(f"Complete encrypted checkpoint published in {time.monotonic() - started:.1f} seconds.")
    except (OSError, BackupFailure):
        parser.exit(1, "Maincopy checkpoint could not acquire its lock or publish protected health state.\n")


if __name__ == "__main__":
    main()
