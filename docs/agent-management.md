# Agent grants

Use the **Agents** screen or `maincopy agents` to manage delegated administration access.
Each grant binds a Nostr public key to an owner account and selected scopes.
The owner account's current roles limit its effective scopes.

## Register a grant

1. Sign in with an account that has `credential_manage` authority.
2. Run `maincopy agent-key inspect` with the target administration origin.
3. Open **Agents** and select **Register an agent**.
4. Copy the inspected public key into the form.
5. Set the owner account UUID, label, and requested scopes.
6. Set an optional future expiry in UTC.
7. Submit the form, then inspect the saved grant and compare its fingerprint.

The local inspection reads the selected origin's protected agent credential.
It does not confirm registration or authorization on the server.
The server stores public metadata for the grant; the protected private key stays local.

To register from the CLI, replace the example values:

```console
maincopy --admin-origin https://admin.example.test login --username first-owner
maincopy --admin-origin https://admin.example.test agent-key inspect
maincopy --admin-origin https://admin.example.test agents register \
  --owner-user-id OWNER_UUID \
  --public-key PUBLIC_KEY_HEX \
  --label publishing-helper \
  --scopes content_read,release_manage \
  --expires-at 2026-12-31T23:59:59Z
```

Registration requires recent authentication and authority over the owner account.
Requested scopes must be a nonempty, unique subset of the authenticated authority.
Use the returned grant UUID to inspect the result:

```console
maincopy --admin-origin https://admin.example.test agents list
maincopy --admin-origin https://admin.example.test agents inspect GRANT_UUID
```

Lists return at most 100 grants per request.
Use the returned `next_cursor` with `agents list --cursor UUID` to continue.
Add `--json` for machine-readable metadata and operation receipts.

## Replace scopes or revoke access

Inspect the grant before each change.
Use its current version as the mutation precondition.
Scope replacement replaces the complete requested scope set.

```console
maincopy --admin-origin https://admin.example.test agents scopes GRANT_UUID \
  --expected-version 1 --scopes content_read
maincopy --admin-origin https://admin.example.test agents inspect GRANT_UUID
maincopy --admin-origin https://admin.example.test agents revoke GRANT_UUID \
  --expected-version 2
```

The browser provides the same actions on the grant detail page.
It displays requested scopes, effective scopes, ownership, expiry, last use, and revocation.
Expired and revoked grants cannot authenticate.
Disabling an owner account also revokes its agent grants; enabling the account does not restore them.

## Recover a failed change

If the server reports a stale version, inspect the current grant and review the intended change again.
If authentication is no longer fresh, sign in again before starting a new operation.

The CLI reports an idempotency key after an uncertain mutation result.
Inspect the current state before retrying.
Reuse that key only with the identical command and original authorizing session.
Supply it with `--idempotency-key UUID`.
A new session cannot replay a mutation authorized by the previous session.
