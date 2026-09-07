"""Finite Litestream generations and independent encrypted-checkpoint expiration.

Native Litestream must be stopped before prepare_epoch. The same publisher lock
serializes rotation, uploads and expiration; no database connection is opened.
Epoch object namespaces never reference another epoch. Native retention remains
a space optimization inside an epoch, not the privacy expiration authority.
"""
import argparse
import datetime
import fcntl
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import tempfile
import uuid

from backup_common import (BackupFailure, CHECKPOINT_NAME, UTC, atomic_json,
                           b2_destination, checkpoint_lock, cleanup_staging, credential_profile, protected_directory, protected_file, rclone, write_config)

MAX_EPOCH_SECONDS = 86400
MAX_EPOCHS = 16384
MAX_CHECKPOINTS = 65536
PUBLICATION_ALLOWANCE_SECONDS = 600
EPOCH_FORMAT = "maincopy-backup-epoch-v1"
SELECTION_FORMAT = "maincopy-backup-selection-v1"


def epoch_time(name):
    if not isinstance(name, str) or not CHECKPOINT_NAME.fullmatch(name):
        raise BackupFailure("backup epoch identity validation")
    try:
        identifier = uuid.UUID(name[17:])
        instant = datetime.datetime.strptime(name[:16], "%Y%m%dT%H%M%SZ").replace(tzinfo=UTC)
    except ValueError as error:
        raise BackupFailure("backup epoch identity validation") from error
    if identifier.version != 4 or str(identifier) != name[17:]:
        raise BackupFailure("backup epoch identity validation")
    return instant


def epoch_record(path):
    value = json.loads(protected_file(path, 1024))
    if not isinstance(value, dict) or set(value) != {"format", "epoch", "duration_seconds"} or value["format"] != EPOCH_FORMAT:
        raise BackupFailure("backup epoch record validation")
    opened = epoch_time(value["epoch"])
    duration = value["duration_seconds"]
    if type(duration) is not int or not 3600 <= duration <= MAX_EPOCH_SECONDS:
        raise BackupFailure("backup epoch duration validation")
    return value["epoch"], opened, duration


def load_epoch(path, now, reserve_seconds=0):
    name, opened, duration = epoch_record(path)
    if opened > now or opened + datetime.timedelta(seconds=duration) <= now + datetime.timedelta(seconds=reserve_seconds):
        raise BackupFailure("backup epoch is outside its upload window")
    return name


def selection(value):
    if not isinstance(value, dict) or set(value) != {"format", "epoch", "checkpoint"} or value["format"] != SELECTION_FORMAT:
        raise BackupFailure("checkpoint selection validation")
    epoch_time(value["epoch"])
    epoch_time(value["checkpoint"])
    return value["epoch"], value["checkpoint"]


def sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def epoch_directories(root, *, only_recognized=False):
    if not root.exists():
        return []
    protected_directory(root)
    result = []
    maximum = MAX_CHECKPOINTS if only_recognized else MAX_EPOCHS
    with os.scandir(root) as entries:
        for index, entry in enumerate(entries):
            if index >= maximum:
                raise BackupFailure("backup epoch inventory limit")
            if only_recognized and not CHECKPOINT_NAME.fullmatch(entry.name):
                continue
            if len(result) >= maximum or not entry.is_dir(follow_symlinks=False):
                raise BackupFailure("backup epoch inventory validation")
            epoch_time(entry.name)
            result.append(Path(entry.path))
    return result


def prepare_epoch(root, duration, now):
    if not 3600 <= duration <= MAX_EPOCH_SECONDS:
        raise BackupFailure("backup epoch duration validation")
    protected_directory(root)
    retired = root / "retired"
    protected_directory(retired)
    if len(epoch_directories(retired)) >= MAX_EPOCHS:
        raise BackupFailure("backup epoch inventory limit")
    # Legacy paths need an explicit migration while native replication is
    # stopped. Never silently mix an existing unscoped replica into a new era.
    if os.path.lexists(root / "metadata") or os.path.lexists(root / "replica"):
        raise BackupFailure("legacy replica requires documented epoch migration")
    active = root / "active"
    if os.path.lexists(active):
        protected_directory(active)
        name, opened, _ = epoch_record(active / "epoch.json")
        if opened > now:
            raise BackupFailure("backup epoch clock moved backwards")
        if os.path.lexists(retired / name):
            raise BackupFailure("retired backup epoch already exists")
        os.rename(active, retired / name)
        sync_directory(retired)
        sync_directory(root)
    # Build the record before atomically exposing active. A failed prepare can
    # leave only an empty unpublished staging directory, never a mixed replica.
    name = now.strftime("%Y%m%dT%H%M%SZ-") + str(uuid.uuid4())
    pending = root / ".preparing"
    if os.path.lexists(pending):
        protected_directory(pending)
        shutil.rmtree(pending)
    protected_directory(pending)
    for child in ("metadata", "replica"):
        protected_directory(pending / child)
    atomic_json(pending / "epoch.json", {"format": EPOCH_FORMAT, "epoch": name, "duration_seconds": duration})
    sync_directory(pending)
    os.rename(pending, active)
    sync_directory(root)
    return name


def expire_local(directory, replica_root, now, retention_days):
    cleanup_staging(directory)
    cutoff = now - datetime.timedelta(days=retention_days)
    checkpoints = directory / "checkpoints"
    for path in epoch_directories(checkpoints, only_recognized=True):
        if epoch_time(path.name) < cutoff:
            shutil.rmtree(path)
    # An object first uploaded at the start of an epoch may still be required
    # by its last checkpoint. Preserve that finite dependency interval too.
    epoch_cutoff = cutoff - datetime.timedelta(seconds=MAX_EPOCH_SECONDS)
    for root in (directory / "epochs", replica_root / "retired"):
        for path in epoch_directories(root):
            if epoch_time(path.name) < epoch_cutoff:
                shutil.rmtree(path)
        if root.exists():
            sync_directory(root)
    if checkpoints.exists():
        sync_directory(checkpoints)
    expire_stopped_replica(replica_root, epoch_cutoff)


def expire_stopped_replica(replica_root, cutoff):
    if not replica_root.exists():
        return
    protected_directory(replica_root)
    descriptor = os.open(replica_root / ".native.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "rb+") as lock:
        metadata = os.fstat(lock.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o077 or metadata.st_uid != os.getuid():
            raise BackupFailure("native replica lock validation")
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            # RuntimeMaxSec is monotonic; a wall-clock jump must never let
            # expiration rename files beneath the still-running replica.
            return
        active = replica_root / "active"
        if not os.path.lexists(active):
            return
        protected_directory(active)
        name, opened, _ = epoch_record(active / "epoch.json")
        if opened < cutoff:
            # A legitimately supervised replica cannot remain in this epoch
            # beyond one day. Also expire stopped/failed stale native state.
            retired = replica_root / "retired"
            protected_directory(retired)
            destination = retired / name
            if os.path.lexists(destination):
                raise BackupFailure("stale epoch retirement collision")
            # A timeout during deletion leaves a recognized retired directory,
            # never an active directory whose identifying record is missing.
            os.rename(active, destination)
            sync_directory(replica_root)
            sync_directory(retired)
            shutil.rmtree(destination)
            sync_directory(retired)


def validate_lifecycle(args, config):
    rules = json.loads(rclone(args, config, "backend", "lifecycle", f"maincopy-b2:{args.bucket}", maximum=16384))
    # A dedicated bucket with one exact whole-bucket rule is reviewable. Never
    # mutate account policy here, and never infer expiration from hidden state.
    if not isinstance(rules, list) or len(rules) != 1 or not isinstance(rules[0], dict):
        raise BackupFailure("B2 lifecycle policy requires operator configuration")
    rule = rules[0]
    expected = {"fileNamePrefix": "", "daysFromUploadingToHiding": args.remote_retention_days,
                "daysFromHidingToDeleting": 1, "daysFromStartingToCancelingUnfinishedLargeFiles": 1}
    if set(rule) != set(expected) or any(type(rule[key]) is not type(value) or rule[key] != value for key, value in expected.items()):
        raise BackupFailure("B2 lifecycle policy does not match configured retention")


def expire_remote(args, config, now):
    destination = b2_destination(args) + "/epochs"
    names = rclone(args, config, "lsf", destination, "--dirs-only", "--max-depth", "1", "--b2-versions", maximum=2 * 1024 * 1024).decode("ascii").splitlines()
    if len(names) > MAX_EPOCHS:
        raise BackupFailure("remote backup epoch inventory limit")
    cutoff = now - datetime.timedelta(days=args.retention_days, seconds=MAX_EPOCH_SECONDS)
    for raw in sorted(set(names)):
        if not raw.endswith("/"):
            raise BackupFailure("remote backup epoch inventory validation")
        name = raw[:-1]
        if epoch_time(name) < cutoff:
            # Native B2 purge deletes current AND historical versions. The
            # validated exact epoch prefix cannot select another recovery unit.
            target = destination + "/" + name
            rclone(args, config, "purge", target)
            rclone(args, config, "backend", "cleanup", target, "-o", "max-age=0s")


def timed_out(_signal, _frame):
    raise BackupFailure("backup epoch operation deadline")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("prepare", "expire-local", "expire-remote"))
    parser.add_argument("--replica-root", required=True)
    parser.add_argument("--directory", required=True)
    parser.add_argument("--epoch-seconds", type=int, default=MAX_EPOCH_SECONDS)
    parser.add_argument("--retention-days", type=int, default=7)
    parser.add_argument("--remote-retention-days", type=int, default=9)
    parser.add_argument("--runtime-directory")
    for name in ("rclone", "credentials", "bucket", "prefix"):
        parser.add_argument("--" + name)
    args = parser.parse_args()
    os.umask(0o077)
    signal.signal(signal.SIGALRM, timed_out)
    signal.signal(signal.SIGTERM, timed_out)
    signal.signal(signal.SIGINT, timed_out)
    signal.alarm(PUBLICATION_ALLOWANCE_SECONDS)
    if not 1 <= args.retention_days <= 30 or not args.retention_days + 2 <= args.remote_retention_days <= 90:
        parser.error("backup retention bounds are invalid")
    directory, replica_root = Path(args.directory), Path(args.replica_root)
    try:
        if args.mode == "expire-local":
            protected_directory(directory)
        # The kernel lock supplies notification; the owned alarm bounds
        # waiting plus work without coordinating through polling sleeps.
        with checkpoint_lock(directory, wait=True):
            now = datetime.datetime.now(UTC)
            if args.mode == "prepare":
                prepare_epoch(replica_root, args.epoch_seconds, now)
            elif args.mode == "expire-local":
                expire_local(directory, replica_root, now, args.retention_days)
            else:
                # Separate unit: LoadCredential or network failure can never
                # stop the independent, credential-free local expiration job.
                runtime = Path(args.runtime_directory)
                protected_directory(runtime)
                with tempfile.TemporaryDirectory(prefix=".config-", dir=runtime) as temporary:
                    staging = Path(temporary)
                    config = write_config(credential_profile(args), staging)
                    validate_lifecycle(args, config)
                    expire_remote(args, config, now)

    except (OSError, ValueError, TypeError, KeyError, AttributeError, BackupFailure):
        parser.exit(1, "Backup epoch preparation or expiration failed; inspect the configured retention and service state.\n")


if __name__ == "__main__":
    main()
