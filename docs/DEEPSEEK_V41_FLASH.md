# DEEPSEEK_V41_FLASH — the single served model

This proxy is optimized end-to-end for `deepseek-v4.1-flash` on the
LobsterAI backend. Other client-requested model ids (Claude/Grok names)
map to it; no other upstream model is addressed.

## Wire semantics

- Chat endpoint: `POST {base}/api/proxy/v1/chat/completions`, OpenAI shape,
  **stream-only** — the proxy always sends `stream: true` and aggregates
  locally for non-stream clients.
- Reasoning arrives as `delta.reasoning_content` before text/tool deltas of
  the same turn; `usage.completion_tokens_details.reasoning_tokens` may
  report reasoning output tokens.
- Tool calls follow the OpenAI shape (parallel-capable, fragmented JSON
  argument deltas).

## Policy applied to every request

1. **Reasoning epochs** — a new human user message starts a new epoch;
   `reasoning_content` from earlier epochs is removed (text, tool_calls,
   ids, and results are never touched).
2. **Canonical historical tool arguments** — only
   `assistant.tool_calls[].function.arguments` of earlier epochs are
   re-serialized with sorted keys; user content and tool results are never
   rewritten.
3. **Requested-only thinking** — reasoning surfaces to the client only when
   the client asked; otherwise it lives in the reasoning shadow.
4. **Prefix stability** — deterministic serialization + a stable prefix
   hash keeps drift observable.
5. **Body filtering** — only known-safe passthrough keys are forwarded;
   `stream_options.include_usage` is set so usage is always reported.

## Known limits

- `count_tokens` is a conservative estimate, never exact.
- Image/document inputs are forwarded as OpenAI multimodal parts; if the
  upstream rejects them the error surfaces unchanged (nothing is dropped).
- `redacted_thinking` history blocks are opaque and skipped.
