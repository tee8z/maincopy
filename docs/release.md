# Release candidate preparation

Prepare reviewable artifacts from one signed commit before publishing a release.
The owner has not selected the first release version or distribution channels.
The current workspace version is development metadata, not an announced release.

This runbook defines manual preparation. The [CI workflow](../.github/workflows/ci.yml)
runs checks; it does not publish tags, GitHub Releases, or crates.
Use [system evidence](system-evidence.md) to track acceptance separately from packaging.

Mailing-list and bulk delivery remain conditional first-release work.
Require separate privacy and deliverability acceptance before including them in release notes or artifacts.

## Select and freeze the candidate

The owner selects the version, supported platforms, distribution channels, and
trusted signing key fingerprints. Record those decisions with the candidate.
Version and packaging changes require their own review and appropriate checks.
Use signed Conventional Commits, such as
`git commit -S -m "chore(release): prepare candidate metadata"`.
Stop if signing fails.

Keep the workspace version, inherited package versions, lockfile, and Maincopy
derivation versions in [flake.nix](../flake.nix) consistent.
Do not change the independently pinned Litestream version to match Maincopy.
Review package contents, README links, license files, and the
[Unreleased changelog](../CHANGELOG.md#unreleased).

Run the following in one Bash session from the clean candidate checkout.
The operator environment needs Git, Nix, Cargo, Python 3.11 or later, jq,
GNU tar, gzip, and SHA-256 tools.
GitHub staging later also needs authenticated GitHub CLI access.

~~~bash
set -euo pipefail
test -z "$(git status --porcelain)"
release_checkout="$PWD"
release_revision="$(git rev-parse HEAD)"
git verify-commit "$release_revision"
release_version="$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')"
release_tag="v$release_version"
git check-ref-format "refs/tags/$release_tag"
release_system="$(nix eval --impure --raw --expr builtins.currentSystem)"
release_root="$(mktemp -d /tmp/maincopy-release.XXXXXX)"
release_artifacts="$release_root/artifacts"
mkdir "$release_artifacts"
cargo metadata --locked --no-deps --format-version 1 > "$release_root/workspace.json"
jq -e --arg version "$release_version" 'all(.packages[]; .version == $version)' "$release_root/workspace.json"
test "$(nix eval --raw --no-update-lock-file ".#packages.$release_system.default.version")" = "$release_version"
~~~

Check the reported commit signer against the owner's trusted fingerprints.
A valid signature alone does not establish an approved signer.
Configure the verifier's trusted keyring or SSH allowed-signers file before verification.
Retain the complete commit ID and tool versions with the evidence.

## Build the committed source archive

The archive contains committed files only. Keep generated evidence outside the
checkout so a retry cannot silently include local state.

~~~bash
git archive --format=tar --prefix="maincopy-$release_version/" "$release_revision" |
  gzip -n > "$release_artifacts/maincopy-$release_version-source.tar.gz"
mkdir "$release_root/source"
tar -xzf "$release_artifacts/maincopy-$release_version-source.tar.gz" -C "$release_root/source"
release_source="$release_root/source/maincopy-$release_version"
tar -tzf "$release_artifacts/maincopy-$release_version-source.tar.gz" > "$release_root/source-files.txt"
nix flake check --no-update-lock-file --print-build-logs "path:$release_source"
nix build --no-update-lock-file --no-link --print-out-paths --print-build-logs \
  "path:$release_source#packages.$release_system.default" > "$release_root/output-path.txt"
release_output="$(cat "$release_root/output-path.txt")"
"$release_output/bin/maincopyd" --version
"$release_output/bin/maincopy" --version
~~~

Inspect the archive listing for required templates, frontend files, migrations,
helpers, licenses, and fixtures. Compare binary versions with the selected version.
The flake defines `x86_64-linux` and `aarch64-linux` outputs.
Run the checks for every advertised platform on a capable native or remote builder.
One host's flake check does not prove another architecture passed.

Complete the manual CRAP gate from [quality.md](quality.md#crap-risk-check).
Record the configured tool versions and results for the same source revision.
Do not infer CRAP success from Nix or CI.

## Record dependencies and artifact identity

Generate the Rust inventory from the extracted source, including enabled features.
This inventory and the Nix closure record are not a vulnerability or license audit.

~~~bash
(
  cd "$release_source"
  cargo metadata --locked --all-features --format-version 1 |
    jq '(.resolve.nodes | map({key: .id, value: .features}) | from_entries) as $features |
      [.packages[] | {name, version, source, license, features: $features[.id]}] |
      sort_by(.name, .version)' \
      > "$release_artifacts/rust-dependencies.json"
)
cp "$release_source/Cargo.lock" "$release_artifacts/Cargo.lock"
cp "$release_source/flake.lock" "$release_artifacts/flake.lock"
nix flake metadata --json --no-update-lock-file "path:$release_source" \
  > "$release_root/nix-metadata.json"
nix path-info --recursive --json "$release_output" \
  > "$release_artifacts/nix-closure-$release_system.json"
~~~

Keep the raw Nix metadata with private preparation logs; it contains local source paths.
The copied `flake.lock` records the pinned inputs for the public dependency inventory.
Review the dependency inventory before staging it.

If the owner selects downloadable Nix closures, export the complete runtime closure:

~~~bash
nix-store --query --requisites "$release_output" > "$release_root/closure-paths.txt"
mapfile -t release_closure < "$release_root/closure-paths.txt"
nix-store --export "${release_closure[@]}" |
  gzip -n > "$release_artifacts/maincopy-$release_version-$release_system.nix-export.gz"
~~~

Verify import and startup in an isolated Nix environment before offering that asset.
Do not treat copied executables as a standalone Nix distribution.
If registry packages are selected, finish their preparation below before finalizing checksums.

~~~bash
(
  cd "$release_artifacts"
  sha256sum -- * > "$release_root/SHA256SUMS"
  cp "$release_root/SHA256SUMS" SHA256SUMS
  sha256sum --check SHA256SUMS
)
~~~

Generate this manifest once the artifact set is complete. On retry, verify it first.
If an artifact changes, prepare a new reviewed manifest before creating or publishing a tag.

## Conditional crates.io preparation

Registry distribution remains undecided. Current internal dependencies use local
paths without registry versions. Package repository and README metadata also need review.
These manifests are not yet a completed registry candidate.

Before a registry dry run, the owner approves the package set and verifies namespace
availability or existing ownership. Add explicit versions to internal path dependencies.
Review each package's description, repository, README, license, and included files.
Verify a fresh installation can locate the server's renderer and SSH helper.
Keep these changes in the reviewed candidate commit.

The current dependency order is:

| Packages | Workspace dependency prerequisite |
| --- | --- |
| `maincopy-shared`, `maincopy-diagram-renderer`, `markdown-compiler` | None |
| `maincopy-cli` | `maincopy-shared` |
| `maincopy-server` | All three prerequisite packages |

For each approved package, set `release_crate` to its exact package name and run:

~~~bash
cd "$release_checkout"
cargo package --locked --list --package "$release_crate"
cargo publish --dry-run --locked --package "$release_crate"
~~~

The dry run performs packaging checks without uploading.
Record its output and the resulting `target/package/*.crate` identity.
Copy only approved crate archives into the artifact directory before finalizing checksums.
Dependent dry runs can fail while required versions are absent from the registry.
Do not publish prerequisite crates merely to turn a preparation failure green.
Record that dependency-index gate and obtain the owner's approved publication sequence.

Do not bypass verification with `--allow-dirty` or `--no-verify`.
Actual `cargo publish --locked --package "$release_crate"` requires publication approval.
After an interrupted upload, inspect the registry version and checksum before retrying.
An existing version with different bytes requires investigation, not a replacement upload.

## Sign the tag and stage a draft

Complete the owner signer, actual B2 recovery, VM, and production checks listed in
[system evidence](system-evidence.md#pending-acceptance) before declaring release acceptance.
Keep unresolved checks visible in the draft notes.
Prepare the exact release notes as `$release_root/release-notes.md`.
Include the commit, platforms, artifacts, checksums, dependency inventory, and acceptance results.

After the owner approves the candidate version and artifact identities, create a
signed annotated tag that binds the checksum manifest:

~~~bash
cd "$release_checkout"
release_checksums="$(sha256sum "$release_artifacts/SHA256SUMS" | cut -d ' ' -f 1)"
git tag -s "$release_tag" "$release_revision" \
  -m "Maincopy $release_version; SHA256SUMS $release_checksums"
git verify-tag "$release_tag"
test "$(git cat-file -t "$release_tag")" = tag
test "$(git rev-parse "$release_tag^{commit}")" = "$release_revision"
~~~

Reject an unsigned tag, unexpected signer, version mismatch, or different commit.
If the tag already exists, verify its signature, target, and checksum annotation.
Reuse a matching tag; do not delete or force-move it.

Remote staging is a separate action. Pushing a tag exposes it publicly.
Run these commands only after authorization to push the tag and create the draft:

~~~bash
git push origin "refs/tags/$release_tag:refs/tags/$release_tag"
gh release view "$release_tag" --json tagName,isDraft,targetCommitish,assets
~~~

If a draft already exists, verify its tag and continue with its missing assets.
If lookup fails from authentication or network errors, stop.
Create a draft only after an authenticated lookup confirms that no release exists:

~~~bash
gh release create "$release_tag" --verify-tag --draft \
  --title "Maincopy $release_version" --notes-file "$release_root/release-notes.md"
~~~

Before each upload, confirm the release remains a draft.
For each artifact filename, inspect the existing asset inventory.
Upload a missing asset with
`gh release upload "$release_tag" "$release_artifacts/$release_asset_name"`.
Set `release_asset_name` to the reviewed filename, including `SHA256SUMS`.
For an existing asset, download into a fresh directory and compare exact bytes:

~~~bash
release_download="$(mktemp -d /tmp/maincopy-release-asset.XXXXXX)"
gh release download "$release_tag" --pattern "$release_asset_name" --dir "$release_download"
cmp -- "$release_artifacts/$release_asset_name" "$release_download/$release_asset_name"
~~~

Skip matching assets. Stop on a mismatch; do not use `--clobber`.
After interruption, repeat inventory and checksum checks before uploading anything else.
Download the complete draft asset set and verify `SHA256SUMS` before final review.

The owner approves the concrete draft and any registry publication before artifacts
become public. No release-publishing workflow is configured.
Any future workflow must use immutable action commits, trusted tag verification,
and an owner-controlled publication gate.
