"""Protected files and stock rclone plumbing shared by checkpoint tools."""
import base64
import configparser
import datetime
import json
import os
from pathlib import Path
import re
import stat
import selectors
import time
import subprocess
import tempfile

UTC = datetime.timezone.utc
REPORT_FORMAT = "maincopy-backup-status-v1"
DIGEST = re.compile(r"[0-9a-f]{64}\Z")
LTX_NAME = re.compile(r"[0-9a-f]{16}-[0-9a-f]{16}\.ltx\Z")
CANDIDATE_NAME = re.compile(r"content-b3-v1-[0-9a-f]{64}\.candidate\Z")
CHECKPOINT_NAME = re.compile(r"\d{8}T\d{6}Z-[0-9a-f-]{36}\Z")
MAX_MANIFEST = 4 * 1024 * 1024
MAX_FILES = 8192
MAX_BYTES = 17 * 1024 * 1024 * 1024
ENVIRONMENT = {"SSL_CERT_FILE": "/etc/ssl/certs/ca-certificates.crt"}


class BackupFailure(Exception):
    pass


def protected_file(path, maximum=None):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o077 or metadata.st_uid not in (0, os.getuid()):
            raise BackupFailure("protected file validation")
        if maximum is None:
            return None
        data = source.read(maximum + 1)
        if not data or len(data) > maximum:
            raise BackupFailure("protected file size validation")
        return data


def protected_directory(path):
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    metadata = path.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_mode & 0o077 or metadata.st_uid != os.getuid():
        raise BackupFailure("protected directory validation")


def run(command, stage, timeout=180, output=None, input_bytes=None, maximum=MAX_MANIFEST):
    # Drain bounded stdout while the child runs. Fail and kill the child as soon
    # as its control response exceeds the limit, without buffering it to disk.
    try:
        with subprocess.Popen(command, stdin=subprocess.PIPE if input_bytes is not None else subprocess.DEVNULL,
                              stdout=subprocess.PIPE if output is None else output,
                              stderr=subprocess.DEVNULL, env=ENVIRONMENT) as child:
            try:
                if input_bytes is not None:
                    child.stdin.write(input_bytes)
                    child.stdin.close()
                deadline = time.monotonic() + timeout
                data = bytearray()
                if output is None:
                    with selectors.DefaultSelector() as selector:
                        selector.register(child.stdout, selectors.EVENT_READ)
                        while True:
                            remaining = deadline - time.monotonic()
                            if remaining <= 0 or not selector.select(remaining):
                                raise BackupFailure(stage + " timeout")
                            chunk = os.read(child.stdout.fileno(), min(65536, maximum + 1 - len(data)))
                            if not chunk:
                                break
                            data.extend(chunk)
                            if len(data) > maximum:
                                raise BackupFailure(stage + " output limit")
                if child.wait(timeout=max(0.001, deadline - time.monotonic())) != 0:
                    raise BackupFailure(stage)
                return bytes(data)
            finally:
                if child.poll() is None:
                    child.kill()
    except (OSError, subprocess.SubprocessError) as error:
        raise BackupFailure(stage) from error


def runtime_config(args, directory, cache):
    raw_key = protected_file(args.key, 128).strip()
    try:
        decoded = base64.b64decode(raw_key, validate=True)
    except ValueError as error:
        raise BackupFailure("crypt key validation") from error
    if len(decoded) != 32 or base64.b64encode(decoded) != raw_key:
        raise BackupFailure("crypt key validation")
    profile = configparser.ConfigParser(interpolation=None)
    try:
        profile.read_string(protected_file(args.credentials, 16384).decode("utf-8"))
    except configparser.Error as error:
        raise BackupFailure("B2 credential profile validation") from error
    if profile.sections() != ["maincopy-b2"] or profile.defaults():
        raise BackupFailure("B2 credential profile validation")
    values = profile["maincopy-b2"]
    if set(values) != {"type", "account", "key"} or values["type"] != "b2":
        raise BackupFailure("B2 credential fields validation")
    if any(not re.fullmatch(r"[A-Za-z0-9/_+=.-]{1,256}", values[field]) for field in ("account", "key")):
        raise BackupFailure("B2 credential value validation")
    # rclone's standard obscure command receives the key on stdin. The generated
    # config is still a secret: obscure is reversible, not encryption at rest.
    obscured = run([args.rclone, "obscure", "-"], "crypt key preparation", 15,
                   input_bytes=raw_key, maximum=1024).decode("ascii").strip()
    for remote, target in (("localcrypt", str(cache)),
                           ("offsitecrypt", f"maincopy-b2:{args.bucket}/{args.prefix}")):
        profile[remote] = {"type": "crypt", "remote": target, "password": obscured,
                           "filename_encryption": "standard", "directory_name_encryption": "true"}
    path = directory / "rclone.conf"
    with path.open("x", encoding="utf-8") as output:
        os.chmod(path, 0o600)
        profile.write(output)
    return path


def rclone(args, config, *command, timeout=180, maximum=MAX_MANIFEST):
    return run([args.rclone, "--config", str(config), "--log-level", "ERROR", "--stats", "0",
                "--retries", "2", "--low-level-retries", "2", "--timeout", "60s",
                "--contimeout", "15s", *command], "encrypted object transfer", timeout, maximum=maximum)


def encoded_paths(args, config, logical):
    result = []
    for start in range(0, len(logical), 128):
        batch = logical[start:start + 128]
        encoded = rclone(args, config, "backend", "encode", "localcrypt:", *batch).decode("ascii").splitlines()
        if len(encoded) != len(batch) or any(not re.fullmatch(r"[a-z0-9/]+", value) for value in encoded):
            raise BackupFailure("encrypted name validation")
        result.extend(encoded)
    return result


def manifest_inventory(path):
    manifest = json.loads(protected_file(path, MAX_MANIFEST))
    if manifest.get("format") != "maincopy-litestream-checkpoint-v1":
        raise BackupFailure("checkpoint format validation")
    files, artifacts = manifest.get("files"), manifest.get("artifacts")
    if not isinstance(files, list) or not isinstance(artifacts, list) or not files or len(files) + len(artifacts) > MAX_FILES:
        raise BackupFailure("checkpoint inventory limit")
    inventory = []
    for item in files:
        level, name = item.get("level"), item.get("name")
        if type(level) is not int or level not in range(10) or not isinstance(name, str) or not LTX_NAME.fullmatch(name):
            raise BackupFailure("checkpoint LTX path validation")
        inventory.append((Path("ltx") / str(level) / name, item.get("file")))
    for item in artifacts:
        name = item.get("name")
        if not isinstance(name, str) or not CANDIDATE_NAME.fullmatch(name):
            raise BackupFailure("checkpoint artifact path validation")
        inventory.append((Path("content-candidates") / name, item.get("file")))
    total, seen = 0, set()
    for path, identity in inventory:
        if path in seen or not isinstance(identity, dict):
            raise BackupFailure("checkpoint duplicate path validation")
        seen.add(path)
        digest, size = identity.get("digest"), identity.get("bytes")
        if not isinstance(digest, str) or not DIGEST.fullmatch(digest) or type(size) is not int or not 0 < size <= MAX_BYTES:
            raise BackupFailure("checkpoint object identity validation")
        total += size
    if total > MAX_BYTES:
        raise BackupFailure("checkpoint byte limit")
    return manifest, inventory


def previous_success(path):
    try:
        previous = json.loads(protected_file(path, 4096))
        if previous.get("format") != REPORT_FORMAT:
            return None
        value = previous.get("last_success_at")
        if not isinstance(value, str) or not value.endswith("Z") or len(value) > 40:
            return None
        datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
        return value
    except (OSError, ValueError, AttributeError, BackupFailure):
        return None


def atomic_json(path, value):
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, prefix=".pending-", delete=False) as temporary:
        pending = Path(temporary.name)
        try:
            os.fchmod(temporary.fileno(), 0o600)
            json.dump(value, temporary, separators=(",", ":"))
            temporary.write("\n")
            temporary.flush()
            os.fsync(temporary.fileno())
            os.replace(pending, path)
        finally:
            pending.unlink(missing_ok=True)
    descriptor = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def write_report(path, healthy, success):
    atomic_json(path, {"format": REPORT_FORMAT, "state": "healthy" if healthy else "degraded", "last_success_at": success})


def add_common_arguments(parser):
    for name in ("config", "maincopyd", "litestream", "rclone", "key", "credentials", "bucket", "prefix"):
        parser.add_argument("--" + name, required=True)
