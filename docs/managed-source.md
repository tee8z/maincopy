# Configure managed Git synchronization

Use this runbook for one read-only SSH repository and exact branch.
Synchronization prepares private previews; publication requires separate approval.
See [design](design.md#content-sources-and-retention) for the source and retention boundaries.

## Configuration ownership

| Location | Values |
| --- | --- |
| Host `maincopy.toml` | Source mode, mirror path, process limits, named credential file references |
| Protected host files | SSH private key and verified `known_hosts` entries |
| SQLite | Remote, branch, content subdirectory, credential name, poll interval, sync operations |

Register each credential in the host configuration. Offline and online source settings select its name:

```toml
[paths]
state_root = "/var/lib/maincopy"
runtime_root = "/run/maincopy"

[source]
mode = "managed_git"
mirror_root = "/var/lib/maincopy/source-mirror"

[source.ssh_credentials.deploy]
private_key_file = "/var/lib/maincopy-credentials/source-key"
known_hosts_file = "/etc/maincopy/source-known-hosts"
```

The mirror must be a dedicated direct child of `paths.state_root` and must not contain SQLite.
Relative host paths resolve beside `maincopy.toml`. Keep private keys outside Git and the Nix store.
Resolved credential paths permit only ASCII letters, digits, slashes, periods, underscores, and hyphens.

These `[source]` settings bound Git operations. Values must be positive and within the host parser's maximums.

| Setting | Default | Purpose |
| --- | ---: | --- |
| `fetch_timeout_seconds` | `120` | Wall time per Git phase |
| `command_output_bytes` | `33554432` | Captured output |
| `mirror_bytes` | `2147483648` | Mirror size |
| `file_bytes` | `1073741824` | Child-process file size |
| `address_space_bytes` | `2147483648` | Child-process address space |
| `cpu_seconds` | `120` | Child-process CPU time |
| `open_files` | `256` | Child-process open files |

## Prepare the credential

Stop `maincopyd` before credential generation and initial setup.

1. Create a protected parent directory for the private key.
2. Install the host configuration above.
3. Generate a dedicated Ed25519 key:

   ```console
   maincopyd --config /etc/maincopy/maincopy.toml \
     source generate-key \
     --private-key-file /var/lib/maincopy-credentials/source-key
   ```

4. Record the printed public key and SHA-256 fingerprint.
5. Install only the public key as a read-only repository deploy key.
6. Obtain the host key through a trusted, independent channel.
7. Write that verified entry to the configured `known_hosts` file.

For a nondefault port, use the OpenSSH `[host]:port` host-field form.
The generator refuses to overwrite the private key or its `.pub` file.

> [!WARNING]
> Use read-only repository access. Write access increases the effect of a compromised deploy key.

> [!CAUTION]
> Do not trust unverified `ssh-keyscan` output. A substituted host key can direct the connection to an attacker.

Give the daemon read access to both files. The private key must belong to the daemon user, with mode `0600` or stricter.
The `known_hosts` file may belong to root or the daemon user, without group or world write access.
Use root-owned or daemon-owned parent directories without group or world write access.
Maincopy rejects symlinks, empty or oversized files, and unsafe ownership or permissions. It rechecks credential paths before each transport command.

## Store source settings

Source setup requires an enabled owner. For new state, bootstrap the owner while the daemon is stopped:

```console
maincopyd --config /etc/maincopy/maincopy.toml \
  identity bootstrap password --username owner
```

Enter the password only at the protected prompt. Then store the initial source settings:

```console
maincopyd --config /etc/maincopy/maincopy.toml \
  source configure \
  --user git --host git.example.test --port 22 \
  --repository-path publisher/site.git --branch main \
  --content-subdirectory publication --credential-name deploy \
  --poll-interval-seconds 300
```

Use `.` for a repository-root content directory. Poll intervals range from 30 through 86400 seconds.
For repairs, add `--expected-version CURRENT_VERSION` to reject stale settings changes.
Offline commands acquire exclusive ownership and bind no listener; they refuse to run against a running instance.

## Start and inspect synchronization

Start the configured daemon:

```console
maincopyd --config /etc/maincopy/maincopy.toml
```

Managed startup synchronizes and compiles content before opening listeners. Invalid settings or content prevent readiness.

Use the same HTTPS administration origin for every CLI invocation; replace the examples with your deployed origin.
For a private CA, add `--admin-ca-file /path/to/trusted-ca.pem` each time.
The CLI does not remember origin or CA options.

```console
maincopy --admin-origin https://admin.example.com login --username owner
maincopy --admin-origin https://admin.example.com source status
maincopy --admin-origin https://admin.example.com source sync --wait
```

`--wait` follows the durable operation to completion. Use `--async` instead to return after admission.
Add `--json` for machine-readable output and `--idempotency-key UUID` for repeatable manual requests.

## Everyday Git-to-preview loop

1. Commit Markdown and local assets, then push the configured branch.
2. Wait for **Next poll**, choose **Sync now** on `/admin/source`, or run the CLI synchronization above.
3. When the result is `applied`, open **Posts** and review **Not published** or **Unpublished changes**.
4. Review the exact preview, then publish that revision explicitly.

Ordinary content changes need no restart or source reconfiguration.
Failed fetches or compilation preserve the previous private catalog and public site.

## Change source settings online

Sign in recently as an Owner, then open **Source settings** on `/admin/source`.
The form accepts a remote, branch, subdirectory, host-registered credential name, and poll interval.
Agent credentials cannot authorize these changes.

For CLI changes, inspect the installed version and deploy identity:

```console
maincopy --admin-origin https://admin.example.com source status
maincopy --admin-origin https://admin.example.com source deploy-key
```

The deploy-key command prints the selected public key and fingerprint, without credential paths.
Submit all proposed settings, replacing `1` with the installed version:

```console
maincopy --admin-origin https://admin.example.com source configure \
  --user git --host git.example.test --port 22 \
  --repository-path publisher/site.git --branch main \
  --content-subdirectory publication --credential-name deploy \
  --poll-interval-seconds 300 --expected-version 1 --wait
```

Use `--async` to return after admission. Repeat an uncertain request with its original `--idempotency-key UUID` and identical settings.

Installed settings, polling, and the private catalog remain authoritative until the proposal passes fetch, validation, compilation, and durable installation.
Successful installation activates the settings and catalog together; it does not publish.
Failure or cancellation preserves the prior installation. Versions can have gaps, so always read the displayed version.

Reconfiguration conflicts with an active sync, and ordinary sync conflicts with an active proposal.
Wait for completion, then reload status before another settings change.
A restart marks an unfinished proposal `failed` with code `interrupted`, then synchronizes the installed settings.
The old operation retains its terminal result. Submit corrected settings with a new retry identity.

## Operation behavior

Concurrent ordinary sync requests share one durable operation identifier.

| Outcome | Meaning |
| --- | --- |
| `applied` | A new private candidate was installed. |
| `no_change` | The branch still resolves to the installed commit. |
| `cancelled` | Shutdown stopped the operation before completion. |
| `failed` | Inspect the stable failure code. |

A live `no_change` skips compilation; startup recompiles retained content to reconstruct serving state.
Shutdown drains work already compiling or committing. Unexpected termination leaves an `interrupted` result after restart.

Maincopy retains the newest 4,096 manual retry aliases. Reusing a retained key returns its original operation.
An expired key conflicts instead of starting new work; use a fresh key.
History retains 4,096 terminal operations plus active, installed, and alias-referenced operations. Older history cursors can expire.

## Failure diagnosis

Inspect `latest_sync.failure_code` and the operation ID:

```console
maincopy --admin-origin https://admin.example.com --json source status
```

| Failure code | Action |
| --- | --- |
| `credential_unavailable` | Check the selected name, ownership, and private-key mode. |
| `unknown_host` | Verify the trusted `known_hosts` entry for the exact host and port. |
| `authentication_failed` | Check read-only deploy-key registration and daemon access to the matching private key. |
| `remote_unavailable` | Check DNS, routing, SSH port, and repository availability. |
| `fetch_failed` | Check repository access, then correlate the operation ID with safe server logs. |
| `branch_unavailable` | Check the exact remote branch name. |
| `validation_failed`, `compile_failed` | Validate the same commit locally and fix the content in Git. |
| `candidate_failed` | Check candidate integrity and capacity; correlate the operation ID with server logs. |
| `timed_out` | Check repository size and connectivity before increasing limits. |
| `interrupted` | Investigate the previous process failure, then request a new sync. |

For a `cancelled` outcome, wait for service readiness before retrying.
After correcting the cause, run:

```console
maincopy --admin-origin https://admin.example.com source sync --wait
```

Source resources omit SSH output and credential paths. Use operation IDs and failure codes to correlate safe logs.

## Candidate retention contract

Automatic candidate collection is disabled. At 4,096 archive/staging entries or 1 GiB of archive bytes, new candidates are rejected.
Existing revisions remain intact. The Git mirror is a disposable transport cache, not a backup.

Retention must preserve every candidate reachable from these roots:

| Root | Required artifacts |
| --- | --- |
| Installed source and private catalog | Installed candidate and every previewable revision |
| Current public revisions | Exact Markdown, assets, renderer identities, and compiled output |
| Nonterminal releases | Inputs needed to complete or recover each release |
| Active sync or configuration proposal | Materialized candidates until a terminal result is durable |
| Compatible backup recovery points | Every digest named by a retained backup manifest |

Directory age, expired cursors, and pruned sync history do not prove an artifact is unused.
Do not remove candidates to recover capacity without accounting for all roots.
See [backup and restore](backup-restore.md) for complete recovery points.

## External checkout mode

For an operator-maintained local tree, omit the managed mirror and credential registry:

```toml
[source]
mode = "external_checkout"
```

This is the default mode. `paths.content_root` selects the tree; Maincopy observes it without Git network or write operations.
`source status` reports `external_checkout`. Manual source sync is unsupported because the operator owns checkout updates.
