# Configure managed Git synchronization

Connect one read-only SSH repository and exact branch. Synchronization prepares private previews; publication requires separate approval.
For host installation, use [deployment](deployment.md).

## Configure the host

Host configuration owns the source mode, mirror, process limits, and credential file references.
SQLite stores the selected remote, branch, content directory, credential name, and poll interval.

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

Use a dedicated mirror directly beneath `paths.state_root`, separate from SQLite.
Relative paths resolve beside `maincopy.toml`. Keep private keys outside Git and the Nix store.
Resolved credential paths permit only ASCII letters, digits, slashes, periods, underscores, and hyphens.
Default limits allow 120 seconds per Git phase and a 2 GiB mirror.

## Prepare the credential

Stop `maincopyd` before initial setup. Create a protected private-key directory, then generate a dedicated key:

```console
maincopyd --config /etc/maincopy/maincopy.toml source generate-key \
  --private-key-file /var/lib/maincopy-credentials/source-key
```

Record the printed public key and fingerprint. Install only the public key as a read-only repository deploy key.
The generator refuses to overwrite the private key or its `.pub` file.

Obtain the SSH host key through a trusted, independent channel and write the verified entry to `known_hosts`.
For a nondefault port, use the OpenSSH `[host]:port` host-field form.
Unverified `ssh-keyscan` output does not establish the server's identity.

The daemon must own the private key, with mode `0600` or stricter, and have read access to both files.
The `known_hosts` file and parent directories may belong to root or the daemon; reject group or world write access.
Maincopy rejects symlinks, empty or oversized files, and unsafe ownership or permissions.

## Store initial source settings

Source setup requires an enabled Owner. For fresh state, bootstrap one while the daemon is stopped:

```console
maincopyd --config /etc/maincopy/maincopy.toml \
  identity bootstrap password --username owner
```

Enter the password at the protected prompt. Store the initial source settings:

```console
maincopyd --config /etc/maincopy/maincopy.toml source configure \
  --user git --host git.example.test --port 22 \
  --repository-path publisher/site.git --branch main \
  --content-subdirectory publication --credential-name deploy \
  --poll-interval-seconds 300
```

Use `.` for repository-root content. Poll intervals range from 30 through 86400 seconds.
For an offline repair, add `--expected-version CURRENT_VERSION` to reject a stale change.
Offline commands acquire exclusive ownership and refuse to operate against a running instance.

## Start and inspect synchronization

```console
maincopyd --config /etc/maincopy/maincopy.toml
```

Managed startup synchronizes and compiles content before opening listeners. Invalid settings or content prevent readiness.
Use your HTTPS administration origin for CLI operations:

```console
maincopy --admin-origin https://admin.example.com login --username owner
maincopy --admin-origin https://admin.example.com source status
maincopy --admin-origin https://admin.example.com source sync --wait
```

For a private CA, also supply `--admin-ca-file /path/to/trusted-ca.pem` on every invocation.
`--wait` follows the operation to completion; `--async` returns after admission.
Use `source --help` for commands and `--json` for structured results.

## Everyday Git-to-preview loop

1. Commit Markdown and local assets, then push the configured branch.
2. Wait for **Next poll**, choose **Sync now** on `/admin/source`, or run `source sync --wait`.
3. After `applied`, open **Posts** and review **Not published** or **Unpublished changes**.
4. Review the exact preview and approve publication separately.

Ordinary content edits need no restart. Failed synchronization preserves the previous private catalog and public site.
`no_change` means the branch still resolves to the installed commit.

## Change source settings online

Sign in recently as an Owner and open **Source settings** on `/admin/source`.
Select the remote, branch, content directory, registered credential name, and poll interval. Agent credentials cannot authorize reconfiguration.

For CLI changes, inspect `source status` and `source deploy-key`, then use `source configure --help`.
Supply all proposed settings, the inspected `--expected-version`, and either `--wait` or `--async`.
Compare the selected deploy public key and fingerprint with the repository registration.

The proposal must fetch and compile successfully before its settings and private catalog replace the current installation.
Failure or cancellation preserves the prior installation. Successful reconfiguration does not publish content.
An active sync and a configuration proposal cannot run together; wait for completion and reload status before another change.

After a lost response, retry identical settings with the original `--idempotency-key`, not the operation ID.
Changed settings need a new retry identity. After `interrupted`, inspect the installed settings before submitting a new proposal.
Always use the displayed version; failed proposals can leave version gaps.

## Failure diagnosis

Inspect `latest_sync.failure_code` and the operation ID with `--json source status`.
Source responses omit SSH output and credential paths; correlate operation IDs with safe server diagnostics.

| Failure code | Action |
| --- | --- |
| `credential_unavailable` | Check the credential name, ownership, and private-key mode. |
| `unknown_host` | Verify the trusted `known_hosts` entry for the exact host and port. |
| `authentication_failed` | Check read-only deploy-key registration and daemon access to the matching private key. |
| `remote_unavailable` | Check DNS, routing, SSH port, and repository availability. |
| `fetch_failed` | Check repository access, then inspect diagnostics for the operation ID. |
| `branch_unavailable` | Check the exact branch name. |
| `validation_failed`, `compile_failed` | Validate the same commit locally and fix the content in Git. |
| `candidate_failed` | Check candidate integrity and capacity before retrying. |
| `timed_out` | Check repository size and connectivity before changing host limits. |
| `interrupted` | Investigate the process failure, then request a new synchronization. |

For `cancelled`, wait for service readiness. After fixing the cause, run `source sync --wait` again.
Concurrent ordinary sync requests share an operation. A retained retry key returns its original result, not necessarily the current installation.
If a retry key or history cursor has expired, inspect current status before starting a new request.

## Candidate retention

Automatic candidate collection is disabled. New candidates are rejected at 4,096 archive/staging entries or 1 GiB of archive bytes.
Existing revisions remain intact. Do not delete candidates merely because they are old or absent from synchronization history.
Public revisions, previews, unfinished work, and retained backups can still need those artifacts.
Follow [backup and restore](backup-restore.md) for complete recovery points; the Git mirror is only a disposable transport cache.

## External checkout mode

For an operator-maintained local tree, use the default mode and omit managed credentials and mirror settings:

```toml
[source]
mode = "external_checkout"
```

`paths.content_root` selects the tree. Maincopy observes it without Git network or write operations.
`source status` reports `external_checkout`; manual source sync is unsupported because the operator owns checkout updates.
