# Architecture

One logical request flows through four stages; every stage is deterministic
and side-effect free except for the marked persistence points.

```
Claude Code / Grok Build
   | POST /v1/messages (Anthropic)        POST /v1/responses (Responses API)
   v
[1] server/orchestrator.rs    auth -> parse -> convert -> policy -> generate
   |  +- anthropic/request.rs    Anthropic -> OpenAI body
   |  +- responses/request.rs    Responses -> OpenAI body
   |                          +- deepseek/policy.rs      epochs, canonical args
   |                          +- reasoning_shadow.rs     restore in-epoch reasoning
   |                          +- lobsterai/upstream.rs   retry invariants
   | OpenAI Chat Completions POST {base}/api/proxy/v1/chat/completions (stream=true)
   v
[2] LobsterAI backend      deepseek-v4.1-flash
   | OpenAI SSE
   v
[3] server StreamPump      watchdog + pings -> protocol renderer
   | Anthropic SSE            +- anthropic/stream.rs: strict block lifecycles
   | Responses SSE            +- responses/stream.rs: Responses event lifecycle
   |                          +- accumulation (usage, tool args, reasoning)
   |                          +- shadow store/clear on finish
   v
[4] Claude Code / Grok Build
```

## Modules

| Module | Responsibility |
| --- | --- |
| `server/orchestrator.rs` | Routes, orchestration, StreamPump (watchdog, pings), non-stream aggregation |
| `server/responses_pump.rs` | Responses streaming pump + non-stream aggregation |
| `anthropic/request.rs` | Anthropic -> OpenAI conversion + strict validation |
| `anthropic/stream.rs` | OpenAI SSE chunk -> Anthropic SSE events + accumulation |
| `anthropic/response.rs` | Non-stream Message assembly |
| `anthropic/types.rs` | Stop-reason/usage mapping, SSE formatting |
| `responses/request.rs` | Responses -> OpenAI conversion (Grok Build compat target) |
| `responses/stream.rs` | OpenAI SSE chunk -> Responses SSE events + accumulation |
| `responses/types.rs` | Responses usage mapping, SSE framing, opaque ids |
| `deepseek/policy.rs` | Reasoning epochs, canonical tool args, prefix hash, token estimation |
| `deepseek/reasoning.rs` | `requested_only` thinking exposure |
| `lobsterai/credential.rs` | `lobsterai-*.json` parsing (nested/flat), secrecy, atomic persistence, JWT `exp` fallback |
| `lobsterai/refresh.rs` | Single-flight refresh (double-check under lock) |
| `lobsterai/pool.rs` | Discovery/seeding, credits-first pick, sticky sessions, cooldowns |
| `lobsterai/headers.rs` | Static/credential header construction (never client-controlled) |
| `lobsterai/checkin.rs` | Daily check-in (slot -> context -> check_in), credit refresh |
| `lobsterai/housekeep.rs` | Background loops: refresh-due, check-in, credits |
| `lobsterai/oauth.rs` | Browser login: local callback server, code exchange, credential save |
| `lobsterai/upstream.rs` | Shared reqwest client, generation call, retry invariants |
| `reasoning_shadow.rs` | Bounded memory-only reasoning continuity store |
| `stream_watch.rs` | first-event / first-semantic / byte-idle / semantic-idle watchdog |
| `session.rs` | Session extraction, domain-separated HMAC fingerprints, conversation ids |
| `redaction.rs` | Unified sanitization for logs/errors |
| `observability.rs` | Global counters, one compact summary per request |
| `config.rs` / `cli.rs` | TOML config, env overrides, clap CLI |

## Invariants (enforced in code and tested)

1. **One logical generation**: at most one successful generation. 401 ->
   single-flight refresh + one retry on the SAME account; 429 /
   out-of-credits -> cooldown + failover; 403/5xx/transport -> stop. After
   the first semantic byte: never replay.
2. **Requested-only thinking**: reasoning reaches the client only when the
   client asked for thinking; otherwise it lives in the shadow store only.
3. **Epoch hygiene**: historical `reasoning_content` is stripped; text,
   `tool_calls`, ids, and results are never modified.
4. **Lossless conversion**: unsupported blocks/types reject with a clear
   error; nothing is silently dropped. Tool schemas pass through verbatim.
5. **Secret hygiene**: tokens live in `SecretString`; every log/error path
   goes through `redaction`; raw uids never appear in logs or admin output
   (HMAC `acct-*` fingerprints only).
6. **Bounded memory**: sticky table, shadow store, SSE line buffer all have
   explicit caps.

## Persistence (all inside the auth directory)

- `lobsterai-*.json` — credential files (atomic temp+rename writes)
- No other state files: credits live in memory, learned from the upstream
  `profile-summary` endpoint on every check-in/credit pass.
