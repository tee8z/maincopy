# Releases

A release publishes all five workspace crates to crates.io and exposes the signed
GitHub tag as a Nix flake. The owner selects the version.

The [workflow](../.github/workflows/release.yml) accepts an existing tag through a
manual dispatch from `master`. It prepares artifacts, waits for owner approval,
then publishes crates and the GitHub Release. It does not create commits or tags.

## Configure the release boundary

Complete these repository and registry settings before the first dispatch:

| Setting | Required action |
| --- | --- |
| Source and tags | Protect `master`. Review workflow and release-script changes. Require signed commits. Restrict `v*` tag creation to maintainers; prohibit tag updates and deletion. |
| `RELEASE_GPG_PUBLIC_KEY` repository variable | Set the approved signer's ASCII-armored public key. |
| `RELEASE_GPG_FINGERPRINT` repository variable | Set the full uppercase primary-key fingerprint, verified through an independent trusted channel. |
| `release` environment | Require owner approval and restrict deployments to `master`. |
| `CARGO_REGISTRY_TOKEN` environment secret | Supply a crates.io token authorized for all five package names. Verify namespace availability or ownership before approval. |
| Immutable releases | Enable this GitHub repository setting before publication. |
| Actions permissions | Permit the publication job's `contents: write` permission. |
| Builders | Use hosted `ubuntu-24.04` for x86_64 and the ARM64 runner below. Both architecture checks are required. |

The workflow verifies the annotated tag and target commit against the configured
GPG fingerprint. SSH signatures are not accepted. The tag must match the
workspace version and target a commit reachable from `origin/master`.

With an administrator's authenticated GitHub CLI, confirm release immutability:

~~~bash
gh api repos/tee8z/maincopy/immutable-releases | jq -e '.enabled == true'
~~~

If access is denied, confirm the setting in the repository UI before approval.
See GitHub's [environment rules](https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments)
and [immutable releases](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases).

## Provision the ARM64 release runner

Register a dedicated Ubuntu 24.04 ARM64 runner with these labels:
`[self-hosted, linux, ARM64, maincopy-release-kvm]`.

The host needs working hardware or nested KVM. Make `/dev/kvm` accessible to the
runner account and Nix build users. Advertise `kvm` and `nixos-test` as Nix system
features. Provide capacity for the 3 GiB guest and Rust/Nix builds.

Install Git, Python 3, GPG, and passwordless sudo for the workflow's Nix and
AppArmor setup. Use a fresh, ephemeral environment for each trusted release run.
Restrict access through repository or runner-group controls; labels only select
runners. Do not share this runner with untrusted pull requests or production credentials.

Both architectures run `nix flake check`, including `deployment-vm`, and build
the package. Missing KVM or runner capacity blocks publication.

## Prepare and sign the candidate

Select the version and review the [changelog](../CHANGELOG.md#unreleased).
Update the workspace version, internal exact dependency requirements, and
Cargo.lock together. All five crates inherit the workspace version.

Use a Linux checkout with symlinks preserved; crate README and license files
link to repository documentation. Commit version changes with a signed
Conventional Commit, then push the reviewed commit to `master`.

Complete the [quality gates](quality.md), Windows client checks, and applicable
[owner acceptance](implementation.md#system-and-security-acceptance) for that commit.
Keep the results with the release review. Package builds do not replace email
privacy, provider, restore, or production acceptance.

From the clean, reviewed `master` checkout, create and push the signed tag:

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

Check the displayed signer against the approved fingerprint. Stop if verification
fails. If the tag exists, verify it and dispatch the same tag without recreating
or replacing it. The dispatch starts preparation; publication still requires approval.

## Review the prepared candidate

Before approving the `release` environment, check:

- Signature and version verification, Cargo package verification, and both Nix jobs passed.
- External acceptance is complete for the intended release features.
- Crate ownership, environment restrictions, and immutable releases are configured.
- The `release-candidate` artifact contains the expected source, five crate archives,
  lockfiles, dependency inventories, release.json, release-notes.md, and SHA256SUMS.

Download and extract `release-candidate`, then verify its manifest:

~~~bash
sha256sum --check SHA256SUMS
~~~

Read release.json and release-notes.md. Keep the approved evidence; Actions
artifacts expire after 30 days.

**Approval authorizes publication of immutable crate versions.** The publication
job rechecks the signed identity and prepared checksums, uploads missing versions,
then publishes the complete GitHub Release.

## Publication order and retries

Cargo publishes dependencies before dependents: `maincopy-shared`,
`maincopy-diagram-renderer`, and `markdown-compiler` precede their consumers,
`maincopy-cli` and `maincopy-server`.

The five-crate upload is not atomic. If publication stops, retry the failed
publication job with its existing candidate artifact. Alternatively, dispatch a
new run for the same signed tag. A new run repeats preparation and checks.
Do not rerun all successful jobs within one run; artifact names cannot be replaced.

Existing crate versions and GitHub assets must match the prepared checksums.
The workflow skips matching uploads and refuses yanked versions, mismatches,
unexpected assets, and incomplete assets. It never overwrites published content.
If a retry reports a mismatch, stop and inspect it. Do not change the tag or checksum manifest.

A completed matching release makes a retry read-only. See Cargo's
[publish behavior](https://doc.rust-lang.org/cargo/commands/cargo-publish.html)
and GitHub's [release assets API](https://docs.github.com/en/rest/releases/assets).

## Install the released version

Replace `vX.Y.Z` with the published tag. Enable Nix's `nix-command` and `flakes`
features. The flake supports Linux x86_64 and arm64 and installs all executables.

~~~bash
release_tag=vX.Y.Z
nix run "github:tee8z/maincopy/$release_tag#maincopy" -- --help
nix run "github:tee8z/maincopy/$release_tag#maincopyd" -- --version
nix profile add "github:tee8z/maincopy/$release_tag#maincopy"
~~~

Select `#maincopy` for the operator CLI; the default app runs the daemon.
The signed tag is the flake reference. No separate Nix registry upload is required.

For NixOS, add the tagged input and module to the host flake:

~~~nix
inputs.maincopy.url = "github:tee8z/maincopy/vX.Y.Z";
# Include this in the host's nixosSystem modules list:
# inputs.maincopy.nixosModules.default
~~~

Commit the host lockfile and check its resolved commit against release.json.
Nix does not verify the release's GPG signer for consumers.
Follow [deployment](deployment.md) for host configuration, credentials, and backups.

For Cargo installation, use the same exact version for each executable crate:

~~~bash
release_version=X.Y.Z
cargo install --locked --version "=$release_version" maincopy-cli
cargo install --locked --version "=$release_version" maincopy-diagram-renderer
cargo install --locked --version "=$release_version" maincopy-server
cargo install --locked --version "=$release_version" markdown-compiler
~~~

`maincopy-shared` is a library. The server also needs the matching renderer and
SSH helper, plus Git and OpenSSH. Validate executable paths and platform
credential storage; the NixOS module supplies the supported service layout.
