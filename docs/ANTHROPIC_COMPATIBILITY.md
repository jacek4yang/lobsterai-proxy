# Anthropic Messages API (Claude Code and Pi)

`POST /v1/messages` speaks the Anthropic Messages wire protocol for Claude
Code and Pi Coding Agent (streaming and non-streaming).
`POST /v1/messages/count_tokens`
returns a deliberately conservative estimate (header
`x-lobsterai-proxy-token-count: estimated-deepseek`); it is never presented
as exact.

## Session identity

The frontend resolves one opaque session hint in this order:

1. `metadata.user_id`;
2. `metadata.session_id`;
3. `x-session-affinity`;
4. `x-session-id`.

Body metadata keeps the existing Claude Code precedence. Pi 0.87.0 uses
`x-session-affinity` when its Anthropic compatibility setting enables session
affinity, or `x-session-id` for its OpenRouter affinity format. For a custom
Anthropic provider without that setting, Pi 0.87.0 sends no session identity;
the proxy does not invent one from the user agent, connection, or source IP.

Accepted values are non-empty and at most 512 bytes. The raw value is never
logged or forwarded to LobsterAI. It is immediately converted to the existing
secret-keyed Anthropic session fingerprint used by account stickiness,
conversation IDs, and the reasoning shadow. Responses API identities remain
in a separate fingerprint domain.

## Conversion rules

- `tool_use` -> `tool_calls` and `tool_result` -> `role=tool` with the SAME
  ids; a user message mixing tool results and text emits the tool messages
  first (OpenAI requires them adjacent to the assistant tool_calls) and the
  remaining content as the following user message.
- Tool schemas pass through byte-identical — never truncated or edited.
- Unknown or unsupported block types are rejected with a clear
  `invalid_request_error` — nothing is silently dropped.
- Lossless normalizations only: single text block -> string, empty text
  blocks dropped, Anthropic-only metadata (`cache_control`, `metadata`,
  thinking budget) not forwarded.
- Server-tool declarations (`web_search_20250305` etc.) are rejected: this
  proxy executes no server-side tools.

## Streaming

OpenAI stream -> Anthropic events (`message_start`, `content_block_*`,
`message_delta`, `message_stop`, periodic pings). Strict block lifecycles:
thinking deltas open a thinking block closed when text or tool content
begins; parallel tool calls each get their own block with
`input_json_delta` fragments. `stream=false` uses ONE upstream stream
aggregated locally — never a second generation.

## Reasoning (`thinking`)

- When the client requests thinking (`thinking: {type: "enabled"}`),
  upstream `reasoning_content` streams as `thinking` blocks.
- Otherwise reasoning is consumed in-turn by a bounded, memory-only
  reasoning shadow so the tool loop keeps its continuity without polluting
  Claude Code history.
- Reasoning epochs: a new human turn starts a new epoch; historical
  reasoning is stripped, the current tool loop's chain is preserved. Text,
  tool calls, ids, and results are never touched.

## Prefix stability

- The leading `x-anthropic-billing-header:` line Claude Code prepends is
  stripped so the system prefix is byte-stable across turns.
- Historical tool-call arguments are canonicalized (sorted-key
  re-serialization); the current epoch's arguments are untouched.
- A deterministic prefix hash is logged (never content) to make drift
  observable.

## Retry invariants

- 401 -> single-flight refresh of the same account, retry once.
- 429 / out-of-credits -> cooldown + failover to another account.
- 403 / 5xx / transport errors -> stop immediately.
- After any semantic output: never replay.
