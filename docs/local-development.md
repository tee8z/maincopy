# Run Maincopy locally

Use this runbook on one Linux workstation to publish the included example and
manage local accounts. For production, use the [deployment runbook](deployment.md).
See [managed source](managed-source.md) for Git mirroring and
[content rendering](content-rendering.md#images) for article images.

## Start the development environment

You need Nix, available ports `3000`, `3001`, `3002`, and `8443`, and an unlocked
Secret Service store for CLI credentials. Use the same `XDG_DATA_HOME` in every terminal.

From the repository root, run:

```console
nix develop -c just start
```

Keep this terminal open. The launcher builds the daemon, isolated Mermaid renderer,
and CLI, then starts the daemon and Caddy. Wait for
`Maincopy development environment is ready` before signing in.

| Command | Effect |
| --- | --- |
| `just start` | Start with existing state and install browser trust. |
| `just start-cli` | Start with existing state without changing browser trust. |
| `just quickstart` | **Delete disposable state**, then start with browser trust. |
| `just reset` | **Delete disposable state** without starting services. |
| `just untrust-browser` | Remove browser trust after stopping the launcher. |

These recipes enter the Nix shell automatically. If `just` is unavailable, prefix
its command with `nix develop -c`.

On fresh state, the daemon creates `owner` and prints its generated password once.
Save it immediately; later starts cannot redisplay it. Only its password hash is stored.

> [!WARNING]
> First-start output contains a credential. Do not share it in terminal logs,
> issue reports, shell files, or screenshots.

| Surface | Address |
| --- | --- |
| Public site | `https://maincopy.localhost:8443` |
| Administration | `https://admin.localhost:8443/admin/login` |
| Public/admin upstreams | `127.0.0.1:3000` / `127.0.0.1:3001` |
| Metrics | `127.0.0.1:3002` |

Caddy terminates HTTPS and forwards to the separate public and admin listeners.
Do not expose the loopback upstreams. This gateway runs as the developer and is
not the production service boundary.

The fixture lives in `crates/server/examples/development/`. Its `external_checkout`
mode reads checked-in content without Git network operations. Runtime state persists
in `target/maincopy-dev/`; ordinary restarts preserve accounts and publication state.

## Publish through the browser

1. Open the administration URL and sign in as `owner` with the generated password.
2. For **Hello, Maincopy**, select **Review exact preview**.
3. Select **Open exact rendered preview** and review the complete article.
4. Select **Continue to publication confirmation**.
5. Leave the scheduled time empty for immediate publication.
6. Select **I reviewed and accept this exact preview**, then **Approve this exact revision**.
7. Check that the release page reports **Published**.

Open `https://maincopy.localhost:8443/posts/hello-maincopy` to see the result.
The RSS feed is `/feed.xml`; the fixture alias `/posts/welcome` redirects to the article.
If the exact revision is already public, no new approval is needed.

Approval binds the candidate revision and preview digest. If either changes,
review the new preview. Reloading the page cannot approve an update.

For later publication, enter a future UTC time before approval. Later source edits
do not change that approved revision. Use **Releases** to inspect, reschedule,
cancel, or retry a blocked release. Refresh after a version conflict.

A blocked release leaves the previous public snapshot available. Resolve its
reported cause before retrying. Cancellation retains history and does not remove
an existing public revision.

## CLI reference and diagnostics

Open another terminal at the repository root with the same `XDG_DATA_HOME`.
The wrapper supplies the local admin origin and CA certificate:

```console
scripts/dev-maincopy.sh login --username owner
scripts/dev-maincopy.sh --help
scripts/dev-maincopy.sh source status
scripts/dev-maincopy.sh posts
```

Browser login does not create a CLI session. The CLI reads passwords from a
protected terminal and stores sessions in Secret Service. Never put passwords or
private keys in arguments or environment variables.

Use `--json` for structured results. Lists return at most 100 records; pass the
returned cursor to the same list command with `--cursor NEXT_CURSOR`.
Command-specific `--help` lists the supported options.

### Preview and publish

From `posts`, copy the post's `Revision` and the top-level `Content` digest.
For the included post, prepare a new preview destination:

```bash
POST_ID=1dd7559b-90a9-4c5b-a13c-70bf6ec01e92
REVISION='COPY_THE_POST_REVISION'
CONTENT_DIGEST='COPY_THE_CONTENT_DIGEST'
PREVIEW_DIRECTORY="$(mktemp -d -t maincopy-preview.XXXXXXXX)"
PREVIEW_PATH="$PREVIEW_DIRECTORY/hello-maincopy.html"
```

```console
scripts/dev-maincopy.sh preview "$POST_ID" \
  --output "$PREVIEW_PATH" \
  --revision "$REVISION" --content-digest "$CONTENT_DIGEST"
```

Open the downloaded file and review it. The CLI never overwrites an existing file.
Root-relative styles and protected assets may not load from a `file:` URL;
use the browser preview when you need those resources.

Copy the reported `Preview` digest, then approve:

```bash
PREVIEW_DIGEST='COPY_THE_PREVIEW_DIGEST'
```

```console
scripts/dev-maincopy.sh publish-now "$POST_ID" \
  --preview-digest "$PREVIEW_DIGEST" --revision "$REVISION"
```

Retain the returned publication and operation IDs. Use the inspected release
version for subsequent changes; replace uppercase placeholders below:

```console
scripts/dev-maincopy.sh releases list
scripts/dev-maincopy.sh releases inspect PUBLICATION_ID
scripts/dev-maincopy.sh releases operation OPERATION_ID
scripts/dev-maincopy.sh releases reschedule PUBLICATION_ID \
  --expected-version VERSION --at UTC_RFC3339
scripts/dev-maincopy.sh releases cancel PUBLICATION_ID --expected-version VERSION
scripts/dev-maincopy.sh releases retry PUBLICATION_ID --expected-version VERSION
```

### Recover an uncertain mutation

Resource mutations generate an idempotency UUID when `--idempotency-key` is omitted.
After a lost response, inspect current state and any available operation receipt.
Retry only identical inputs with the original `--idempotency-key ORIGINAL_KEY`
reported by the command. Do not substitute a resource or source-sync identifier.
Use a new operation for a changed intent; reload after a stale-version error.

A receipt describes the accepted result, which can differ from the current resource.
Account and agent-grant retries also require the original authorizing session.
After signing in again, inspect current state before starting a new operation.

### Check public readiness

```bash
MAINCOPY_DATA_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}"
ROOT_CERTIFICATE="$MAINCOPY_DATA_ROOT/maincopy/dev-ca/rootCA.pem"
```

```console
curl --noproxy '*' --cacert "$ROOT_CERTIFICATE" --max-time 5 \
  --fail --silent --show-error \
  https://maincopy.localhost:8443/health/ready
```

Expect `{"status":"ready"}`. A fresh unpublished article returns `404` at its public URL.
After publication, check the article, feed, and alias in the browser.

## Accounts and login credentials

Use **Users** to inspect or create accounts and manage credentials. New accounts
receive Publisher by default. Only Owners assign roles; Administrators can manage
accounts within their own authority. Publishers cannot open account administration.

Credential changes require a recent sign-in and the displayed resource version.
Replacing or removing a credential ends that user's existing sessions.
Disabling an account also revokes agent grants; enabling it does not restore them.
Maincopy preserves an enabled Owner and a usable credential for each enabled account.

To replace your password, open **Users → Manage your account** within 15 minutes
of signing in. Save a password of 15–128 characters, then sign in again.

For Nostr browser login, select the account's key in a
[NIP-07 signer](https://github.com/nostr-protocol/nips/blob/master/07.md), then choose
**Sign in with Nostr**. The private key stays in the signer.
If the result is uncertain, choose **Open administration** before retrying.

The account's **Nostr login key** form accepts a public key as 64 lowercase
hexadecimal characters. Compare the saved public key and fingerprint after a change.

Common CLI operations:

```console
scripts/dev-maincopy.sh users list
scripts/dev-maincopy.sh users inspect USER_UUID
scripts/dev-maincopy.sh users create --roles publisher password --username publisher
scripts/dev-maincopy.sh users create --roles publisher nostr --public-key PUBLIC_KEY_HEX
scripts/dev-maincopy.sh users status USER_UUID --expected-version VERSION --status disabled
scripts/dev-maincopy.sh users roles USER_UUID --expected-version VERSION --roles publisher
scripts/dev-maincopy.sh users credentials USER_UUID --help
```

`users roles` replaces the complete role set. Credential replacement and removal
require the **credential version** from inspection, not the separate account version.
Password commands use a hidden confirmation prompt.

Run `scripts/dev-maincopy.sh logout` before replacing a stored CLI session.
For human CLI login with an external Nostr signer:

```console
scripts/dev-maincopy.sh login-nostr
```

Sign the exact displayed event within 60 seconds, then paste its signed JSON as
one line at the protected prompt. Restart the command for an expired challenge.
This flow uses the human account's signer, not the local agent key.

## Profile and tips

Owners and Administrators can use **Profile** to set a display name, Lightning
Address, and tip eligibility. **Tips** selects the recipient by user ID.
An empty field clears its value. Ineligible recipients leave articles readable
without tip links. These edits do not approve articles.

```console
scripts/dev-maincopy.sh profile show
scripts/dev-maincopy.sh profile create --display-name Alice \
  --lightning-address alice@example.com --tips-enabled true
scripts/dev-maincopy.sh profile update --expected-version VERSION \
  --display-name Alice --lightning-address alice@example.com --tips-enabled false
scripts/dev-maincopy.sh tip-recipient show
scripts/dev-maincopy.sh tip-recipient set USER_UUID --expected-version VERSION
scripts/dev-maincopy.sh tip-recipient clear --expected-version VERSION
```

`profile create` requires an absent profile. Updates replace every field;
omitted display-name or Lightning Address options clear those values.
`--tips-enabled` is required. Recipient changes use the setting version from `show`.

## Agent grants

Use **Agents** or `agents` commands to delegate administration to a Nostr key.
Registration requires recent authentication, `credential_manage`, and authority
over the selected owner account. Its current roles limit the grant's effective scopes.

Configure and inspect the local key for this admin origin:

```console
scripts/dev-maincopy.sh agent-key set
scripts/dev-maincopy.sh agent-key inspect
```

Skip `set` if the intended key is already configured. It reads the private key from
the protected terminal. Inspection makes no API request and reports only the public
key and its fingerprint: `SHA256:` followed by the raw key's hash in unpadded Base64.
Local inspection does not prove that a server grant exists or remains active.

Open **Agents → Register an agent**, or register the inspected public key from the CLI:

```console
scripts/dev-maincopy.sh agents register \
  --owner-user-id OWNER_UUID --public-key PUBLIC_KEY_HEX \
  --label publishing-helper --scopes content_read,release_manage
scripts/dev-maincopy.sh agents list
scripts/dev-maincopy.sh agents inspect GRANT_UUID
```

Add `--expires-at UTC_RFC3339` for a future UTC expiry. Requested scopes must be
nonempty, unique, and within the authenticated authority. Compare the saved public
key and fingerprint. Inspection shows ownership, requested/effective scopes, expiry,
last use, revocation, and the version needed for changes.

```console
scripts/dev-maincopy.sh agents scopes GRANT_UUID \
  --expected-version VERSION --scopes content_read
scripts/dev-maincopy.sh agents revoke GRANT_UUID --expected-version VERSION
scripts/dev-maincopy.sh --auth-context agent posts
```

Scope replacement replaces the complete requested set. Inspect the new version
before another change. Expired or revoked grants cannot authenticate.
Use `--auth-context agent` explicitly; human authentication remains the default.

Each admin origin has a separate protected key entry. Outside development, replace
the wrapper with `maincopy --admin-origin HTTPS_ORIGIN`.
For a private CA, also supply `--admin-ca-file CA_PEM_PATH`.
Use the same origin when configuring, inspecting, and using the key.
`agent-key remove` deletes only the local key; revoke the server grant separately.

## Browser trust lifecycle

Browser starts install the durable development CA in supported user NSS stores.
Restart an open browser if it still reports a certificate error.
The CA lives outside disposable state:

- `$XDG_DATA_HOME/maincopy/dev-ca/`, when `XDG_DATA_HOME` is set;
- `$HOME/.local/share/maincopy/dev-ca/`, otherwise.

Its certificate is `rootCA.pem`; its private key is `rootCA-key.pem`.

> [!WARNING]
> Never share or commit `rootCA-key.pem`. Its holder can issue certificates
> trusted by every browser store where you installed this CA.

Stop the launcher before running `just untrust-browser`. This removes browser
trust but keeps the CA files. CLI commands can still trust `rootCA.pem` explicitly.
`just start-cli` creates the CA if needed without installing browser trust.

## Preserve or reset publication state

Sign out of the browser and run `scripts/dev-maincopy.sh logout` while services
are available. Stop the launcher with `Ctrl+C` before resetting or moving state.
Ordinary restarts do not require sign-out.

> [!CAUTION]
> `just reset` and `just quickstart` delete **all of `target/maincopy-dev/`**,
> including its database, retained content, gateway data, and leaf certificates.
> They preserve the durable CA and its browser trust. Reset refuses active locks.

To preserve state before starting fresh, move it outside the reset directory:

```bash
STATE_ARCHIVE="$(mktemp -d "$HOME/maincopy-dev-state.XXXXXXXX")"
mv -- target/maincopy-dev/state "$STATE_ARCHIVE/state"
printf 'Preserved prior state at %s\n' "$STATE_ARCHIVE"
```

The archive contains private application state; keep it protected.
The next start creates new state and prints a new owner password.
If a retained CLI session is rejected, use `logout` to clear it before signing in again.

## Troubleshooting

| Symptom | Action |
| --- | --- |
| Credential store unavailable | Run from the graphical session that owns Secret Service and unlock its default collection. |
| Human session already stored | Run `scripts/dev-maincopy.sh logout`, then sign in again. |
| Logout cannot reach the server | Restore server availability. Transport failures retain local credentials; explicit session rejection clears them. |
| Initial password lost | With a recent browser session, replace it under **Users → Manage your account**. Otherwise preserve or reset disposable state. |
| CA missing or browser certificate error | Check `XDG_DATA_HOME` in both terminals. Run `just start-cli` if the CA is absent; restart the browser after installing trust. |
| Nostr signer locked or cancelled | Unlock the signer and start a new sign-in attempt. |
| Stale resource version | Inspect or reload current state, review the change, and submit a new operation. |

If startup never becomes ready, inspect server and Caddy diagnostics without sharing
first-start credentials. Check ports `3000`, `3001`, `3002`, and `8443`.
Stop a conflicting process only when you own it and no longer need it.

If a retained revision is unavailable, stop the launcher and preserve needed state
before using `just quickstart` to rebuild from the current example content.

Only one gateway can use the durable CA at a time. Stop an earlier gateway normally;
do not remove `gateway.lock`. The operating system releases its lock on process exit.

For production identities, network exposure, and recovery, follow the
[deployment runbook](deployment.md) and [remaining acceptance](implementation.md).
