#!/usr/bin/env python3
"""Prepare and publish the fixed Maincopy release package set."""

import argparse
import gzip
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

PACKAGES = (
    "maincopy-shared",
    "maincopy-diagram-renderer",
    "markdown-compiler",
    "maincopy-cli",
    "maincopy-server",
)
SYSTEMS = ("x86_64-linux", "aarch64-linux")
REPOSITORY = "tee8z/maincopy"
CREDENTIAL_POLICY = "maincopy-release-v1"
JSON_LIMIT = 8 * 1024 * 1024
ARTIFACT_LIMIT = 256 * 1024 * 1024
VERSION = re.compile(
    r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?"
)


class ReleaseError(Exception):
    """A release precondition or external operation failed."""


def require(condition, message):
    if not condition:
        raise ReleaseError(message)


def command(arguments, **kwargs):
    result = subprocess.run(arguments, check=False, **kwargs)
    require(result.returncode == 0, f"{arguments[0]} {arguments[1]} failed")
    return result


def output(arguments, **kwargs):
    return command(arguments, capture_output=True, text=True, **kwargs).stdout.strip()


def read_json(path):
    with Path(path).open("rb") as stream:
        raw = stream.read(JSON_LIMIT + 1)
    require(len(raw) <= JSON_LIMIT, "JSON input exceeds the release limit")
    return json.loads(raw)


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def checksum(path):
    require(
        path.is_file() and not path.is_symlink(),
        "release artifact must be a regular file",
    )
    require(path.stat().st_size <= ARTIFACT_LIMIT, "release artifact exceeds 256 MiB")
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def workspace_version():
    manifest = tomllib.loads(Path("Cargo.toml").read_text())
    workspace = manifest["workspace"]
    version = workspace["package"]["version"]
    require(
        VERSION.fullmatch(version),
        "workspace version must be SemVer without build metadata",
    )
    packages = {}
    for member in workspace["members"]:
        package = tomllib.loads((Path(member) / "Cargo.toml").read_text())["package"]
        require(
            package["version"] == {"workspace": True},
            "all crate versions must inherit the workspace version",
        )
        require(
            package.get("publish", True) in (True, ["crates-io"]),
            "all five crates must be publishable on crates.io",
        )
        require(
            (Path(member) / "LICENSE").read_bytes() == Path("LICENSE").read_bytes(),
            "crate license bytes differ from the root license",
        )
        packages[package["name"]] = member
    require(
        set(packages) == set(PACKAGES),
        "release package set differs from the five approved crates",
    )
    for name, dependency in workspace["dependencies"].items():
        if isinstance(dependency, dict) and "path" in dependency:
            require(
                name in packages and dependency["path"] == packages[name],
                "unknown workspace path dependency",
            )
            require(
                dependency.get("version") == f"={version}",
                "internal dependency version differs from the release version",
            )
    lock = tomllib.loads(Path("Cargo.lock").read_text())
    locked = {
        item["name"]: item["version"]
        for item in lock["package"]
        if "source" not in item
    }
    require(
        locked == dict.fromkeys(PACKAGES, version), "lockfile workspace versions differ"
    )
    return version


def verify_signature(kind, reference, fingerprint, environment):
    result = subprocess.run(
        [
            "git",
            "-c",
            "gpg.format=openpgp",
            "-c",
            "gpg.program=gpg",
            f"verify-{kind}",
            "--raw",
            reference,
        ],
        capture_output=True,
        text=True,
        env=environment,
        check=False,
    )
    valid = [
        line.split()
        for line in result.stderr.splitlines()
        if line.startswith("[GNUPG:] VALIDSIG ")
    ]
    require(
        result.returncode == 0 and len(valid) == 1,
        f"{kind} signature verification failed",
    )
    # GnuPG reports the signing subkey first and the primary fingerprint last.
    require(
        valid[0][-1] == fingerprint,
        f"{kind} was not signed by the configured primary key",
    )


def verify_tag(tag):
    version = workspace_version()
    require(tag == f"v{version}", "tag must exactly match the workspace version")
    require(
        not output(["git", "status", "--porcelain", "--untracked-files=normal"]),
        "release checkout must be clean",
    )
    reference = f"refs/tags/{tag}"
    require(
        output(["git", "cat-file", "-t", reference]) == "tag",
        "release tag must be annotated and signed",
    )
    tag_object = output(["git", "cat-file", "tag", reference])
    header = tag_object.partition("\n\n")[0]
    tag_names = [
        line.removeprefix("tag ")
        for line in header.splitlines()
        if line.startswith("tag ")
    ]
    require(tag_names == [tag], "signed tag name differs from its reference")
    commit = output(["git", "rev-parse", f"{reference}^{{commit}}"])
    require(
        commit == output(["git", "rev-parse", "HEAD"]),
        "checkout differs from the signed release tag",
    )
    command(["git", "merge-base", "--is-ancestor", commit, "origin/master"])
    fingerprint = os.environ.get("RELEASE_GPG_FINGERPRINT", "")
    key = os.environ.get("RELEASE_GPG_PUBLIC_KEY", "")
    require(
        re.fullmatch(r"[A-F0-9]{40,64}", fingerprint),
        "configure the trusted GPG primary fingerprint",
    )
    require(0 < len(key) <= 65536, "configure the trusted GPG public key")
    with tempfile.TemporaryDirectory(prefix="maincopy-release-gpg-") as directory:
        environment = dict(os.environ, GNUPGHOME=directory)
        command(
            ["gpg", "--batch", "--import"],
            input=key,
            capture_output=True,
            text=True,
            env=environment,
        )
        verify_signature("tag", reference, fingerprint, environment)
        verify_signature("commit", commit, fingerprint, environment)
    return {
        "format": "maincopy-release-v1",
        "version": version,
        "tag": tag,
        "commit": commit,
        "tag_object": output(["git", "rev-parse", reference]),
    }


def validate_identity(identity):
    require(
        identity.get("format") == "maincopy-release-v1",
        "unknown release manifest format",
    )
    require(
        isinstance(identity.get("version"), str)
        and VERSION.fullmatch(identity["version"]),
        "invalid release version",
    )
    require(
        identity.get("tag") == f"v{identity['version']}",
        "manifest tag/version mismatch",
    )
    for field in ("commit", "tag_object"):
        require(
            isinstance(identity.get(field), str)
            and re.fullmatch(r"[0-9a-f]{40}", identity[field]),
            "invalid Git object identity",
        )


def confirm_checkout(identity):
    validate_identity(identity)
    require(
        workspace_version() == identity["version"],
        "workspace differs from prepared release",
    )
    require(
        output(["git", "rev-parse", "HEAD"]) == identity["commit"],
        "prepared commit differs from checkout",
    )
    require(
        output(["git", "rev-parse", f"refs/tags/{identity['tag']}"])
        == identity["tag_object"],
        "prepared tag object differs",
    )
    remote = output(
        ["git", "ls-remote", "--exit-code", "origin", f"refs/tags/{identity['tag']}"]
    )
    require(
        remote.split() == [identity["tag_object"], f"refs/tags/{identity['tag']}"],
        "remote release tag moved or disappeared",
    )


def verify_crate(path, name, version):
    with tarfile.open(path, "r:gz") as archive:
        prefix = f"{name}-{version}/"
        members = archive.getmembers()
        require(
            len(members) <= 50000
            and sum(member.size for member in members) <= ARTIFACT_LIMIT,
            "crate archive exceeds release bounds",
        )
        for member in members:
            require(
                member.name.startswith(prefix) and ".." not in Path(member.name).parts,
                "crate archive has an invalid path",
            )
            require(
                member.isfile() or member.isdir(),
                "crate archive contains unresolved links or special files",
            )
        license_bytes = archive.extractfile(prefix + "LICENSE").read()
        require(
            license_bytes == Path("LICENSE").read_bytes(), "packaged license differs"
        )
        require(
            archive.getmember(prefix + "README.md").isfile(),
            "packaged README is missing",
        )
        manifest = tomllib.loads(
            archive.extractfile(prefix + "Cargo.toml").read().decode()
        )
        require(
            manifest["package"]["name"] == name
            and manifest["package"]["version"] == version,
            "crate identity differs",
        )
        vcs = json.load(archive.extractfile(prefix + ".cargo_vcs_info.json"))
        require(
            vcs["git"]["sha1"] == output(["git", "rev-parse", "HEAD"])
            and not vcs["git"].get("dirty", False),
            "crate does not describe the clean candidate commit",
        )


def prepare(identity_path, directory, target):
    identity = read_json(identity_path)
    confirm_checkout(identity)
    directory.mkdir(parents=True, exist_ok=False)
    version = identity["version"]
    source = directory / f"maincopy-{version}-source.tar.gz"
    with tempfile.TemporaryFile() as archive:
        command(
            [
                "git",
                "archive",
                "--format=tar",
                f"--prefix=maincopy-{version}/",
                identity["commit"],
            ],
            stdout=archive,
        )
        archive.seek(0)
        with (
            source.open("wb") as destination,
            gzip.GzipFile(
                filename="", fileobj=destination, mode="wb", mtime=0
            ) as compressed,
        ):
            shutil.copyfileobj(archive, compressed)
    # A fresh target avoids accidentally publishing an older successful dry run.
    require(not target.exists(), "release Cargo target must be new")
    command(
        [
            "cargo",
            "publish",
            "--dry-run",
            "--locked",
            "--workspace",
            "--registry",
            "crates-io",
            "--target-dir",
            str(target),
        ]
    )
    crates = {}
    for name in PACKAGES:
        filename = f"{name}-{version}.crate"
        archive = target / "package" / "tmp-crate" / filename
        checksum(archive)
        verify_crate(archive, name, version)
        shutil.copyfile(archive, directory / filename)
        crates[name] = checksum(archive)
    metadata = json.loads(
        output(
            ["cargo", "metadata", "--locked", "--all-features", "--format-version", "1"]
        )
    )
    features = {node["id"]: node["features"] for node in metadata["resolve"]["nodes"]}
    inventory = [
        {key: package[key] for key in ("name", "version", "source", "license")}
        | {"features": sorted(features.get(package["id"], []))}
        for package in metadata["packages"]
    ]
    write_json(
        directory / "rust-dependencies.json",
        sorted(inventory, key=lambda item: (item["name"], item["version"])),
    )
    for filename in ("Cargo.lock", "flake.lock"):
        shutil.copyfile(filename, directory / filename)
    identity.update(
        crates=crates,
        source_sha256=checksum(source),
        cargo=output(["cargo", "--version"]),
    )
    write_json(directory / "release.json", identity)
    changelog = Path("CHANGELOG.md").read_text()
    notes = changelog.split("## Unreleased", 1)[1].split("\n## ", 1)[0].strip()
    (directory / "release-notes.md").write_text(
        f"Maincopy {version}\n\nSigned tag: `{identity['tag']}`\nCommit: `{identity['commit']}`\n\n"
        f"All five crates are published on crates.io. Nix: `nix run github:{REPOSITORY}/{identity['tag']}#maincopy -- --help`.\n\n"
        "The attached SHA256SUMS covers the source, crate archives, lockfiles, and dependency inventories. "
        "See docs/release.md in the source archive for installation and verification.\n\n"
        + notes
        + "\n"
    )


def artifact_names(identity):
    version = identity["version"]
    return {
        "release.json",
        "release-notes.md",
        "Cargo.lock",
        "flake.lock",
        "rust-dependencies.json",
        f"maincopy-{version}-source.tar.gz",
        *(f"{name}-{version}.crate" for name in PACKAGES),
        *(f"nix-closure-{system}.json" for system in SYSTEMS),
    }


def verify_artifacts(directory, sealed=True):
    identity = read_json(directory / "release.json")
    validate_identity(identity)
    expected = artifact_names(identity)
    require(
        {path.name for path in directory.iterdir()}
        == expected | ({"SHA256SUMS"} if sealed else set()),
        "release artifact set is incomplete or contains unexpected files",
    )
    hashes = {name: checksum(directory / name) for name in sorted(expected)}
    require(
        identity.get("crates")
        == {name: hashes[f"{name}-{identity['version']}.crate"] for name in PACKAGES},
        "crate checksum differs from prepared manifest",
    )
    require(
        identity.get("source_sha256")
        == hashes[f"maincopy-{identity['version']}-source.tar.gz"],
        "source checksum differs from prepared manifest",
    )
    lines = "".join(f"{digest}  {name}\n" for name, digest in hashes.items())
    if sealed:
        require(
            (directory / "SHA256SUMS").read_text() == lines,
            "SHA256SUMS differs from release artifacts",
        )
    return identity, lines


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, file, code, message, headers, url):
        return None


def request_json(url, *, method="GET", token=None, data=None, missing=False):
    headers = {
        "User-Agent": "maincopy-release/1",
        "Accept": "application/vnd.github+json",
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
        headers["X-GitHub-Api-Version"] = "2022-11-28"
    if isinstance(data, dict):
        data = json.dumps(data).encode()
        headers["Content-Type"] = "application/json"
    elif data is not None:
        headers["Content-Type"] = "application/octet-stream"
    request = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.build_opener(NoRedirect).open(
            request, timeout=60
        ) as response:
            raw = response.read(JSON_LIMIT + 1)
    except urllib.error.HTTPError as error:
        error.close()
        if missing and method == "GET" and error.code == 404:
            return None
        raise ReleaseError(
            f"release HTTP {method} failed with status {error.code}"
        ) from None
    except (urllib.error.URLError, TimeoutError):
        raise ReleaseError(
            f"release HTTP {method} failed; check remote state before retrying"
        ) from None
    require(len(raw) <= JSON_LIMIT, "release HTTP response exceeds the limit")
    return json.loads(raw)


def registry_checksum(name, version):
    url = f"https://index.crates.io/{name[:2]}/{name[2:4]}/{name}"
    request = urllib.request.Request(
        url, headers={"User-Agent": "maincopy-release/1", "Cache-Control": "no-cache"}
    )
    try:
        with urllib.request.build_opener(NoRedirect).open(
            request, timeout=30
        ) as response:
            raw = response.read(JSON_LIMIT + 1)
    except urllib.error.HTTPError as error:
        error.close()
        if error.code == 404:
            return None
        raise ReleaseError(
            f"crates.io index lookup failed with status {error.code}"
        ) from None
    except (urllib.error.URLError, TimeoutError):
        raise ReleaseError("crates.io index lookup failed") from None
    require(len(raw) <= JSON_LIMIT, "crates.io index response exceeds the limit")
    entries = [json.loads(line) for line in raw.splitlines()]
    matches = [entry for entry in entries if entry.get("vers") == version]
    require(len(matches) <= 1, "duplicate crate version in registry index")
    if not matches:
        return None
    entry = matches[0]
    require(
        entry.get("name") == name and entry.get("yanked") is False,
        "registered crate identity is wrong or yanked",
    )
    require(
        re.fullmatch(r"[0-9a-f]{64}", entry.get("cksum", "")),
        "registry checksum is invalid",
    )
    return entry["cksum"]


def unpublished_packages(identity, lookup=registry_checksum):
    pending = []
    for name in PACKAGES:
        registered = lookup(name, identity["version"])
        require(
            registered is None or registered == identity["crates"][name],
            f"{name} already exists with different bytes",
        )
        if registered is None:
            pending.append(name)
    return pending


def credential_configuration():
    provider = Path(__file__).with_name("release_credential.py").resolve()
    # A nonempty argument prevents Cargo from resolving a credential-alias for
    # this executable. Cargo passes it through the JSON protocol, not argv.
    # Use a scalar so Cargo cannot concatenate a lower-priority provider array.
    require(
        not any(character.isspace() for character in str(provider)),
        "release automation path must not contain whitespace",
    )
    return "registry.credential-provider=" + json.dumps(
        f"{provider} {CREDENTIAL_POLICY}"
    )


def publish_crates(directory, target):
    identity, _ = verify_artifacts(directory)
    confirm_checkout(identity)
    require(
        output(["cargo", "--version"]) == identity["cargo"],
        "Cargo differs from the preparation toolchain",
    )
    pending = unpublished_packages(identity)
    if not pending:
        print("All five crate versions already match the prepared checksums.")
        return
    require(
        os.environ.get("MAINCOPY_RELEASE_TOKEN"),
        "crates.io environment token is missing",
    )
    arguments = [
        "cargo",
        "--config",
        credential_configuration(),
        "publish",
        "--locked",
        "--registry",
        "crates-io",
        "--target-dir",
        str(target),
    ]
    for name in pending:
        arguments.extend(["--package", name])
    environment = dict(
        os.environ,
        MAINCOPY_RELEASE_MANIFEST=str((directory / "release.json").resolve()),
    )
    # Cargo itself orders the selected dependency graph and waits for index visibility.
    # Its provider verifies the exact regenerated archive checksum before each upload.
    result = subprocess.run(arguments, env=environment, check=False)
    remaining = unpublished_packages(identity)
    require(
        not remaining,
        "release remains incomplete; retry the same tag after checking registry visibility",
    )
    require(
        result.returncode == 0,
        "Cargo reported an error after uploads; all checksums match, rerun to confirm",
    )


def verify_asset(asset, path):
    require(asset.get("name") == path.name, "GitHub release asset name differs")
    require(
        asset.get("state") == "uploaded",
        "existing GitHub asset is incomplete; inspect it before retrying",
    )
    require(
        asset.get("size") == path.stat().st_size
        and asset.get("digest") == "sha256:" + checksum(path),
        "GitHub release asset differs; refusing to overwrite",
    )


def publish_github(directory, identity, api):
    base = f"https://api.github.com/repos/{REPOSITORY}"
    tag = identity["tag"]
    reference = api(f"{base}/git/ref/tags/{tag}")
    tag_object = reference.get("object", {})
    require(
        reference.get("ref") == f"refs/tags/{tag}"
        and tag_object.get("type") == "tag"
        and tag_object.get("sha") == identity["tag_object"],
        "GitHub tag no longer matches the approved signed tag object",
    )
    release = api(f"{base}/releases/tags/{tag}", missing=True)
    if release is None:
        release = api(
            f"{base}/releases",
            method="POST",
            data={
                "tag_name": tag,
                "target_commitish": identity["commit"],
                "name": f"Maincopy {identity['version']}",
                "body": (directory / "release-notes.md").read_text(),
                "draft": True,
                "prerelease": "-" in identity["version"],
            },
        )
    require(release.get("tag_name") == tag, "GitHub release tag differs")
    require(
        release.get("name") == f"Maincopy {identity['version']}"
        and release.get("body") == (directory / "release-notes.md").read_text()
        and release.get("prerelease") == ("-" in identity["version"]),
        "GitHub release notes or metadata differ from the approved candidate",
    )
    release_id = release["id"]
    require(isinstance(release_id, int), "invalid GitHub release ID")
    assets = api(f"{base}/releases/{release_id}/assets?per_page=100")
    require(
        isinstance(assets, list) and len(assets) < 100,
        "unexpected GitHub release asset count",
    )
    existing = {asset["name"]: asset for asset in assets}
    expected = artifact_names(identity) | {"SHA256SUMS"}
    require(
        len(existing) == len(assets) and set(existing) <= expected,
        "GitHub release contains unexpected or duplicate assets",
    )
    # Check every existing object before uploading anything new.
    for name, asset in existing.items():
        verify_asset(asset, directory / name)
    for name in sorted(expected - set(existing)):
        require(
            release.get("draft") is True,
            "published release is missing an expected asset",
        )
        path = directory / name
        url = (
            f"https://uploads.github.com/repos/{REPOSITORY}/releases/{release_id}/assets?"
            + urllib.parse.urlencode({"name": name})
        )
        verify_asset(api(url, method="POST", data=path.read_bytes()), path)
    if release.get("draft") is True:
        release = api(
            f"{base}/releases/{release_id}",
            method="PATCH",
            data={"draft": False, "make_latest": "legacy"},
        )
    require(
        release.get("immutable") is True,
        "release was published without immutability; enable immutable releases before any further release",
    )


def nix_inventory(output_path, system, destination):
    require(system in SYSTEMS, "unsupported Nix system")
    raw = json.loads(output(["nix", "path-info", "--recursive", "--json", output_path]))
    entries = (
        raw
        if isinstance(raw, list)
        else [dict(value, path=key) for key, value in raw.items()]
    )
    inventory = [
        {key: item[key] for key in ("path", "narHash", "narSize", "references")}
        for item in entries
    ]
    for item in inventory:
        item["references"].sort()
    write_json(
        destination / f"nix-closure-{system}.json",
        sorted(inventory, key=lambda item: item["path"]),
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="operation", required=True)
    verify = subcommands.add_parser("verify-tag")
    verify.add_argument("--tag", required=True)
    verify.add_argument("--output", required=True, type=Path)
    preparation = subcommands.add_parser("prepare")
    preparation.add_argument("--identity", required=True, type=Path)
    preparation.add_argument("--output", required=True, type=Path)
    preparation.add_argument("--target-dir", required=True, type=Path)
    for name in ("seal", "check", "publish-crates", "publish-github"):
        child = subcommands.add_parser(name)
        child.add_argument("--artifacts", required=True, type=Path)
        if name == "publish-crates":
            child.add_argument("--target-dir", required=True, type=Path)
    inventory = subcommands.add_parser("nix-inventory")
    inventory.add_argument("--output-path", required=True)
    inventory.add_argument("--system", required=True)
    inventory.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if args.operation == "verify-tag":
        write_json(args.output, verify_tag(args.tag))
    elif args.operation == "prepare":
        prepare(args.identity, args.output, args.target_dir)
    elif args.operation == "seal":
        _, lines = verify_artifacts(args.artifacts, sealed=False)
        (args.artifacts / "SHA256SUMS").write_text(lines)
    elif args.operation == "check":
        identity, _ = verify_artifacts(args.artifacts)
        confirm_checkout(identity)
    elif args.operation == "publish-crates":
        publish_crates(args.artifacts, args.target_dir)
    elif args.operation == "publish-github":
        identity, _ = verify_artifacts(args.artifacts)
        confirm_checkout(identity)
        require(
            not unpublished_packages(identity),
            "all five crates must be visible before GitHub publication",
        )
        token = os.environ.get("GH_TOKEN")
        require(token, "GitHub release token is missing")
        publish_github(
            args.artifacts,
            identity,
            lambda url, **options: request_json(url, token=token, **options),
        )
    elif args.operation == "nix-inventory":
        nix_inventory(args.output_path, args.system, args.output)


if __name__ == "__main__":
    try:
        main()
    except (
        ReleaseError,
        OSError,
        ValueError,
        KeyError,
        TypeError,
        AttributeError,
        tarfile.TarError,
    ) as error:
        # External response bodies, subprocess diagnostics and credentials stay out.
        print(
            f"Release stopped: {error if isinstance(error, ReleaseError) else type(error).__name__}",
            file=sys.stderr,
        )
        sys.exit(1)
