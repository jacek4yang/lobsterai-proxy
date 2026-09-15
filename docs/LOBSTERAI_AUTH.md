# LobsterAI credentials and login

## Credential file

One file per account in the auth directory (`auth/` next to the executable
by default; `LOBSTERAI_AUTH_DIR` or `[auth] dir` overrides):

`auth/lobsterai-<uid>.json` — nested form (also what the login flow writes):

```json
{
  "auth": {
    "accessToken": "<JWT, HS512, ~30 days>",
    "refreshToken": "<opaque>",
    "expiresAt": 1900000000,
    "uuid": "<install uuid>",
    "firstKeyfrom": "<first-login epoch ms>",
    "latestKeyfrom": "<latest-activity epoch ms>"
  },
  "account": {"uid": "...", "userId": "...", "nickname": "..."}
}
```

A flat single-object form (`{"accessToken": ..., "uid": ...}`) is accepted
on read and normalized to the nested form on the first refresh.

## Login

```sh
lobsterai-proxy login             # opens the browser
lobsterai-proxy login --no-browser  # prints the URL only
```

The command binds a random local port on 127.0.0.1, opens

```
{portal}/portal#/login?source=electron&redirect_uri=http://127.0.0.1:{port}/auth/callback&state={state}
```

and waits for the browser redirect (10 minutes). After the callback the
code is exchanged at `POST /api/auth/exchange` and the credential is saved.
Repeat for each additional account — the pool loads every
`lobsterai-*.json` in the directory and picks per request.

## Refresh

- Proactive: the housekeeping loop refreshes tokens within
  `refresh_margin_secs` (default 10 min) of expiry, plus a daily keepalive
  refresh for idle accounts (`keepalive_secs`).
- Reactive: on an upstream 401 the proxy refreshes the SAME account once
  (single-flight; concurrent failures share one refresh call) and retries
  the request exactly once. A second 401 surfaces an authentication error.
- `expiresAt` missing? The JWT `exp` claim is used as a fallback.

## Seeding

`[auth] seed_dirs` lists read-only directories whose `lobsterai-*.json`
files are copied into the managed dir at startup (dedup by uid; never
written back). Useful to share one login across several deployments.

## Security

- Tokens are held in `SecretString`; logs and admin output go through
  redaction and show only HMAC-derived `acct-*` safe names.
- Writes are atomic (temp file + rename in the same directory); a crash can
  never truncate a credential file. Stale temp files are swept at startup.
- Never commit `auth/` or share credential files: they grant full account
  access.
