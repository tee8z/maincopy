# Releases

Maincopy releases publish all five workspace crates to crates.io and expose the
same signed GitHub tag as a Nix flake. The owner selects the release version.
The development version in Cargo.toml does not announce a release.

The [release workflow](../.github/workflows/release.yml) runs from `master` with
an existing signed tag as input. It prepares a candidate, waits for one owner
approval, publishes missing crate versions, and publishes the complete GitHub
Release last. It does not change versions, create commits, or replace tags.

Use [the implementation plan](implementation.md#system-and-security-acceptance) for release acceptance.
Mailing-list and bulk delivery require their own privacy and deliverability
acceptance before inclusion in a release. A successful package build does not
satisfy those checks.

## Configure the release boundary

Complete this setup before dispatching the first release. These are repository
and registry settings; the workflow does not configure them.

| Setting | Required value or action |
| --- | --- |
| Protected source | Protect `master` and review changes to the workflow and `scripts/release*`. Require signed release commits. |
| Tag rules | Restrict creation of `v*` tags to release maintainers. Prohibit updates and deletion of release tags. |
| Repository variable `RELEASE_GPG_PUBLIC_KEY` | ASCII-armored public key for the approved release signer. Never provide the private key. |
| Repository variable `RELEASE_GPG_FINGERPRINT` | Full uppercase primary-key fingerprint, checked through an independent trusted channel. |
| Environment `release` | Require an owner review and restrict deployments to the `master` branch. Do not leave an automatically created, unprotected environment in place. |
| Environment secret `CARGO_REGISTRY_TOKEN` | A crates.io token authorized to create or publish all five package names. Keep it in this environment, not in repository files or command arguments. |
| GitHub Release setting | Enable immutable releases before publication. The job verifies immutability after publishing. |
| Actions permissions | Allow the publication job's `contents: write` permission. Preparation jobs have read access only. |
| Builders | Use hosted `ubuntu-24.04` for x86_64. Provision the ARM64 KVM runner described below. Both Nix checks must pass; neither architecture is an optional gate. |

The owner must verify crate namespace availability or existing ownership before
approval. The first upload claims a previously unused name. A matching registry
checksum does not prove that a maintainer controls that package.

The workflow intentionally has no tag-push trigger. A workflow loaded from an
unverified tag cannot establish its own trust by calling a verifier from that
same tag. Each job instead checks out the dispatched `master` revision under
`automation/`, then checks out the candidate under `candidate/`. The trusted
helper verifies the tag and commit before running candidate build commands.
Only dispatches from `master` run these jobs. The protected environment provides
the separate authorization to publish.

Signature verification uses a temporary GPG keyring with the configured public
key. Both the annotated tag and its target commit must match the approved
primary fingerprint. The tag must match the workspace version, point to the
checked-out commit, and target a commit reachable from `origin/master`.
The verifier also checks the tag object's internal name. SSH signatures are
not accepted by this workflow; the current release signing policy uses GPG.

See GitHub's [deployment environment rules](https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments)
and [immutable release guarantees](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases).
With an administrator's authenticated GitHub CLI, confirm immutability before
approving a release:

~~~bash
gh api repos/tee8z/maincopy/immutable-releases | jq -e '.enabled == true'
~~~

A permission error does not prove that immutability is enabled. Resolve it or
confirm the setting in the repository UI before approval.

## Provision the ARM64 release runner

The ARM matrix entry requires all four labels:
`[self-hosted, linux, ARM64, maincopy-release-kvm]`. Provisioning and the first
successful ARM release check remain external acceptance work. This repository
does not create or register the runner.

Use a dedicated Ubuntu 24.04 ARM64 host with hardware KVM, or an ARM64 virtual
machine with verified nested KVM support. GitHub's hosted ARM64 fleet does not
expose the KVM device required by the current NixOS VM gate. This is a host
limitation; adding a label, container privilege, or a Nix feature flag does not
provide virtualization. See the [GitHub runner limitation](https://github.com/actions/runner-images/issues/14062#issuecomment-5352403358).

Provision Git, Python 3, GPG, and passwordless sudo for the existing Nix and
AppArmor setup steps. Make `/dev/kvm` usable by the runner account and Nix build
users. The Nix builder must advertise real `kvm` and `nixos-test` capabilities.
Allow capacity for the configured 3 GiB guest and the concurrent Rust/Nix builds.
The workflow first checks native architecture and opens `/dev/kvm` to query its
API version, then checks Nix's declared features. The complete VM test remains
the proof that the builder can run Maincopy's deployment checks.

Use a fresh, ephemeral runner environment for each trusted release run. Restrict
runner access to the approved release workflow; never use it for untrusted pull
requests or keep production credentials on it. Labels select runners and do not
provide access control. Apply repository or runner-group access controls before
registration. See [GitHub runner routing](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/use-in-a-workflow).

Both architectures still execute the full `nix flake check`, including
`deployment-vm`, and build the package. Missing runner capacity leaves that gate
pending; it does not permit publication. The workflow does not substitute
software emulation or extend the VM's existing 60-second readiness deadlines.

## Prepare and sign the candidate

Select the version and review the [changelog](../CHANGELOG.md#unreleased).
Keep the workspace version, all five inherited package versions, internal
exact dependency requirements, and Cargo.lock consistent. The Maincopy Nix
version comes from the workspace manifest. Litestream has its own version.

Use signed Conventional Commits for the reviewed version changes. Complete the
[quality gates](quality.md#crap-risk-check), Windows client checks, and applicable
owner acceptance for that exact commit. Record the evidence with the release
review. The workflow repeats the Nix checks; it does not calculate the manual
CRAP score or verify real signer, keychain, B2, or production acceptance.

Ordinary CI and release preparation also run the release helper tests through
`nix develop`. The development shell supplies Python, GPG, and the pinned Cargo
toolchain; tests use temporary keys, repositories, and a loopback registry.

Prepare on Linux with symlinks preserved. Each crate's LICENSE and README.md
links to repository documentation; Cargo packages their resolved contents.
A checkout with `core.symlinks=false` does not provide those bytes.

From the clean, reviewed `master` checkout, create the signed annotated tag.
These commands publish the tag and start preparation; the protected job later
waits for approval before any crate or GitHub Release upload.

~~~bash
set -euo pipefail
test "$(git branch --show-current)" = master
test -z "$(git status --porcelain)"
release_version="$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')"
release_tag="v$release_version"
release_commit="$(git rev-parse HEAD)"
git verify-commit "$release_commit"
git tag -s "$release_tag" "$release_commit" -m "Maincopy $release_version"
git verify-tag "$release_tag"
git push origin "$release_tag"
gh workflow run release.yml --repo tee8z/maincopy --ref master -f tag="$release_tag"
~~~

Check the displayed signer against the approved fingerprint. Stop if signing
or verification fails. If the tag already exists, verify it and dispatch that
same tag; do not run the tag-creation command again or force an update.
The signed tag binds the source commit. The generated checksum manifest is a
release asset, not part of the tag annotation.

## Review the prepared candidate

| Stage | Evidence and behavior |
| --- | --- |
| Signature and metadata | Approved GPG signer, signed commit, exact tag, five package identities, matching version pins and license bytes. |
| Cargo preparation | `cargo publish --dry-run --locked --workspace --registry crates-io` using the toolchain pinned by flake.lock. Cargo verifies the extracted packages and their dependency graph without uploading. |
| Nix validation | Both `x86_64-linux` and `aarch64-linux` run `nix flake check` and build the committed source archive. |
| Candidate sealing | The `release-candidate` Actions artifact contains the source archive, five `.crate` files, lockfiles, Rust dependency inventory, both Nix closure inventories, release identity, notes, and SHA256SUMS. |
| Owner approval | Review that artifact, the job results, the intended version, ownership/settings, and the external acceptance record. Approve the `release` environment once. |
| Publication | Reverify signed identity and artifact checksums, publish missing crate versions, upload missing matching draft assets, then publish the GitHub Release. |

The source archive uses `git archive` and a gzip stream with a fixed timestamp.
Cargo archives come from a fresh publish dry run's `package/tmp-crate/` directory;
stale package output is not accepted. The helper checks packaged README, license,
and clean Git identity. Nix inventories omit machine-specific registration times.
Dependency inventories describe the build inputs; they are not a license or
vulnerability audit.

Download `release-candidate` from the workflow run before approval. In its
extracted directory, verify the manifest:

~~~bash
sha256sum --check SHA256SUMS
~~~

Read release.json and release-notes.md. SHA256SUMS covers the complete expected
asset set, including both files. Changes to any prepared asset stop publication.
Actions artifacts are retained for 30 days; keep the approved evidence with the
release record. Package verification builds runtime targets, so it does not
replace the complete source test gate.

## Publication order and retries

| Crate | Workspace prerequisites |
| --- | --- |
| `maincopy-shared` | None |
| `maincopy-diagram-renderer` | None |
| `markdown-compiler` | None |
| `maincopy-cli` | `maincopy-shared` |
| `maincopy-server` | The first three crates |

The helper checks every existing version before uploading. It rejects a yanked
version or a checksum mismatch. It then gives Cargo the remaining package set;
Cargo orders dependencies and waits for registry visibility.
The release-only [credential provider](../scripts/release_credential.py) checks
Cargo's actual archive checksum immediately before each upload. It returns an
uncached credential only for the prepared package/version/checksum. There is no
fallback token provider for publication. See Cargo's
[publish behavior](https://doc.rust-lang.org/cargo/commands/cargo-publish.html),
[credential protocol](https://doc.rust-lang.org/cargo/reference/credential-provider-protocol.html),
and [registry checksum contract](https://doc.rust-lang.org/cargo/reference/registry-index.html).

A five-crate release is not an atomic registry transaction. After an interrupted
upload, some immutable versions can already exist. Retry the failed publication
job to reuse the same candidate artifact, or dispatch a new run for the same
signed tag. A new run prepares and checks the candidate again. Cargo can finish
an upload before its visibility wait times out; the helper checks registry
state before proceeding. It never treats a network or authorization error as
proof that a version is absent.

Do not rerun every successful job inside the same workflow run: the artifact
names are immutable within that run. Use a new dispatch for full preparation.
If the toolchain, archive bytes, or tag identity differ on retry, stop and
investigate. Do not edit the checksum manifest to authorize a replacement.

GitHub publication starts with a draft. Existing assets must have the expected
name, size, uploaded state, and GitHub SHA-256 digest. The helper checks all
existing assets before uploading missing ones and never overwrites them.
It publishes only after every crate is visible and every asset is complete.
A completed matching release makes a retry read-only.
An incomplete GitHub asset or an unexpected asset requires owner inspection;
automation does not delete it. A missing asset on an already published release
also stops the run. See the [release assets API](https://docs.github.com/en/rest/releases/assets).

## Install the released version

Replace `vX.Y.Z` with the published immutable tag. Nix needs the `nix-command`
and `flakes` experimental features enabled. The tagged flake supports Linux
x86_64 and arm64. Its package installs `maincopy`, `maincopyd`, `maincopy-ssh`,
`maincopy-mermaid`, and `markdowncompiler` together.

~~~bash
release_tag=vX.Y.Z
nix run "github:tee8z/maincopy/$release_tag#maincopy" -- --help
nix run "github:tee8z/maincopy/$release_tag#maincopyd" -- --version
nix profile add "github:tee8z/maincopy/$release_tag#maincopy"
~~~

The default flake app runs the daemon, so select `#maincopy`
for the operator CLI. These commands consume the tagged source; there is no
separate Nix registry publication or claim of portable standalone binaries.
See the [Nix flake reference](https://nix.dev/manual/nix/stable/command-ref/new-cli/nix3-flake.html).

For a NixOS host, add the immutable tag to the host flake and commit its lockfile:

~~~nix
inputs.maincopy.url = "github:tee8z/maincopy/vX.Y.Z";
# Include this in the host's nixosSystem modules list:
# inputs.maincopy.nixosModules.default
~~~

Follow [deployment.md](deployment.md) for host configuration, credentials,
bootstrap, and backups. Keep the input lockfile's resolved commit consistent
with release.json. Nix fetches the tag and content hashes; it does not perform
the workflow's GPG signer verification for the consumer.

For Cargo installation, use the same exact version for the executable crates:

~~~bash
release_version=X.Y.Z
cargo install --locked --version "=$release_version" maincopy-cli
cargo install --locked --version "=$release_version" maincopy-diagram-renderer
cargo install --locked --version "=$release_version" maincopy-server
cargo install --locked --version "=$release_version" markdown-compiler
~~~

`maincopy-shared` is a library dependency and has no executable to install.
The server needs the matching renderer and SSH helper, plus Git and OpenSSH.
Validate the installed process layout and platform credential storage before
using a Cargo installation in production; the NixOS module supplies the
supported service layout and protected host paths.
