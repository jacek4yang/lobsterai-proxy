# lobsterai-proxy

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A local, single-binary Rust proxy that exposes the **Anthropic Messages API**
(for [Claude Code](https://claude.com/claude-code)) and the **OpenAI Responses
API** (for [Grok Build](https://github.com/xai-org/grok-build)) and translates
both to the **NetEase Youdao LobsterAI** backend, proxying **exclusively
`deepseek-flash`**.

```
Claude Code                      Grok Build
   │ Anthropic Messages API         │ OpenAI Responses API
   │ (SSE)                          │ (SSE)
   ▼                                ▼
lobsterai-proxy  (this project, localhost)
   │ OpenAI Chat Completions (SSE, multi-account pool)
   ▼
LobsterAI backend  →  deepseek-flash
```

## Features

- **Multi-account pool** — automatic polling across all logged-in accounts:
  highest-credits-first selection, request-level failover rotation,
  429 / out-of-credits cooldowns, single-flight token refresh with JWT `exp`
  fallback, persisted pool state.
- **Automatic daily check-in** — implements the real client-activities
  check-in flow (slot → context → check_in) discovered from the desktop
  client, +100 credits per account per day, with retries across the day and
  a persisted ledger.
- **Anthropic Messages API** — `POST /v1/messages`, `POST /v1/messages/count_tokens`;
  full Claude Code tool semantics (`tool_use` ↔ `tool_calls`, parallel calls,
  fragmented streaming JSON arguments), SSE event lifecycle, non-stream
  aggregation from ONE upstream stream.
- **OpenAI Responses API** — `POST /v1/responses` for Grok Build: full
  streaming event lifecycle, tool-call round-tripping with stable call ids,
  reasoning, usage.
- **Single model by design** — everything is tuned for `deepseek-flash`;
  other client-requested model ids map to it.
- **Hardening** — tokens live in `SecretString`, unified log/error redaction,
  client headers never forwarded upstream, local API key enforced for
  non-loopback binds, bounded memory everywhere.

## Quick start

```sh
# 1. Build
cargo build --release      # target/release/lobsterai-proxy(.exe)

# 2. Log in (browser opens; credential saved to auth/lobsterai-<uid>.json)
lobsterai-proxy login      # repeat for each additional account

# 3. Serve
lobsterai-proxy serve      # reads ./config.toml if present

# 4. Point Claude Code at it
export ANTHROPIC_BASE_URL=http://127.0.0.1:8090
export ANTHROPIC_API_KEY=sk-anything       # only needed if api_key configured
claude
```

Copy `config.example.toml` to `config.toml` and edit as needed.
Environment overrides: `LOBSTERAI_AUTH_DIR`, `LOBSTERAI_PROXY_API_KEY`,
`LOBSTERAI_UPSTREAM_BASE`, `LOBSTERAI_LOGIN_PORTAL`.

## Endpoints

| Route | Auth | Description |
| --- | --- | --- |
| `POST /v1/messages` | api_key (if set) | Anthropic Messages (stream + non-stream) |
| `POST /v1/responses` | api_key (if set) | OpenAI Responses API for Grok Build |
| `POST /v1/messages/count_tokens` | api_key (if set) | Conservative estimate |
| `GET /v1/models` | api_key (if set) | Lists `deepseek-flash` |
| `GET /healthz` | public | `ok` |
| `GET /readyz` | public | 200 when accounts loaded, else 503 |
| `GET /admin/status` | api_key | Version, uptime, account health, counters (no secrets) |

## Security — credentials are local-only

**Never commit or share:**

- `auth/` directory and any `lobsterai-*.json` credential files
- `config.toml` / `.env` (local configuration)
- LobsterAI access tokens or refresh tokens

LobsterAI credentials provide full account access and must be treated as
secrets. They are git-ignored by default and never required for building or
running tests (tests use a local mock backend).

## Build from source

```sh
cargo build --release
cargo test --all           # unit + mock-backend integration tests (no network)
```

Pure Rust, no OpenSSL (rustls); Windows and Linux first-class.

## License

MIT
