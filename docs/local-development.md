# Run Maincopy locally

Publish the included example, then try the browser, CLI, and agent workflows.
Use [deployment](deployment.md) for a real server and [managed source](managed-source.md) for Git synchronization.

## Start the development environment

You need Nix and available ports `3000`, `3001`, `3002`, and `8443`.
CLI credentials also require an unlocked Secret Service store.
Use the same `XDG_DATA_HOME` in every terminal.

From the repository root, run:

```console
nix develop -c just start
```

Keep this terminal open. Wait for `Maincopy development environment is ready` before signing in.
On fresh state, save the generated `owner` password; later starts cannot redisplay it.
Do not share first-start output: it contains that credential.

| Surface | Address |
| --- | --- |
| Public site | `https://maincopy.localhost:8443` |
| Administration | `https://admin.localhost:8443/admin/login` |

Caddy supplies local HTTPS. Keep the public, admin, and metrics upstreams on loopback.
The launcher reads `crates/server/examples/development/` without Git network operations.
Accounts and publication state persist in `target/maincopy-dev/` across ordinary restarts.

## Publish through the browser

1. Sign in to administration as `owner` with the generated password.
2. For **Hello, Maincopy**, select **Review exact preview**.
3. Open and review the exact rendered preview.
4. Continue to publication confirmation; leave the scheduled time empty.
5. Accept the preview, approve the exact revision, and check for **Published**.

Open `https://maincopy.localhost:8443/posts/hello-maincopy` to verify publication.
The RSS feed is `/feed.xml`; `/posts/welcome` redirects to the example article.
Before publication, the article returns `404`.

If the revision or preview changes, review it again before approving.
Skip approval if that exact revision is already public.
For scheduled publication, enter a future UTC time. **Releases** provides status, rescheduling, cancellation, and blocked-release recovery.
Resolve the reported failure before retrying; failed work preserves the previous public site.

## CLI reference and diagnostics

Open another terminal at the repository root. The wrapper supplies the local admin origin and CA certificate:

```console
scripts/dev-maincopy.sh login --username owner
scripts/dev-maincopy.sh posts
scripts/dev-maincopy.sh --help
```

Browser and CLI sessions are separate. Passwords and private keys belong at protected terminal prompts, never in arguments or environment variables.
The CLI stores credentials in Secret Service. Use `logout` before replacing a stored human session.

For another server, replace the wrapper with `maincopy --admin-origin HTTPS_ORIGIN`.
Add `--admin-ca-file CA_PEM_PATH` for a private CA. Supply the same origin and trust options on every invocation.
Use command-specific `--help` for options, `--json` for structured results, and `--cursor` when a list returns another page.

### Preview and publish

Copy the post revision and top-level content digest from `posts`:

```bash
POST_ID=1dd7559b-90a9-4c5b-a13c-70bf6ec01e92
REVISION='COPY_THE_POST_REVISION'
CONTENT_DIGEST='COPY_THE_CONTENT_DIGEST'
PREVIEW_DIRECTORY="$(mktemp -d -t maincopy-preview.XXXXXXXX)"
PREVIEW_PATH="$PREVIEW_DIRECTORY/hello-maincopy.html"
scripts/dev-maincopy.sh preview "$POST_ID" --output "$PREVIEW_PATH" \
  --revision "$REVISION" --content-digest "$CONTENT_DIGEST"
```

Review the downloaded file. Existing files are never overwritten.
Use the browser preview when styles or protected assets cannot load from a `file:` URL.
Copy the reported preview digest, then approve:

```bash
PREVIEW_DIGEST='COPY_THE_PREVIEW_DIGEST'
scripts/dev-maincopy.sh publish-now "$POST_ID" \
  --preview-digest "$PREVIEW_DIGEST" --revision "$REVISION"
scripts/dev-maincopy.sh releases list
scripts/dev-maincopy.sh releases --help
```

Keep the returned publication and operation IDs. Inspect the current resource version before changing a release.

### Recover an uncertain mutation

After a lost response, inspect current state and any operation receipt.
Retry identical inputs with the original `--idempotency-key` reported by the command.
Do not substitute the resource ID or source-sync ID. Changed intent needs a new operation; stale versions require another inspection.
Account and agent-grant retries also require the original authorizing session.

### Check public readiness

```bash
MAINCOPY_DATA_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}"
curl --noproxy '*' --cacert "$MAINCOPY_DATA_ROOT/maincopy/dev-ca/rootCA.pem" \
  --max-time 5 --fail --silent --show-error \
  https://maincopy.localhost:8443/health/ready
```

Expect `{"status":"ready"}`. This checks service readiness, not whether an article is published.

## Accounts, profiles, and tips

Use **Users** for accounts and login credentials, **Profile** for public identity, and **Tips** for the selected recipient.
Only Owners assign roles; other actions remain limited by the current account's authority.
Credential changes require recent authentication and end affected sessions. Disabling an account also revokes its agent grants.

To replace your password, open **Users → Manage your account** within 15 minutes of signing in.
For Nostr browser login, unlock a NIP-07 signer containing the account's key, then select **Sign in with Nostr**.
For human CLI login with an external signer, use `login-nostr` and follow its protected challenge prompt.

Use `users --help`, `profile --help`, and `tip-recipient --help` for CLI operations.
Role changes replace the complete role set; credential changes use the credential version, not the account version.
Profile updates replace every field, so omitted optional values are cleared.

## Agent grants

Configure a separate local agent key, then inspect its public identity:

```console
scripts/dev-maincopy.sh agent-key set
scripts/dev-maincopy.sh agent-key inspect
```

Skip `set` if the intended key is already configured. Inspection does not prove that a server grant exists.
Use **Agents → Register an agent** with recent human authentication, or use the CLI:

```console
scripts/dev-maincopy.sh users list
scripts/dev-maincopy.sh agents register \
  --owner-user-id OWNER_UUID --public-key PUBLIC_KEY_HEX \
  --label publishing-helper --scopes content_read,release_manage
scripts/dev-maincopy.sh agents inspect GRANT_UUID
scripts/dev-maincopy.sh --auth-context agent posts
```

Replace the placeholders with the Owner UUID, inspected public key, and returned grant UUID.
Compare the saved public key and fingerprint. The owner's current roles limit the requested scopes.
Use `agents --help` for expiry, scope replacement, and revocation; inspect the grant version before changing it.
Expired or revoked grants cannot authenticate. Human authentication remains the default unless `--auth-context agent` is supplied.

Each admin origin has its own protected local key. `agent-key remove` deletes only that key; revoke the server grant separately.

## Browser trust lifecycle

`just start` installs the development CA in supported user NSS stores; `just start-cli` skips browser trust installation.
Restart an open browser if it still reports a certificate error.
The durable CA lives at `${XDG_DATA_HOME:-$HOME/.local/share}/maincopy/dev-ca/`, outside disposable state.

Keep `rootCA-key.pem` private: its holder can issue certificates trusted by those browser stores.
After stopping the launcher, run `just untrust-browser` to remove browser trust while keeping the CA files.
CLI commands can still trust `rootCA.pem` explicitly.

## Preserve or reset publication state

Before a reset, sign out of the browser and run CLI `logout` while services remain available.
Stop the launcher with `Ctrl+C` before moving or resetting state. Ordinary restarts need no sign-out.

> [!CAUTION]
> `just reset` deletes all of `target/maincopy-dev/`; `just quickstart` deletes it and starts again.
> Both preserve the durable CA and browser trust. Reset refuses active locks.

To preserve private application state before starting fresh:

```bash
STATE_ARCHIVE="$(mktemp -d "$HOME/maincopy-dev-state.XXXXXXXX")"
mv -- target/maincopy-dev/state "$STATE_ARCHIVE/state"
```

Keep the archive protected. The next start creates new state and prints a new owner password.
Recipes enter the Nix shell automatically; prefix them with `nix develop -c` if `just` is unavailable.

## Troubleshooting

| Symptom | Action |
| --- | --- |
| Credential store unavailable | Use the graphical session that owns Secret Service and unlock its default collection. |
| Human session already stored or rejected | Run `scripts/dev-maincopy.sh logout`, then sign in again. |
| Logout cannot reach the server | Restore server availability; transport failures retain local credentials. |
| Initial password lost | Replace it through a recent browser session, or preserve/reset disposable state. |
| CA missing or certificate error | Check `XDG_DATA_HOME`; create the CA with `just start-cli`, install browser trust with `just start`, then restart the browser. |
| Startup never ready | Inspect local diagnostics without sharing credentials; check the required ports and conflicting processes. |
| Retained revision unavailable | Stop services and preserve needed state before rebuilding disposable state with `just quickstart`. |
| Gateway lock held | Stop the previous gateway normally. Do not delete `gateway.lock`; process exit releases the lock. |
