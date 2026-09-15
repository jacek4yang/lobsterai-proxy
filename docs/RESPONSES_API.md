# OpenAI Responses API (Grok Build)

`POST /v1/responses` speaks the Responses wire schema consumed by Grok Build
(`api_backend = "responses"`). The request is normalized into the same
OpenAI Chat Completions body the Anthropic frontend produces, so all shared
policy (reasoning epochs, prefix stability, canonical tool arguments,
requested-only thinking) applies unchanged.

## Streaming event lifecycle

```text
response.created
response.in_progress
response.output_item.added        (reasoning, when requested)
response.reasoning_summary_part.added
response.reasoning_summary_text.delta ...
response.reasoning_summary_text.done
response.reasoning_summary_part.done
response.output_item.added        (message)
response.content_part.added
response.output_text.delta ...
response.output_text.done
response.content_part.done
response.output_item.done
response.output_item.added        (function_call)   <- ALWAYS before args
response.function_call_arguments.delta ...
response.function_call_arguments.done
response.output_item.done
response.completed | response.incomplete | response.failed
```

Grok Build maps `output_index` -> its internal tool index ONLY when the
function-call item arrives in `response.output_item.added`; argument deltas
for an unmapped index are silently dropped. This proxy therefore never
emits argument deltas before the item-added frame, and never emits an
item-added without `call_id`/`name`.

## Request mapping

- `input` string -> one user message; array items convert in order
  (replayed conversation state — never reordered).
- `function_call` history -> assistant `tool_calls` with the SAME
  `call_id` (never regenerated); consecutive calls of one assistant turn
  are merged into one message; `function_call_output` -> `role=tool` with
  `tool_call_id = call_id`.
- `reasoning.effort` low/medium/high/xhigh/max -> `reasoning_effort`
  (none/minimal -> low); `reasoning.summary` present -> reasoning may be
  exposed as a Responses reasoning item (requested-only).
- `max_output_tokens` -> `max_tokens` (never dropped).
- `prompt_cache_key` -> forwarded verbatim to the upstream passthrough and
  fingerprinted (server secret, domain-separated) for sticky account
  routing; the raw key is never logged.
- Flat Responses function tools -> nested OpenAI function tools; `parameters`
  schemas byte-identical.

## Hosted tools

Backend-hosted tool declarations (`web_search`, `x_search`,
`code_interpreter`, MCP, …) are dropped, not forwarded: this proxy cannot
execute them and never fabricates results. A replayed `web_search_call`
input item is rejected explicitly (this proxy never mints one). Grok Build
sends a default hosted `web_search` entry routinely, so the drop keeps
sessions working with client function tools.

## Errors

Failures use the OpenAI error envelope (`{"error": {message, type, code}}`)
— not the Anthropic shape — so Grok Build's error parser renders them.
