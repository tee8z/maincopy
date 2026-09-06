#!/usr/bin/env python3
"""Cargo credential provider: approve only the prepared crates.io checksums."""

import json
import os
import sys
from pathlib import Path

from release import (
    CREDENTIAL_POLICY,
    PACKAGES,
    ReleaseError,
    read_json,
    require,
    validate_identity,
)

REQUEST_LIMIT = 65536
CRATES_IO = {
    "sparse+https://index.crates.io/",
    "https://github.com/rust-lang/crates.io-index",
}


def credential(request, manifest, token):
    require(
        request.get("v") == 1 and request.get("kind") == "get",
        "unsupported credential request",
    )
    require(
        request.get("registry", {}).get("index-url") in CRATES_IO,
        "credential is restricted to crates.io",
    )
    require(
        request.get("args") == [CREDENTIAL_POLICY], "credential policy argument differs"
    )
    validate_identity(manifest)
    require(
        set(manifest.get("crates", {})) == set(PACKAGES),
        "prepared package set is incomplete",
    )
    operation = request.get("operation")
    if operation == "publish":
        name = request.get("name")
        require(
            name in PACKAGES and request.get("vers") == manifest["version"],
            "package was not approved for this release",
        )
        require(
            request.get("cksum") == manifest["crates"][name],
            "Cargo archive differs from the approved checksum",
        )
    else:
        # Cargo performs a preliminary read credential request before packaging.
        require(operation == "read", "only read and approved publication are supported")
    require(
        isinstance(token, str) and 0 < len(token) <= 16384,
        "release credential is missing or invalid",
    )
    return {
        "Ok": {
            "kind": "get",
            "token": token,
            "cache": "never",
            "operation_independent": False,
        }
    }


def main():
    print(json.dumps({"v": [1]}), flush=True)
    try:
        require(sys.argv[1:] == ["--cargo-plugin"], "provider must be called by Cargo")
        raw = sys.stdin.buffer.readline(REQUEST_LIMIT + 1)
        require(
            len(raw) <= REQUEST_LIMIT and raw.endswith(b"\n"),
            "credential request exceeds the limit",
        )
        manifest = read_json(Path(os.environ["MAINCOPY_RELEASE_MANIFEST"]))
        response = credential(
            json.loads(raw), manifest, os.environ.get("MAINCOPY_RELEASE_TOKEN")
        )
    except (ReleaseError, OSError, ValueError, KeyError, TypeError, AttributeError):
        response = {
            "Err": {
                "kind": "other",
                "message": "Maincopy release credential policy rejected this operation",
            }
        }
    # stdout is the private Cargo protocol pipe. Never log this response elsewhere.
    print(json.dumps(response), flush=True)


if __name__ == "__main__":
    main()
