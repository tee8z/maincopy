#!/usr/bin/env python3
"""Real Litestream replay and rclone crypt with a local B2 transport fixture."""
import argparse
import base64
import ctypes
from contextlib import closing
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import select
import sqlite3
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

parser = argparse.ArgumentParser()
parser.add_argument("--scripts", required=True)
parser.add_argument("--litestream", required=True)
parser.add_argument("--rclone", required=True)
options, remaining = parser.parse_known_args()
sys.argv[1:] = remaining
sys.path.insert(0, options.scripts)
from backup_common import BackupFailure, encoded_paths, manifest_inventory, protected_directory, rclone, run, runtime_config
spec = importlib.util.spec_from_file_location("backup", str(Path(options.scripts) / "checkpoint-backup.py"))
backup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(backup)
os.umask(0o077)


class ReplicaStartup(unittest.TestCase):
    def test_guard_to_marker_publication_cannot_look_absent(self):
        spec = importlib.util.spec_from_file_location("replica_start", str(Path(options.scripts) / "replica-start.py"))
        replica_start = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(replica_start)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            marker, guard = root / "database.restore.json", root / "database.restore-pending"
            guard.touch()

            class CompletingAcceptance(type(marker)):
                def lstat(self):
                    # Model the problematic interleaving: a marker observation
                    # sees absence just before acceptance publishes that marker
                    # and removes its guard. Guard-first inspection must block.
                    if self == marker:
                        marker.write_text("accepted")
                        guard.unlink()
                        raise FileNotFoundError
                    return super().lstat()

            self.assertTrue(replica_start.restoration_pending(CompletingAcceptance(marker)))

    def command(self, marker, output):
        return [
            sys.executable, str(Path(options.scripts) / "replica-start.py"),
            "--marker", str(marker), "--timeout-seconds", "1", "--",
            sys.executable, "-c",
            "from pathlib import Path; import sys; Path(sys.argv[1]).write_text('started')",
            str(output),
        ]

    def test_pending_acceptance_defers_exec_until_marker_is_consumed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            marker, output = root / "database.restore.json", root / "native-started"
            marker.write_text("pending acceptance")
            ready, notify = os.pipe()
            command = self.command(marker, output)
            runner = """
import importlib.util, os, sys
spec = importlib.util.spec_from_file_location("replica_start", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
notification = int(sys.argv[2])
native_select = module.select.select
def observed_select(*arguments):
    global notification
    if notification is not None:
        os.write(notification, b"1")
        os.close(notification)
        notification = None
    return native_select(*arguments)
module.select.select = observed_select
sys.argv = [sys.argv[1], *sys.argv[3:]]
sys.exit(module.main())
"""
            process = subprocess.Popen([sys.executable, "-c", runner, command[1], str(notify), *command[2:]],
                                       pass_fds=(notify,), stderr=subprocess.PIPE)
            os.close(notify)
            try:
                self.assertTrue(select.select([ready], [], [], 3)[0])
                self.assertEqual(os.read(ready, 1), b"1")
                self.assertIsNone(process.poll())
                self.assertFalse(output.exists())
                marker.unlink()
                _, error = process.communicate(timeout=3)
                self.assertEqual(process.returncode, 0, error)
                self.assertEqual(output.read_text(), "started")
            finally:
                os.close(ready)
                if process.poll() is None:
                    process.kill()
                    process.communicate()

    def test_pending_or_broken_symlink_acceptance_times_out_without_exec(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            marker, output = root / "database.restore.json", root / "native-started"
            for kind in ("regular", "broken symlink", "interrupted acceptance"):
                with self.subTest(kind=kind):
                    pending = marker
                    if kind == "regular":
                        marker.write_text("pending acceptance")
                    elif kind == "broken symlink":
                        marker.symlink_to(root / "missing")
                    else:
                        pending = root / "database.restore-pending"
                        pending.write_text("interrupted acceptance")
                    result = subprocess.run(self.command(marker, output), capture_output=True, timeout=3)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(b"restore acceptance is still pending", result.stderr)
                    self.assertFalse(output.exists())
                    pending.unlink()


class CheckpointOperations(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.state = self.root / "backup"
        self.state.mkdir(mode=0o700)
        self.artifacts = self.root / "artifacts"
        self.artifacts.mkdir(mode=0o700)
        self.archive = self.artifacts / ("content-b3-v1-" + hashlib.sha256(b"immutable candidate").hexdigest() + ".candidate")
        self.archive.write_bytes(b"immutable candidate")
        self.database = self.root / "live.db"
        self.connection = sqlite3.connect(self.database)
        self.connection.execute("PRAGMA journal_mode=WAL")
        self.connection.execute("CREATE TABLE evidence(value TEXT)")
        self.connection.execute("CREATE TABLE required_artifacts(digest TEXT)")
        self.connection.execute("INSERT INTO evidence VALUES ('first complete checkpoint')")
        self.connection.commit()
        self.replica = self.root / "replica"
        configuration = self.root / "litestream.yml"
        configuration.write_text(f"socket:\n  enabled: true\n  path: {self.root}/control.sock\nlogging:\n  level: error\ndbs:\n  - path: {self.database}\n    monitor-interval: 50ms\n    meta-path: {self.root}/metadata\n    replica:\n      type: file\n      path: {self.replica}\n      sync-interval: 50ms\n")
        self.replication_log = (self.root / "replication.log").open("wb")
        self.start_replication(configuration)
        self.wait_for_plan()
        self.key = self.root / "key"
        self.key.write_bytes(base64.b64encode(os.urandom(32)) + b"\n")
        self.credentials = self.root / "b2.conf"
        self.credentials.write_text("[maincopy-b2]\ntype=b2\naccount=fixture-account\nkey=fixture-secret-key\n")
        self.offsite = self.root / "offsite"
        self.offsite.mkdir()
        self.fail_transfer = self.root / "fail-transfer"
        self.rclone = self.program("rclone-fixture", f'''
            import configparser, os, pathlib, sys, tempfile
            args = sys.argv[1:]
            assert "fixture-secret-key" not in " ".join(args)
            network = any(value.startswith("maincopy-b2:") for value in args)
            if network:
                if pathlib.Path({str(self.fail_transfer)!r}).exists():
                    sys.exit(41)
                if "--files-from" in args:
                    listing = pathlib.Path(args[args.index("--files-from") + 1]).read_text().splitlines()
                    source = pathlib.Path(args[-2])
                    for name in listing:
                        assert (source / name).read_bytes().startswith(b"RCLONE\\x00\\x00")
                else:
                    assert pathlib.Path(args[-2]).read_bytes().startswith(b"RCLONE\\x00\\x00")
                args = [{str(self.offsite)!r} + value.split("maincopy-b2:fixture-bucket/maincopy", 1)[1] if value.startswith("maincopy-b2:") else value for value in args]
            if "--config" in args:
                config_path = pathlib.Path(args[args.index("--config") + 1])
                config = configparser.ConfigParser(interpolation=None)
                config.read(config_path)
                config["offsitecrypt"]["remote"] = {str(self.offsite)!r}
                with config_path.open("w") as output:
                    config.write(output)
            os.execv({options.rclone!r}, [{options.rclone!r}, *args])
        ''')
        # Typed Rust checkpoint validation has its own tests. This fixture emits
        # the agreed format and independently verifies hashes so these tests
        # exercise the native capture/encryption/publication/replay boundaries.
        self.maincopyd = self.program("maincopy-fixture", '''
            from contextlib import closing
            import hashlib, json, pathlib, sqlite3, sys
            args = sys.argv[1:]
            command = args[2]
            def value(flag): return pathlib.Path(args[args.index(flag) + 1])
            def identity(path): return {"bytes": path.stat().st_size, "digest": hashlib.sha256(path.read_bytes()).hexdigest()}
            if command == "checkpoint-manifest":
                with closing(sqlite3.connect(value("--database-file"))) as database:
                    for (digest,) in database.execute("SELECT digest FROM required_artifacts"):
                        assert (value("--artifact-root") / ("content-b3-v1-" + digest + ".candidate")).exists()
                plan = json.loads(value("--plan-file").read_text())
                for item in plan["files"]:
                    item["file"] = identity(value("--ltx-root") / str(item["level"]) / item["name"])
                manifest = {"format": "maincopy-litestream-checkpoint-v1", "database": identity(value("--database-file")), "min_txid": plan["min_txid"], "max_txid": plan["max_txid"], "files": plan["files"], "artifacts": [{"name": path.name, "file": identity(path)} for path in value("--artifact-root").iterdir()]}
                value("--output").write_text(json.dumps(manifest))
            else:
                manifest = json.loads(value("--manifest-file").read_text())
                for item in manifest["files"]:
                    assert identity(value("--ltx-root") / str(item["level"]) / item["name"]) == item["file"]
                if command == "verify-checkpoint":
                    print(json.dumps({"max_txid": manifest["max_txid"]}))
                elif command == "restore-replica":
                    assert identity(value("--database-file")) == manifest["database"]
                    value("--database-file").with_suffix(".accepted").write_text("verified replay")
        ''')
        self.args = argparse.Namespace(config=str(self.root / "host.toml"), directory=str(self.state),
            maincopyd=str(self.maincopyd), litestream=options.litestream, rclone=str(self.rclone),
            key=str(self.key), credentials=str(self.credentials), bucket="fixture-bucket", prefix="maincopy",
            replica=str(self.replica), socket=str(self.root / "control.sock"), database=str(self.database), artifacts=str(self.artifacts), status_file=str(self.state / "backup-status.json"))

    def stop_replication(self):
        if self.replication.poll() is None:
            self.replication.terminate()
            self.replication.wait(timeout=15)

    def start_replication(self, configuration):
        # Install a filesystem notification before native startup. Socket
        # creation is the readiness boundary, followed by native sync -wait.
        libc = ctypes.CDLL(None, use_errno=True)
        libc.inotify_init1.argtypes = [ctypes.c_int]
        libc.inotify_add_watch.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_uint32]
        descriptor = libc.inotify_init1(os.O_CLOEXEC | os.O_NONBLOCK)
        self.assertGreaterEqual(descriptor, 0)
        try:
            self.assertGreaterEqual(libc.inotify_add_watch(descriptor, os.fsencode(self.root), 0x100 | 0x80), 0)
            self.replication = subprocess.Popen([options.litestream, "replicate", "-config", str(configuration)], stdout=self.replication_log, stderr=self.replication_log)
            self.addCleanup(self.stop_replication)
            deadline = time.monotonic() + 10
            while not (self.root / "control.sock").is_socket():
                remaining = deadline - time.monotonic()
                self.assertGreater(remaining, 0, "Litestream control socket readiness timed out")
                self.assertTrue(select.select([descriptor], [], [], remaining)[0], "Litestream control socket readiness timed out")
                os.read(descriptor, 4096)
        finally:
            os.close(descriptor)

    def tearDown(self):
        self.stop_replication()
        self.replication_log.close()
        self.connection.close()
        self.temporary.cleanup()

    def program(self, name, source):
        path = self.root / name
        path.write_text(f"#!{sys.executable}\n" + textwrap.dedent(source))
        path.chmod(0o700)
        return path

    def wait_for_plan(self, previous=None):
        confirmed = json.loads(run([options.litestream, "sync", "-wait", "-json", "-timeout", "10",
                                    "-socket", str(self.root / "control.sock"), str(self.database)],
                                   "fixture replica synchronization", timeout=15, maximum=1024))
        self.assertGreaterEqual(confirmed["replica_txid"], confirmed["txid"])
        plan = json.loads(run([options.litestream, "restore", "-dry-run", "-json", "-o", str(self.root / "planned.db"),
                               "file://" + str(self.replica)], "fixture replica plan", timeout=15))
        if previous is not None:
            self.assertNotEqual(plan["max_txid"], previous)
        return plan

    def execute_backup(self):
        command = [sys.executable, str(Path(options.scripts) / "checkpoint-backup.py")]
        for key, value in vars(self.args).items():
            command += ["--" + key.replace("_", "-"), str(value)]
        return subprocess.run(command, capture_output=True, text=True)

    def recover(self, name):
        command = [sys.executable, str(Path(options.scripts) / "checkpoint-restore.py")]
        for key in ("config", "maincopyd", "litestream", "rclone", "key", "credentials", "bucket", "prefix"):
            command += ["--" + key, str(getattr(self.args, key))]
        destination = self.root / name
        command += ["--directory", str(destination)]
        result = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((destination / "database.accepted").exists())
        with closing(sqlite3.connect(destination / "database.sqlite3")) as connection:
            return connection.execute("SELECT value FROM evidence").fetchone()[0]

    def test_native_checkpoint_is_encrypted_offsite_and_replays(self):
        result = self.execute_backup()
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(Path(self.args.status_file).read_text())
        self.assertEqual(report["state"], "healthy")
        self.assertEqual(self.recover("recovered"), "first complete checkpoint")
        self.assertFalse(list(self.state.glob(".capture-*")))
        retained = list((self.state / "checkpoints").iterdir())
        self.assertEqual(len(retained), 1)
        for path in retained[0].rglob("*"):
            if path.is_file():
                self.assertTrue(path.read_bytes().startswith(b"RCLONE\x00\x00"))
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_interrupted_upload_keeps_previous_complete_recovery_and_health_time(self):
        self.assertEqual(self.execute_backup().returncode, 0)
        before = json.loads(Path(self.args.status_file).read_text())
        previous = self.wait_for_plan()["max_txid"]
        self.connection.execute("UPDATE evidence SET value = 'unpublished second checkpoint'")
        self.connection.commit()
        self.wait_for_plan(previous)
        self.fail_transfer.touch()
        failed = self.execute_backup()
        self.assertEqual(failed.returncode, 1)
        self.assertNotIn("fixture-secret-key", failed.stderr)
        after = json.loads(Path(self.args.status_file).read_text())
        self.assertEqual(after["state"], "degraded")
        self.assertEqual(after["last_success_at"], before["last_success_at"])
        self.fail_transfer.unlink()
        self.assertEqual(self.recover("previous"), "first complete checkpoint")

    def test_corrupt_cached_ciphertext_cannot_publish_a_checkpoint(self):
        self.assertEqual(self.execute_backup().returncode, 0)
        with tempfile.TemporaryDirectory(dir=self.root) as temporary:
            config = runtime_config(self.args, Path(temporary), self.state / "encrypted-objects")
            encoded = encoded_paths(self.args, config, ["objects/" + hashlib.sha256(self.archive.read_bytes()).hexdigest()])[0]
        cache_file = self.state / "encrypted-objects" / encoded
        with cache_file.open("r+b") as output:
            output.seek(-1, 2)
            original = output.read(1)
            output.seek(-1, 2)
            output.write(bytes([original[0] ^ 1]))
        failed = self.execute_backup()
        self.assertEqual(failed.returncode, 1)
        self.assertEqual(json.loads(Path(self.args.status_file).read_text())["state"], "degraded")
        self.assertEqual(self.recover("previous"), "first complete checkpoint")

    def test_retained_checkpoints_share_ciphertext_and_prune_only_old_directories(self):
        self.assertEqual(self.execute_backup().returncode, 0)
        cache = self.state / "encrypted-objects"
        with tempfile.TemporaryDirectory(dir=self.root) as temporary:
            config = runtime_config(self.args, Path(temporary), cache)
            encoded = encoded_paths(self.args, config, ["objects/" + hashlib.sha256(self.archive.read_bytes()).hexdigest()])[0]
        source = cache / encoded
        inode = source.stat().st_ino
        retained = self.state / "checkpoints"
        old = retained / "20200101T000000Z-11111111-1111-4111-8111-111111111111"
        old.mkdir()
        os.link(source, old / "ciphertext")
        unrelated = retained / "operator-notes"
        unrelated.mkdir()
        self.assertEqual(self.execute_backup().returncode, 0)
        self.assertFalse(old.exists())
        self.assertTrue(unrelated.exists())
        links = [path for path in retained.rglob("*") if path.is_file() and path.stat().st_ino == inode]
        self.assertEqual(len(links), 2)
        self.assertGreaterEqual(source.stat().st_nlink, 3)

    def test_stopped_replica_cannot_refresh_an_old_complete_checkpoint(self):
        self.assertEqual(self.execute_backup().returncode, 0)
        previous = json.loads(Path(self.args.status_file).read_text())
        self.stop_replication()
        self.connection.execute("UPDATE evidence SET value = 'not replicated'")
        self.connection.commit()
        failed = self.execute_backup()
        self.assertEqual(failed.returncode, 1)
        report = json.loads(Path(self.args.status_file).read_text())
        self.assertEqual(report["state"], "degraded")
        self.assertEqual(report["last_success_at"], previous["last_success_at"])
        self.assertEqual(self.recover("previous"), "first complete checkpoint")

    def test_unsynchronized_or_wrong_database_confirmation_is_rejected(self):
        for result in ({"db_path": str(self.database), "txid": 5, "replica_txid": 4},
                       {"db_path": "/wrong/database", "txid": 4, "replica_txid": 4}):
            self.args.litestream = str(self.program("unconfirmed-replica", "print(" + repr(json.dumps(result)) + ")"))
            with self.assertRaises(BackupFailure):
                backup.confirm_replica(self.args)
        self.assertEqual(list(self.offsite.iterdir()), [])

    def test_interrupted_local_retention_is_cleaned_under_the_backup_lock(self):
        retained = self.state / "checkpoints"
        retained.mkdir()
        pending = retained / ".pending-20260101T000000Z-11111111-1111-4111-8111-111111111111"
        pending.mkdir()
        (pending / "partial-ciphertext-link").write_bytes(b"ciphertext")
        backup.cleanup_staging(self.state)
        self.assertFalse(pending.exists())

    def test_missing_required_archive_prevents_complete_publication(self):
        previous = self.wait_for_plan()["max_txid"]
        self.connection.execute("INSERT INTO required_artifacts VALUES (?)", ("1" * 64,))
        self.connection.commit()
        self.wait_for_plan(previous)
        result = self.execute_backup()
        self.assertEqual(result.returncode, 1)
        self.assertEqual(list(self.offsite.iterdir()), [])
        self.assertEqual(json.loads(Path(self.args.status_file).read_text())["state"], "degraded")
        self.assertFalse(list(self.state.glob(".capture-*")))

    def test_plaintext_keys_and_transport_overrides_fail_closed(self):
        for value in (b"", b" ", b"short", base64.b64encode(b"x" * 31)):
            self.key.write_bytes(value)
            self.assertEqual(self.execute_backup().returncode, 1)
        self.key.write_bytes(base64.b64encode(os.urandom(32)))
        self.key.chmod(0o644)
        self.assertEqual(self.execute_backup().returncode, 1)
        self.key.chmod(0o600)
        self.credentials.write_text("[maincopy-b2]\ntype=b2\naccount=a\nkey=b\nendpoint=http://example.test\n")
        self.assertEqual(self.execute_backup().returncode, 1)
        self.assertEqual(list(self.offsite.iterdir()), [])

    def test_unversioned_candidate_names_fail_before_publication(self):
        self.archive.rename(self.artifacts / ("a" * 64 + ".candidate"))
        result = self.execute_backup()
        self.assertEqual(result.returncode, 1)
        self.assertIn("candidate inventory validation", result.stderr)
        self.assertEqual(list(self.offsite.iterdir()), [])

    def test_artifact_remaining_budget_is_checked_before_any_target_copy(self):
        staging = self.root / "aggregate-limit"
        staging.mkdir()
        total = backup.MAX_BYTES - self.archive.stat().st_size + 1
        with self.assertRaises(BackupFailure):
            backup.stage_artifacts(self.args, staging, total)
        self.assertEqual(list((staging / "content-candidates").iterdir()), [])

        # Sparse input exercises the real 1GiB boundary without allocating or
        # copying a gigabyte. Admission must reject it from opened-FD metadata.
        with self.archive.open("wb") as oversized:
            oversized.truncate(backup.MAX_ARTIFACT_BYTES + 1)
        with self.assertRaises(BackupFailure):
            backup.stage_artifacts(self.args, staging, 0)
        self.assertEqual(list((staging / "content-candidates").iterdir()), [])

    def test_copying_a_source_that_grew_after_metadata_is_bounded(self):
        source = self.root / "growing-input"
        source.write_bytes(b"original")
        destination = self.root / "bounded-output"
        with source.open("rb", buffering=0) as incoming, destination.open("xb", buffering=0) as output:
            admitted = os.fstat(incoming.fileno()).st_size
            with source.open("ab") as writer:
                writer.write(b"later data" * 4096)
            with self.assertRaises(BackupFailure):
                backup.copy_exact_input(incoming, output, admitted)
            self.assertEqual(incoming.tell(), admitted + 1)
            self.assertEqual(output.tell(), admitted)
            self.assertEqual(os.fstat(output.fileno()).st_size, admitted)
        self.assertEqual(destination.read_bytes(), b"original")

    def test_clone_or_copy_never_extends_beyond_the_admitted_range(self):
        source = self.root / "growing-reflink-input"
        source.write_bytes(b"original")
        destination = self.root / "bounded-reflink-output"
        with source.open("rb", buffering=0) as incoming, destination.open("xb", buffering=0) as output:
            admitted = os.fstat(incoming.fileno()).st_size
            with source.open("ab") as writer:
                writer.write(b"later data" * 4096)
            with self.assertRaises(BackupFailure):
                backup.copy_pinned_input(incoming, output, admitted)
            self.assertLessEqual(os.fstat(output.fileno()).st_size, admitted)

    def test_child_control_output_and_manifest_paths_are_bounded(self):
        with self.assertRaises(BackupFailure):
            run([sys.executable, "-c", "print('x' * 10000)"], "fixture", maximum=100)
        manifest = self.root / "hostile.json"
        manifest.write_text(json.dumps({"format": "maincopy-litestream-checkpoint-v1", "files": [{"level": 0, "name": "../../escape"}], "artifacts": []}))
        with self.assertRaises(BackupFailure):
            manifest_inventory(manifest)


if __name__ == "__main__":
    unittest.main()
