# Run the local alpha

Status: supported development workflow

Related: [project overview](../README.md),
[managed source runbook](managed-source.md),
[implementation plan](implementation.md), and [engineering style](quality.md).

Use this runbook to publish the included Markdown post through the local HTTPS
gateway. Start with the browser walkthrough. Use the command-line interface
(CLI) reference for detailed checks and diagnostics.

This workflow is for development on one Linux workstation. It is not a
production deployment or NixOS acceptance test.

## Boundaries and durable state

| Surface | Address | Purpose |
| --- | --- | --- |
| Public HTTPS origin | `https://maincopy.localhost:8443` | Canonical pages, RSS, and public assets |
| Administration HTTPS origin | `https://admin.localhost:8443` | Authenticated browser and CLI requests |
| Public loopback upstream | `127.0.0.1:3000` | Caddy access only |
| Administration loopback upstream | `127.0.0.1:3001` | Caddy access only |

The local-alpha fixture is in `crates/server/examples/local-alpha/`. Runtime
state persists in `target/maincopy-dev/` between launcher restarts.

This fixture uses `external_checkout`, the default source mode. The launcher
observes its checked-in content tree and performs no Git network operation.

The launcher runs the Rust daemon and Caddy as separate processes. Caddy
terminates HTTPS and forwards to the two loopback listeners; `maincopyd` does
not act as its own gateway.

The development certificate authority (CA) persists outside `target/`:

- `$XDG_DATA_HOME/maincopy/dev-ca/`, when `XDG_DATA_HOME` is set.
- `$HOME/.local/share/maincopy/dev-ca/`, in other supported sessions.

The CA certificate is `rootCA.pem`. The CA private key is `rootCA-key.pem`.

> [!WARNING]
> Never share or commit `rootCA-key.pem`. Its holder can issue certificates
> trusted by each browser store that contains this development CA.

> [!CAUTION]
> Do not expose either loopback upstream. The development Caddy process is the
> only supported network path for this workflow.

## Prerequisites

1. Start from a Linux user session that can run Nix.
2. Use the same `XDG_DATA_HOME` value in each terminal.
3. Ensure that ports `3000`, `3001`, and `8443` are available.
4. For the CLI workflow, provide an unlocked Secret Service store.

The human CLI stores its session in Secret Service. It does not use a plaintext
file, process argument, or environment variable for this credential.

## Browser walkthrough

### 1. Start the services

Enter the development shell from the repository root:

```console
nix develop
```

Start the local alpha and install its CA in supported browser stores:

```console
scripts/dev-alpha.sh --trust-browser
```

The launcher builds `maincopyd`, its isolated `maincopy-mermaid` renderer, and
`maincopy`. It then starts the server and the Caddy gateway. Keep this terminal
open.

On fresh state, `maincopyd` generates the `owner` account with an
instance-unique password from 256 bits of operating-system randomness. It
prints the username and password once before it persists the identity
transaction. Copy the password from the launcher output immediately.

Maincopy stores only the Argon2id password hash. It has no shared default
password, and it does not display this password on later starts. The first
publication UI does not include password rotation.

The credential output appears before the readiness output:

```text
Maincopy generated the initial owner credential.

  Username: owner
  Password: COPY_THIS_GENERATED_VALUE

Save this password now. Maincopy will not display it after identity setup.
```

> [!WARNING]
> Treat first-start standard output as credential material. Do not copy the
> generated password into shared terminal logs, issue reports, or shell files.

Wait for this output:

```text
Maincopy local alpha is ready.

  Public: https://maincopy.localhost:8443
  Admin:  https://admin.localhost:8443/admin/login
  CLI:    scripts/dev-maincopy.sh login --username owner
```

### 2. Sign in and inspect the revision

Open `https://admin.localhost:8443/admin/login`. Sign in as `owner` with the
generated password.

For `Hello, Maincopy`, choose `Review exact preview`. Compare the current public
revision with the candidate revision. New state displays `Not published`.

### 3. Review the exact preview

Choose `Open exact rendered preview`. Review the complete article before you
continue. Confirm that the Rust source is escaped plain code and that the
Mermaid source appears as a diagram. The CLI diagnostics below verify the
semantic `language-rust` class in the rendered HTML.

### 4. Publish and verify the article

Choose `Continue to publication confirmation`. On the separate confirmation
page, select `I reviewed and accept this exact preview`, then choose `Publish
this exact revision`.

The confirmation page identifies the candidate revision and exact preview
digest. If either value changes, open the new preview before publication.

New state publishes the initial article. If the live post has a newer candidate,
the same workflow publishes an update. A reload cannot publish that update by
itself.

Open `https://maincopy.localhost:8443/posts/hello-maincopy`. Confirm that the
reviewed article is public.

On a repeat run, `Published` and `This exact revision is already public` mean
that no publication action is needed. The public URL already serves the current
candidate.

### 5. Sign out before a state reset

Return to the post list and choose `Sign out` before resetting local state.
The browser returns to the sign-in page and clears the session and CSRF cookies.
You may keep the session for an ordinary launcher restart because restarts
preserve the same state.

## Browser trust lifecycle

The browser walkthrough installs the durable development CA in supported user
Network Security Services (NSS) browser stores. Restart an open browser if it
still reports a certificate error.

To run only CLI diagnostics, start the launcher without changing browser trust:

```console
scripts/dev-alpha.sh
```

To remove browser trust, first stop the launcher. Then run:

```console
scripts/dev-gateway.sh --untrust-browser
```

This command removes browser trust and keeps the durable CA files. The CLI can
continue to trust `rootCA.pem` explicitly on later runs.

## CLI reference and diagnostics

This section preserves the detailed API checks for development and fault
diagnosis. Browser trust is not required because the CLI uses the CA file
directly.

If the local alpha is not running, start it from the repository root:

```console
nix develop
scripts/dev-alpha.sh
```

Open a second terminal at the repository root. Enter `nix develop` with the
same `XDG_DATA_HOME` value.

Set the public origin and CA certificate path:

```bash
PUBLIC_ORIGIN=https://maincopy.localhost:8443
DATA_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}"
ROOT_CERTIFICATE="$DATA_ROOT/maincopy/dev-ca/rootCA.pem"
POST_ID=1dd7559b-90a9-4c5b-a13c-70bf6ec01e92
```

Confirm that the public service is ready:

```console
curl --noproxy '*' --cacert "$ROOT_CERTIFICATE" --max-time 5 \
  --fail --silent --show-error \
  "$PUBLIC_ORIGIN/health/ready"
```

Expected output:

```json
{"status":"ready"}
```

For new state, confirm that the canonical route is not public:

```console
curl --noproxy '*' --cacert "$ROOT_CERTIFICATE" --max-time 5 \
  --silent --show-error --output /dev/null \
  --write-out '%{http_code}\n' \
  "$PUBLIC_ORIGIN/posts/hello-maincopy"
```

Expected output:

```text
404
```

### 1. Log in

Run the human login command:

```console
scripts/dev-maincopy.sh login --username owner
```

Enter the generated initial owner password. A successful command reports the
session, user, provider, roles, and expiry time.

### 2. Select the loaded revision

Confirm that this fixture uses the operator-maintained checkout:

```console
scripts/dev-maincopy.sh source status
```

Expected source mode:

```text
Source mode: external_checkout
```

List the loaded posts:

```console
scripts/dev-maincopy.sh posts
```

Find the `Hello, Maincopy` record. Copy its `Revision` value and the top-level
`Content` value into these variables:

```bash
REVISION='COPY_THE_POST_REVISION'
CONTENT_DIGEST='COPY_THE_CONTENT_DIGEST'
```

For a new state directory, the record has the `unpublished` status.

### 3. Review the exact preview

Create a new preview destination:

```bash
PREVIEW_DIRECTORY="$(mktemp -d -t maincopy-preview.XXXXXXXX)"
PREVIEW_PATH="$PREVIEW_DIRECTORY/hello-maincopy.html"
```

Download the selected preview:

```console
scripts/dev-maincopy.sh preview "$POST_ID" \
  --output "$PREVIEW_PATH" \
  --revision "$REVISION" \
  --content-digest "$CONTENT_DIGEST"
```

Open `PREVIEW_PATH` and review the article. Confirm that the Rust block remains
escaped plain code inside the canonical `language-rust` wrapper and has no
token-level highlighting spans. Confirm that the Mermaid source has become a
diagram rather than remaining source text. Copy the reported `Preview` value:

```bash
PREVIEW_DIGEST='COPY_THE_PREVIEW_DIGEST'
```

The CLI never overwrites an existing preview file. Create a new destination
when you repeat this step.

The file preserves the reviewed HTML bytes. Some root-relative styles and
protected assets do not load from a `file:` URL in the current alpha.

### 4. Publish the reviewed preview

Approve the exact revision and preview:

```console
scripts/dev-maincopy.sh publish-now "$POST_ID" \
  --preview-digest "$PREVIEW_DIGEST" \
  --revision "$REVISION" \
  --idempotency-key aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa
```

Expected status:

```text
Status: published
```

Do not reuse this idempotency key for a different publication command.

### 5. Verify canonical output, RSS, and the alias

Fetch the canonical page:

```console
curl --noproxy '*' --cacert "$ROOT_CERTIFICATE" --max-time 5 \
  --fail --silent --show-error \
  "$PUBLIC_ORIGIN/posts/hello-maincopy" \
  --output "$PREVIEW_DIRECTORY/published.html"
grep -F '<h1>Hello, Maincopy</h1>' \
  "$PREVIEW_DIRECTORY/published.html"
grep -F 'class="article-code"><code class="language-rust"' \
  "$PREVIEW_DIRECTORY/published.html"
grep -F 'class="mermaid-diagram"' \
  "$PREVIEW_DIRECTORY/published.html"
```

The code-language class records the author-declared language. V1 does not
perform token-level syntax highlighting. These checks and all public
navigation work without JavaScript.

Fetch the RSS feed and verify its canonical post URL:

```console
curl --noproxy '*' --cacert "$ROOT_CERTIFICATE" --max-time 5 \
  --fail --silent --show-error \
  "$PUBLIC_ORIGIN/feed.xml" \
  --output "$PREVIEW_DIRECTORY/feed.xml"
grep -F '<item>' "$PREVIEW_DIRECTORY/feed.xml"
grep -F '<link>https://maincopy.localhost:8443/posts/hello-maincopy</link>' \
  "$PREVIEW_DIRECTORY/feed.xml"
```

The fixture declares `aliases = ["welcome"]` in the post frontmatter.
Verify its redirect:

```console
curl --noproxy '*' --cacert "$ROOT_CERTIFICATE" --max-time 5 \
  --silent --show-error --output /dev/null \
  --write-out '%{http_code} %{redirect_url}\n' \
  "$PUBLIC_ORIGIN/posts/welcome?source=local-alpha"
```

Expected output:

```text
308 https://maincopy.localhost:8443/posts/hello-maincopy
```

If browser trust is installed, open these URLs for the visual demonstration:

- `https://maincopy.localhost:8443/posts/hello-maincopy`
- `https://maincopy.localhost:8443/feed.xml`
- `https://maincopy.localhost:8443/posts/welcome`

### 6. Log out

Revoke the session while the server and gateway are still running:

```console
scripts/dev-maincopy.sh logout
```

A successful command reports `Revoked session` and its identifier. Stop the
launcher with `Ctrl+C` after this command succeeds.

## Preserve or reset publication state

Normal launcher restarts preserve the database and retained content candidates
in `target/maincopy-dev/state/`. They also preserve published visibility.

To repeat the first-publication workflow, first revoke the human session. In
the browser, return to the post list and choose `Sign out`. For a CLI session,
run `scripts/dev-maincopy.sh logout`. Then stop the launcher and move the state
directory aside:

```bash
STATE_ARCHIVE="target/maincopy-dev/state.before-$(date -u +%Y%m%dT%H%M%SZ)"
mv -- target/maincopy-dev/state "$STATE_ARCHIVE"
printf 'Preserved prior state at %s\n' "$STATE_ARCHIVE"
```

The next launcher run creates new state and prints a new owner password once.
This reset does not replace the durable development CA.

> [!WARNING]
> Sign out of the browser and log out of the CLI before resetting state. A
> successful browser login replaces stale cookies, but an operating-system
> credential can retain a CLI session for a database that no longer exists and
> block a later CLI login.

## Development evidence and production boundary

The `development-gateway` flake check validates the Caddy configuration and
its launcher scripts. The gateway binds both virtual hosts to loopback. It
removes untrusted identity headers, disables upstream retries, and blocks
metrics forwarding.

This evidence applies only to the local-alpha harness. The gateway runs with
the developer's identity and uses a workstation CA.

The fixture does not test an SSH server, deploy key, or managed mirror. Use the
[managed source runbook](managed-source.md) for that configuration contract.

[Work package 4.5](implementation.md#work-package-45-https-admin-gateway-contract)
still requires the complete production gateway contract. It includes remote
exposure policy, browser routes, logging evidence, and security tests.

[Work package 8.2](implementation.md#work-package-82-nixos-module-and-admin-gateway)
still requires the NixOS module. It includes separate service identities,
firewall enforcement, protected credentials, and virtual-machine evidence.

## Troubleshooting

### The credential store is unavailable

Run the CLI from the graphical login session that owns Secret Service. Unlock
the default collection, then repeat the command.

### A human session is already stored

Use the stored session or restore its original server state. Run
`scripts/dev-maincopy.sh logout` before a state reset.

If the original state no longer exists, remove only the Maincopy entry through
the operating system credential manager. Confirm the target before removal.

### The generated owner password was not saved

If no human session exists, stop the launcher and move the disposable local
alpha state aside with the reset procedure above. Restart the launcher, then
save the new password before the readiness message appears.

If a session exists, log out before the reset. Maincopy does not redisplay the
initial password, and the current alpha does not provide a completed browser
password-rotation flow.

### The development CA is missing

Use the same `XDG_DATA_HOME` value in both terminals. If the CA does not exist,
restart `scripts/dev-alpha.sh` to create it.

The launcher creates a fresh disposable leaf certificate from the durable CA
on each start. A changed `XDG_DATA_HOME` therefore cannot leave the gateway
serving a leaf from a different development CA.

### The launcher does not become ready

Read the server and Caddy diagnostics in the launcher terminal. Identify any
process that owns ports `3000`, `3001`, or `8443`.

Stop that process only when you own it and no longer need it. Then restart the
launcher.

### Another development gateway owns the lock

Only one gateway can use the durable development CA at a time. Stop the earlier
gateway normally and retry. Do not remove `gateway.lock`; the operating system
releases its lock when the owning process exits.
