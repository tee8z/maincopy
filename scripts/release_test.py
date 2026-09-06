#!/usr/bin/env python3
"""Release boundary tests. No registry uploads or repository changes."""

import contextlib
import copy
import hashlib
import json
import os
import struct
import subprocess
import sys
import tempfile
import threading
import unittest
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch

import release
import release_credential


def identity():
    return {
        "format": "maincopy-release-v1",
        "version": "1.2.3",
        "tag": "v1.2.3",
        "commit": "a" * 40,
        "tag_object": "b" * 40,
        "crates": dict.fromkeys(release.PACKAGES, "c" * 64),
    }


def artifacts(directory):
    manifest = identity()
    for name in release.artifact_names(manifest):
        (directory / name).write_text(name + "\n")
    manifest["crates"] = {
        name: release.checksum(directory / f"{name}-1.2.3.crate")
        for name in release.PACKAGES
    }
    manifest["source_sha256"] = release.checksum(
        directory / "maincopy-1.2.3-source.tar.gz"
    )
    release.write_json(directory / "release.json", manifest)
    _, lines = release.verify_artifacts(directory, sealed=False)
    (directory / "SHA256SUMS").write_text(lines)
    return manifest


class ArtifactTests(unittest.TestCase):
    def test_seal_covers_exact_artifacts_and_detects_changed_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            manifest = artifacts(directory)
            self.assertEqual(release.verify_artifacts(directory)[0], manifest)
            (directory / "Cargo.lock").write_text("changed")
            with self.assertRaisesRegex(release.ReleaseError, "SHA256SUMS differs"):
                release.verify_artifacts(directory)

    def test_extra_missing_and_symlink_artifacts_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            artifacts(directory)
            extra = directory / "unreviewed"
            extra.touch()
            with self.assertRaisesRegex(release.ReleaseError, "artifact set"):
                release.verify_artifacts(directory)
            extra.unlink()
            lock = directory / "Cargo.lock"
            lock.unlink()
            with self.assertRaisesRegex(release.ReleaseError, "artifact set"):
                release.verify_artifacts(directory)
            lock.symlink_to("flake.lock")
            with self.assertRaisesRegex(release.ReleaseError, "regular file"):
                release.verify_artifacts(directory)

    def test_partial_registry_retry_skips_only_matching_versions(self):
        manifest = identity()
        existing = {release.PACKAGES[0]: "c" * 64}
        self.assertEqual(
            release.unpublished_packages(
                manifest, lambda name, version: existing.get(name)
            ),
            list(release.PACKAGES[1:]),
        )
        existing[release.PACKAGES[-1]] = "d" * 64
        with self.assertRaisesRegex(release.ReleaseError, "different bytes"):
            release.unpublished_packages(
                manifest, lambda name, version: existing.get(name)
            )


class CredentialTests(unittest.TestCase):
    def request(self):
        return {
            "v": 1,
            "kind": "get",
            "operation": "publish",
            "name": release.PACKAGES[0],
            "vers": "1.2.3",
            "cksum": "c" * 64,
            "args": [release.CREDENTIAL_POLICY],
            "registry": {"index-url": "sparse+https://index.crates.io/"},
        }

    def test_only_exact_prepared_publication_receives_uncached_credentials(self):
        request = self.request()
        response = release_credential.credential(request, identity(), "fixture-token")
        self.assertEqual(response["Ok"]["cache"], "never")
        self.assertFalse(response["Ok"]["operation_independent"])
        cases = [
            ("cksum", "d" * 64),
            ("vers", "1.2.4"),
            ("name", "unapproved"),
            ("operation", "yank"),
            ("kind", "login"),
            ("v", 2),
            ("args", []),
            ("registry", {"index-url": "https://another-registry.invalid"}),
        ]
        for key, value in cases:
            with self.subTest(key=key):
                changed = dict(request, **{key: value})
                with self.assertRaises(release.ReleaseError):
                    release_credential.credential(changed, identity(), "fixture-token")

    def test_read_cannot_cache_approval_for_later_publication(self):
        request = dict(self.request(), operation="read")
        response = release_credential.credential(request, identity(), "fixture-token")
        self.assertEqual(response["Ok"]["cache"], "never")
        self.assertFalse(response["Ok"]["operation_independent"])

    def test_actual_cargo_official_registry_has_no_alias_environment_or_global_fallback(
        self,
    ):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            marker = root / "unexpected-provider-ran"
            unwanted = root / "unwanted-provider.py"
            unwanted.write_text(
                "#!/usr/bin/env python3\nfrom pathlib import Path\n"
                f"Path({str(marker)!r}).touch()\n"
                "print('{\"v\":[1]}', flush=True)\n"
                'print(\'{"Err":{"kind":"other","message":"unwanted provider"}}\', flush=True)\n'
            )
            unwanted.chmod(0o700)
            provider = str(Path(release_credential.__file__).resolve())
            # Cargo looks up the alias through dotted configuration keys.
            alias_key = ".".join(json.dumps(part) for part in provider.split("."))
            manifest = root / "release.json"
            release.write_json(manifest, identity())
            environment = dict(
                os.environ,
                CARGO_HOME=str(root / "cargo-home"),
                CARGO_REGISTRY_CREDENTIAL_PROVIDER=str(unwanted),
                CARGO_REGISTRIES_CRATES_IO_CREDENTIAL_PROVIDER=str(unwanted),
                MAINCOPY_RELEASE_MANIFEST=str(manifest),
            )
            for value in (str(unwanted), [str(unwanted)]):
                configuration = root / "config.toml"
                configuration.write_text(
                    "[registry]\ncredential-provider=" + json.dumps(value) + "\n"
                    "global-credential-providers=" + json.dumps([str(unwanted)]) + "\n"
                    "[registries.crates-io]\ncredential-provider="
                    + json.dumps(str(unwanted))
                    + "\n"
                    "[credential-alias]\n"
                    + alias_key
                    + "="
                    + json.dumps([str(unwanted)])
                    + "\n"
                )
                result = subprocess.run(
                    [
                        "cargo",
                        "--config",
                        str(configuration),
                        "--config",
                        release.credential_configuration(),
                        "logout",
                        "--registry",
                        "crates-io",
                    ],
                    cwd=root,
                    env=environment,
                    capture_output=True,
                    timeout=10,
                    check=False,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(
                    marker.exists(), "Cargo executed an unapproved credential provider"
                )
                if isinstance(value, str):
                    self.assertIn(
                        b"Maincopy release credential policy rejected", result.stderr
                    )
                    # This control proves that the alias would redirect an
                    # otherwise identical invocation without the fixed argument.
                    subprocess.run(
                        [
                            "cargo",
                            "--config",
                            str(configuration),
                            "--config",
                            "registry.credential-provider=" + json.dumps(provider),
                            "logout",
                            "--registry",
                            "crates-io",
                        ],
                        cwd=root,
                        env=environment,
                        capture_output=True,
                        timeout=10,
                        check=False,
                    )
                    self.assertTrue(marker.exists())
                    marker.unlink()

    def test_real_provider_protocol_rejects_oversize_and_mismatch_without_token(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "release.json"
            release.write_json(path, identity())
            environment = dict(
                os.environ,
                MAINCOPY_RELEASE_MANIFEST=str(path),
                MAINCOPY_RELEASE_TOKEN="fixture-private-token",
            )
            request = self.request()
            valid = json.dumps(request).encode() + b"\n"
            request["cksum"] = "0" * 64
            for raw in (
                valid,
                json.dumps(request).encode() + b"\n",
                b"x" * 65537 + b"\n",
                b"{}",
            ):
                result = subprocess.run(
                    [
                        sys.executable,
                        str(Path(release_credential.__file__)),
                        "--cargo-plugin",
                    ],
                    input=raw,
                    capture_output=True,
                    env=environment,
                    timeout=10,
                    check=True,
                )
                lines = [json.loads(line) for line in result.stdout.splitlines()]
                self.assertEqual(lines[0], {"v": [1]})
                self.assertEqual(len(lines), 2)
                self.assertEqual(result.stderr, b"")
                if raw == valid:
                    self.assertEqual(lines[1]["Ok"]["token"], "fixture-private-token")
                else:
                    self.assertIn("Err", lines[1])
                    self.assertNotIn(b"fixture-private-token", result.stdout)


class GithubFixture:
    def __init__(self, directory):
        self.directory = directory
        self.value = None
        self.assets = {}
        self.calls = []
        self.fail_asset = None

    def __call__(self, url, *, method="GET", data=None, missing=False):
        self.calls.append((method, url))
        path = urllib.parse.urlparse(url).path
        if method == "GET" and "/git/ref/tags/" in path:
            return {
                "ref": "refs/tags/v1.2.3",
                "object": {"type": "tag", "sha": "b" * 40},
            }
        if method == "GET" and "/releases/tags/" in path:
            return copy.deepcopy(self.value)
        if method == "POST" and path.endswith("/releases"):
            self.value = dict(data, id=123, immutable=False)
            return copy.deepcopy(self.value)
        if method == "GET" and path.endswith("/assets"):
            return list(copy.deepcopy(self.assets).values())
        if method == "POST" and path.endswith("/assets"):
            name = urllib.parse.parse_qs(urllib.parse.urlparse(url).query)["name"][0]
            if name == self.fail_asset:
                raise release.ReleaseError("fixture interrupted upload")
            if name in self.assets:
                raise AssertionError("workflow attempted to overwrite an asset")
            self.assets[name] = {
                "name": name,
                "size": len(data),
                "state": "uploaded",
                "digest": "sha256:" + release.checksum(self.directory / name),
            }
            return copy.deepcopy(self.assets[name])
        if method == "PATCH":
            self.assert_complete()
            self.value.update(draft=False, immutable=True)
            return copy.deepcopy(self.value)
        raise AssertionError("unexpected GitHub operation")

    def assert_complete(self):
        assert set(self.assets) == {path.name for path in self.directory.iterdir()}


class GithubTests(unittest.TestCase):
    def test_interrupted_upload_remains_draft_then_retry_finishes_and_replay_is_read_only(
        self,
    ):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            manifest = artifacts(directory)
            api = GithubFixture(directory)
            api.fail_asset = "maincopy-cli-1.2.3.crate"
            with self.assertRaisesRegex(release.ReleaseError, "interrupted"):
                release.publish_github(directory, manifest, api)
            self.assertTrue(api.value["draft"])
            self.assertTrue(api.assets)
            self.assertFalse(any(method == "PATCH" for method, _ in api.calls))
            api.fail_asset = None
            release.publish_github(directory, manifest, api)
            self.assertFalse(api.value["draft"])
            api.assert_complete()
            api.calls.clear()
            release.publish_github(directory, manifest, api)
            self.assertTrue(all(method == "GET" for method, _ in api.calls))

    def test_existing_changed_or_incomplete_asset_stops_before_new_upload(self):
        for changes in (
            {"digest": "sha256:" + "0" * 64},
            {"state": "starter"},
            {"size": 0},
        ):
            with (
                self.subTest(changes=changes),
                tempfile.TemporaryDirectory() as temporary,
            ):
                directory = Path(temporary)
                manifest = artifacts(directory)
                api = GithubFixture(directory)
                release.publish_github(directory, manifest, api)
                api.value["draft"] = True
                api.assets["Cargo.lock"].update(changes)
                del api.assets["SHA256SUMS"]
                api.calls.clear()
                with self.assertRaises(release.ReleaseError):
                    release.publish_github(directory, manifest, api)
                self.assertTrue(all(method == "GET" for method, _ in api.calls))

    def test_unexpected_asset_or_missing_published_asset_requires_investigation(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            manifest = artifacts(directory)
            api = GithubFixture(directory)
            release.publish_github(directory, manifest, api)
            api.assets["unexpected"] = dict(api.assets["Cargo.lock"], name="unexpected")
            with self.assertRaisesRegex(release.ReleaseError, "unexpected"):
                release.publish_github(directory, manifest, api)
            del api.assets["unexpected"]
            del api.assets["SHA256SUMS"]
            with self.assertRaisesRegex(
                release.ReleaseError, "published release is missing"
            ):
                release.publish_github(directory, manifest, api)


class HttpTests(unittest.TestCase):
    def test_only_confirmed_404_means_absent_and_redirects_are_rejected(self):
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                code = int(self.path[1:])
                self.send_response(code)
                if code == 302:
                    self.send_header("Location", "/200")
                self.end_headers()
                self.wfile.write(b'{"ok":true}')

            def log_message(self, *arguments):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            base = f"http://127.0.0.1:{server.server_port}"
            self.assertEqual(
                release.request_json(base + "/200", missing=True), {"ok": True}
            )
            self.assertIsNone(release.request_json(base + "/404", missing=True))
            for status in (302, 403, 429, 500):
                with (
                    self.subTest(status=status),
                    self.assertRaises(release.ReleaseError),
                ):
                    release.request_json(base + f"/{status}", missing=True)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
            self.assertFalse(thread.is_alive())


class CargoBoundaryTests(unittest.TestCase):
    def test_actual_cargo_checksums_gate_upload_before_the_local_registry_receives_it(
        self,
    ):
        uploads = []
        index = {}
        archives = {}

        class Registry(BaseHTTPRequestHandler):
            def do_GET(self):
                name = self.path.rsplit("/", 1)[-1]
                if self.path == "/config.json":
                    base = f"http://127.0.0.1:{self.server.server_port}"
                    payload = json.dumps(
                        {"dl": base + "/downloads/{crate}/{version}", "api": base}
                    ).encode()
                elif self.path.startswith("/downloads/"):
                    payload = archives[self.path.split("/")[2]]
                elif name in index:
                    payload = json.dumps(index[name]).encode() + b"\n"
                else:
                    self.send_response(404)
                    self.end_headers()
                    return
                self.send_response(200)
                self.end_headers()
                self.wfile.write(payload)

            def do_PUT(self):
                length = int(self.headers["Content-Length"])
                if self.path != "/api/v1/crates/new" or length > 1024 * 1024:
                    self.send_response(400)
                    self.end_headers()
                    return
                body = self.rfile.read(length)
                metadata_length = struct.unpack("<I", body[:4])[0]
                metadata = json.loads(body[4 : 4 + metadata_length])
                archive_start = 8 + metadata_length
                archive = body[archive_start:]
                dependencies = [
                    {
                        "name": item["name"],
                        "req": item["version_req"],
                        "features": item["features"],
                        "optional": item["optional"],
                        "default_features": item["default_features"],
                        "target": item["target"],
                        "kind": item["kind"],
                    }
                    for item in metadata["deps"]
                ]
                if (
                    any(item["name"] not in index for item in dependencies)
                    or metadata["name"] in index
                ):
                    self.send_response(409)
                    self.end_headers()
                    return
                uploads.append(metadata["name"])
                archives[metadata["name"]] = archive
                index[metadata["name"]] = {
                    "name": metadata["name"],
                    "vers": metadata["vers"],
                    "deps": dependencies,
                    "features": {},
                    "yanked": False,
                    "cksum": hashlib.sha256(archive).hexdigest(),
                }
                self.send_response(200)
                self.end_headers()
                self.wfile.write(
                    b'{"warnings":{"invalid_categories":[],"invalid_badges":[],"other":[]}}'
                )

            def log_message(self, *arguments):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Registry)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                project = root / "project"
                project.mkdir()
                (project / "Cargo.toml").write_text(
                    "[workspace]\nresolver='2'\nmembers="
                    + json.dumps(release.PACKAGES)
                    + "\n"
                )
                prerequisites = {
                    "maincopy-cli": ["maincopy-shared"],
                    "maincopy-server": list(release.PACKAGES[:3]),
                }
                for name in release.PACKAGES:
                    crate = project / name
                    (crate / "src").mkdir(parents=True)
                    (crate / "src/lib.rs").write_text(
                        "pub fn fixture() -> bool { true }\n"
                    )
                    dependencies = "".join(
                        f"{dependency}={{path='../{dependency}',version='=1.2.3',registry='fixture'}}\n"
                        for dependency in prerequisites.get(name, [])
                    )
                    (crate / "Cargo.toml").write_text(
                        f"[package]\nname='{name}'\nversion='1.2.3'\nedition='2021'\n"
                        "description='Local release protocol fixture'\nlicense='MIT'\n[dependencies]\n"
                        + dependencies
                    )
                manifest_path = root / "release.json"
                release.write_json(manifest_path, identity())
                wrapper = root / "provider.py"
                # Only this fixture maps its loopback registry to the production
                # policy's crates.io identity. Production rejects other registries.
                wrapper.write_text(
                    "#!/usr/bin/env python3\nimport json, os, sys\n"
                    f"sys.path.insert(0, {str(Path(release.__file__).parent)!r})\n"
                    "from release import read_json, ReleaseError\n"
                    "from release_credential import credential\n"
                    "print(json.dumps({'v':[1]}), flush=True)\n"
                    "request=json.loads(sys.stdin.readline())\n"
                    "request['registry']['index-url']='sparse+https://index.crates.io/'\n"
                    "try:\n"
                    "    response=credential(request, read_json(os.environ['MAINCOPY_RELEASE_MANIFEST']), 'local-fixture-token')\n"
                    "except ReleaseError:\n"
                    "    response={'Err':{'kind':'other','message':'fixture checksum rejected'}}\n"
                    "print(json.dumps(response), flush=True)\n"
                )
                wrapper.chmod(0o700)
                configuration = root / "config.toml"
                configuration.write_text(
                    f'[registries.fixture]\nindex="sparse+http://127.0.0.1:{server.server_port}/"\n'
                    "credential-provider="
                    + json.dumps([str(wrapper), release.CREDENTIAL_POLICY])
                    + "\n"
                )
                environment = dict(
                    os.environ,
                    CARGO_HOME=str(root / "cargo-home"),
                    MAINCOPY_RELEASE_MANIFEST=str(manifest_path),
                )

                def cargo(*arguments):
                    return subprocess.run(
                        ["cargo", "--config", str(configuration), *arguments],
                        cwd=project,
                        env=environment,
                        capture_output=True,
                        timeout=60,
                        check=False,
                    )

                self.assertEqual(cargo("generate-lockfile", "--offline").returncode, 0)
                target = root / "target"
                arguments = [
                    "publish",
                    "--locked",
                    "--registry",
                    "fixture",
                    "--target-dir",
                    str(target),
                ]
                dry_run = cargo(*arguments, "--workspace", "--dry-run")
                self.assertEqual(
                    dry_run.returncode, 0, "tiny local-registry Cargo dry run failed"
                )
                expected = {
                    name: release.checksum(
                        target / f"package/tmp-crate/{name}-1.2.3.crate"
                    )
                    for name in release.PACKAGES
                }
                rejected = cargo(*arguments, "--workspace")
                self.assertNotEqual(rejected.returncode, 0)
                self.assertIn(b"fixture checksum rejected", rejected.stderr)
                self.assertEqual(uploads, [])
                manifest = identity()
                manifest["crates"] = dict(expected, **{"maincopy-server": "0" * 64})
                release.write_json(manifest_path, manifest)
                interrupted = cargo(*arguments, "--workspace")
                self.assertNotEqual(interrupted.returncode, 0)
                self.assertIn(b"fixture checksum rejected", interrupted.stderr)
                self.assertGreater(len(uploads), 0)
                self.assertLess(len(uploads), 5)
                manifest["crates"] = expected
                release.write_json(manifest_path, manifest)
                pending = release.unpublished_packages(
                    manifest, lambda name, version: index.get(name, {}).get("cksum")
                )
                selection = [
                    argument for name in pending for argument in ("--package", name)
                ]
                accepted = cargo(*arguments, *selection)
                self.assertEqual(
                    accepted.returncode, 0, "tiny local-registry approved retry failed"
                )
                self.assertEqual(set(uploads), set(release.PACKAGES))
                self.assertEqual(len(uploads), 5)
                self.assertEqual(
                    {
                        name: hashlib.sha256(data).hexdigest()
                        for name, data in archives.items()
                    },
                    expected,
                )
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
            self.assertFalse(thread.is_alive())


class SignedTagTests(unittest.TestCase):
    def test_real_gpg_signatures_require_trusted_primary_and_exact_tag_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            keyring = root / "keyring"
            keyring.mkdir(mode=0o700)
            repository = root / "repository"
            repository.mkdir()
            environment = dict(os.environ, GNUPGHOME=str(keyring))

            def run(*arguments):
                return subprocess.run(
                    arguments,
                    cwd=repository,
                    env=environment,
                    capture_output=True,
                    text=True,
                    check=True,
                ).stdout.strip()

            run(
                "gpg",
                "--batch",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                "",
                "--quick-generate-key",
                "Release fixture <release@example.invalid>",
                "ed25519",
                "sign",
                "1d",
            )
            fingerprint = next(
                line.split(":")[9]
                for line in run("gpg", "--with-colons", "--list-keys").splitlines()
                if line.startswith("fpr:")
            )
            public = run("gpg", "--armor", "--export", fingerprint)
            run("git", "init", "--initial-branch=master")
            for key, value in (
                ("user.name", "Release fixture"),
                ("user.email", "release@example.invalid"),
                ("user.signingkey", fingerprint),
                ("gpg.format", "openpgp"),
            ):
                run("git", "config", key, value)
            (repository / "LICENSE").write_text("fixture license")
            members = [f"crates/{name}" for name in release.PACKAGES]
            (repository / "Cargo.toml").write_text(
                "[workspace]\nmembers = "
                + json.dumps(members)
                + "\n[workspace.package]\nversion = '1.2.3'\n[workspace.dependencies]\n"
            )
            lock = []
            for name, member in zip(release.PACKAGES, members):
                crate = repository / member
                crate.mkdir(parents=True)
                (crate / "LICENSE").symlink_to("../../LICENSE")
                (crate / "Cargo.toml").write_text(
                    f"[package]\nname = '{name}'\nversion.workspace = true\n"
                )
                lock.append(f"[[package]]\nname = '{name}'\nversion = '1.2.3'\n")
            (repository / "Cargo.lock").write_text("\n".join(lock))
            run("git", "add", ".")
            run("git", "commit", "-S", "-m", "test(release): create signed fixture")
            run("git", "update-ref", "refs/remotes/origin/master", "HEAD")
            run("git", "tag", "-s", "v1.2.3", "-m", "Fixture release")
            with (
                contextlib.chdir(repository),
                patch.dict(
                    os.environ,
                    RELEASE_GPG_PUBLIC_KEY=public,
                    RELEASE_GPG_FINGERPRINT=fingerprint,
                ),
            ):
                self.assertEqual(
                    release.verify_tag("v1.2.3")["commit"],
                    run("git", "rev-parse", "HEAD"),
                )
                with (
                    patch.dict(os.environ, RELEASE_GPG_FINGERPRINT="0" * 40),
                    self.assertRaisesRegex(
                        release.ReleaseError, "configured primary key"
                    ),
                ):
                    release.verify_tag("v1.2.3")
                with self.assertRaisesRegex(release.ReleaseError, "workspace version"):
                    release.verify_tag("v1.2.4")
                run(
                    "git",
                    "tag",
                    "-s",
                    "wrong-name",
                    "-m",
                    "Misleading message\n\ntag v1.2.3\n",
                )
                run("git", "verify-tag", "wrong-name")
                run("git", "update-ref", "refs/tags/v1.2.3", "refs/tags/wrong-name")
                with self.assertRaisesRegex(
                    release.ReleaseError, "signed tag name differs"
                ):
                    release.verify_tag("v1.2.3")
                run("git", "update-ref", "refs/tags/v1.2.3", "HEAD")
                with self.assertRaisesRegex(
                    release.ReleaseError, "annotated and signed"
                ):
                    release.verify_tag("v1.2.3")


if __name__ == "__main__":
    unittest.main()
