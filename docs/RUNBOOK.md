# Deployment runbook (Windows portable, mirrors codebuddy-proxy-bin)

## Layout

```
D:\Workspace\lobsterai-proxy-bin\
├── lobsterai-proxy.exe        # release build (copied, not in git)
├── config.toml                # production config
├── auth\                      # lobsterai-<uid>.json credentials (login output)
└── CLAUDE.md                  # local usage notes (optional)
```

## Deploy

```sh
cargo build --release
Copy-Item target\release\lobsterai-proxy.exe D:\Workspace\lobsterai-proxy-bin\
```

For an upgrade, keep the previous binary as
`lobsterai-proxy.exe.bak-<yyyyMMdd-HHmmss>`.

## First run

```sh
cd D:\Workspace\lobsterai-proxy-bin
.\lobsterai-proxy.exe login     # repeat per account
.\lobsterai-proxy.exe status    # verify the pool
.\lobsterai-proxy.exe serve     # listens on 127.0.0.1:8090 by default
```

## Client wiring

Claude Code:

```sh
set ANTHROPIC_BASE_URL=http://127.0.0.1:8090
set ANTHROPIC_API_KEY=sk-anything   # only if api_key is configured
claude
```

Grok Build (`api_backend = "responses"`): point the Responses base URL at
`http://127.0.0.1:8090` (Bearer key as configured).

## Health

- `GET /healthz` — liveness (`ok`).
- `GET /readyz` — 200 once at least one account is loaded.
- `GET /admin/status` — per-account health/credits/cooldowns (safe names).
- `GET /metrics` — Prometheus counters/gauges (`lobsterai_proxy_*`).

## Incident notes

- **401 storm**: tokens expired faster than the keepalive; check the auth
  dir is writable and the machine was not suspended past `expiresAt`. The
  proxy refreshes and retries once per request automatically.
- **All accounts cooling (429)**: per-(account, model) cooldowns, 10 min by
  default; wait for the reset or add accounts.
- **Account out of credits**: 12 h cooldown; the next daily check-in
  (+100) revives it — accounts self-heal on the schedule.
- **Stale tokens during check-in**: the loop refreshes once and retries
  within the same pass; persistent failures mean the refresh token itself
  is dead — re-login that account.
