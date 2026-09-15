//! deepseek-flash request policy, applied to the converted OpenAI body:
//!
//! 1. **Reasoning epochs** — a new *human* user message starts a new epoch;
//!    assistant tool calls and their results continue the current one.
//!    `reasoning_content` from epochs before the newest human turn is
//!    removed (text, tool_calls, ids, and results are never touched), so
//!    history never accumulates unbounded reasoning while the current tool
//!    loop keeps its reasoning chain intact.
//! 2. **Canonical historical tool arguments** — only
//!    `assistant.tool_calls[].function.arguments` strings are re-serialized
//!    with deterministic key order. User content, source code, shell output,
//!    and tool results are never parsed or rewritten.
//! 3. **Prefix stability accounting** — a stable prefix hash over the
//!    normalized system + messages + tools makes drift observable without
//!    logging content.
//! 4. **Conservative token estimation** for `count_tokens` (never presented
//!    as exact).

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// The billing attribution header Claude Code prepends to system text. Only a
/// *leading* line of exactly this shape is removed — never a full-text search,
/// never occurrences the user authored later in the text.
pub const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

/// Length of the leading billing-header line (including its terminator).
pub fn strip_leading_billing_header(text: &str) -> usize {
    if !text.starts_with(BILLING_HEADER_PREFIX) {
        return 0;
    }
    let rest = &text[BILLING_HEADER_PREFIX.len()..];
    let line_end = rest
        .find(['\n', '\r'])
        .map(|position| BILLING_HEADER_PREFIX.len() + position)
        .unwrap_or(text.len());
    let mut cut = line_end;
    if text[cut..].starts_with("\r\n") {
        cut += 2;
    } else if text[cut..].starts_with(['\r', '\n']) {
        cut += 1;
    }
    cut
}

/// Apply the leading billing-header strip to an Anthropic-shaped request's
/// `system` (string or text-block array). Shared by the wire path and
/// `count_tokens` so both see identical normalization.
pub fn strip_billing_in_anthropic_system(request: &mut Value) {
    let Some(system) = request.get_mut("system") else {
        return;
    };
    match system {
        Value::String(text) => {
            let cut = strip_leading_billing_header(text);
            if cut > 0 {
                *text = text[cut..].to_owned();
            }
        }
        Value::Array(blocks) => {
            for block in blocks.iter_mut() {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(Value::String(text)) = block.get_mut("text") {
                        let cut = strip_leading_billing_header(text);
                        if cut > 0 {
                            *text = text[cut..].to_owned();
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Canonical JSON: object keys sorted at every level, array order preserved,
/// strings/numbers emitted exactly as serde renders them.
pub fn canonical_json_string(value: &Value) -> String {
    let mut output = String::new();
    write_canonical(value, &mut output);
    output
}

fn write_canonical(value: &Value, output: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            output.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).unwrap_or_default());
                output.push(':');
                write_canonical(&map[*key], output);
            }
            output.push('}');
        }
        Value::Array(items) => {
            output.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical(item, output);
            }
            output.push(']');
        }
        Value::String(text) => {
            if let Ok(rendered) = serde_json::to_string(text) {
                output.push_str(&rendered);
            }
        }
        other => {
            if let Ok(rendered) = serde_json::to_string(other) {
                output.push_str(&rendered);
            }
        }
    }
}

/// Canonicalize historical `tool_calls[].function.arguments` in an OpenAI body
/// (in place). The current epoch — everything from the last user/tool message
/// onward — is left untouched. Malformed argument strings are forwarded
/// as-is. Returns the number of arguments rewritten.
pub fn canonicalize_tool_arguments(body: &mut Map<String, Value>) -> usize {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    let Some(last_action) = messages.iter().rposition(|message| {
        matches!(
            message.get("role").and_then(Value::as_str),
            Some("user") | Some("tool")
        )
    }) else {
        return 0;
    };
    let mut count = 0usize;
    for (index, message) in messages.iter_mut().enumerate() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") || index >= last_action
        {
            continue;
        }
        let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) else {
            continue;
        };
        for call in calls.iter_mut() {
            let Some(function) = call.get_mut("function").and_then(Value::as_object_mut) else {
                continue;
            };
            let Some(arguments) = function.get("arguments").and_then(Value::as_str) else {
                continue;
            };
            if let Ok(parsed) = serde_json::from_str::<Value>(arguments) {
                let canonical = canonical_json_string(&parsed);
                if canonical != arguments {
                    function.insert("arguments".into(), Value::String(canonical));
                    count += 1;
                }
            }
        }
    }
    count
}

/// Index of the newest human user message in an OpenAI body (the reasoning
/// epoch boundary): a `user` message carrying content other than tool_result
/// data. In the converted OpenAI body, tool results are `role=tool` messages,
/// so any `user` message is human. Returns `messages.len()` when none exists.
pub fn reasoning_epoch_boundary(messages: &[Value]) -> usize {
    messages
        .iter()
        .rposition(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .unwrap_or(messages.len())
}

/// Remove `reasoning_content` from assistant messages before the epoch
/// boundary. Returns removed bytes. Never touches text/tool_calls/ids.
pub fn strip_historical_reasoning(body: &mut Map<String, Value>) -> u64 {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    let boundary = reasoning_epoch_boundary(messages);
    let mut removed = 0u64;
    for (index, message) in messages.iter_mut().enumerate() {
        if index >= boundary {
            break;
        }
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let object = match message.as_object_mut() {
            Some(object) => object,
            None => continue,
        };
        if let Some(removed_value) = object.remove("reasoning_content") {
            removed += serde_json::to_vec(&removed_value)
                .map(|b| b.len() as u64)
                .unwrap_or(0);
        }
    }
    removed
}

/// Strip historical `thinking` blocks from an *Anthropic* request (used by
/// `count_tokens` so its estimate matches the optimized wire request).
/// The epoch boundary is the newest human user message: a `user` message
/// with any content beyond tool_result blocks.
pub fn strip_anthropic_thinking(request: &mut Value) -> usize {
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    let boundary = messages
        .iter()
        .rposition(|message| {
            if message.get("role").and_then(Value::as_str) != Some("user") {
                return false;
            }
            match message.get("content") {
                Some(Value::String(text)) => !text.is_empty(),
                Some(Value::Array(blocks)) => blocks
                    .iter()
                    .any(|block| block.get("type").and_then(Value::as_str) != Some("tool_result")),
                _ => false,
            }
        })
        .unwrap_or(messages.len());
    let mut removed = 0usize;
    for message in messages.iter_mut().take(boundary) {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let before = blocks.len();
        blocks.retain(|block| block.get("type").and_then(Value::as_str) != Some("thinking"));
        removed += before.saturating_sub(blocks.len());
    }
    removed
}

/// SHA-256 prefix hash over the request sections that must stay byte-stable
/// across consecutive turns (system, messages, tools of the OpenAI body).
/// A local stability metric only — equal hashes mean locally identical
/// prefixes, never a proven upstream cache hit.
pub fn stable_prefix_hash(body: &Map<String, Value>) -> (String, usize) {
    let mut hasher = Sha256::new();
    let mut prefix_bytes = 0usize;
    for section in ["system", "messages", "tools"] {
        if let Some(value) = body.get(section) {
            if let Ok(bytes) = serde_json::to_vec(value) {
                hasher.update(section.as_bytes());
                hasher.update([0]);
                hasher.update((bytes.len() as u64).to_le_bytes());
                hasher.update(&bytes);
                prefix_bytes += bytes.len();
            }
        }
    }
    (
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        prefix_bytes,
    )
}

// ---------------------------------------------------------------------------
// Conservative token estimation (never presented as exact)
// ---------------------------------------------------------------------------

/// Tokens for one string: ASCII bytes at ~4 bytes/token, non-ASCII chars at
/// 2 tokens each. Deliberately biased high (a low estimate breaks Claude
/// Code's budgeting; a high one only wastes headroom).
pub fn estimate_string_tokens(text: &str) -> u64 {
    let mut ascii_bytes = 0u64;
    let mut non_ascii = 0u64;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_bytes += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii_bytes.div_ceil(4) + non_ascii.saturating_mul(2)
}

/// Estimated token count for a full Anthropic request, after the same
/// normalizations the wire path applies (billing-header strip, historical
/// thinking removal). Includes per-message and per-tool overheads.
pub fn estimate_anthropic_tokens(request: &Value) -> u64 {
    let mut total: u64 = 16; // base request overhead
    if let Some(system) = request.get("system") {
        total += match system {
            Value::String(text) => estimate_string_tokens(text),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .map(estimate_string_tokens)
                .sum(),
            _ => 0,
        };
    }
    if let Some(messages) = request.get("messages").and_then(Value::as_array) {
        for message in messages {
            total += 8; // per-message overhead (role, separators)
            total += estimate_message_tokens(message);
        }
    }
    if let Some(tools) = request.get("tools").and_then(Value::as_array) {
        for tool in tools {
            total += 8;
            if let Some(name) = tool.get("name").and_then(Value::as_str) {
                total += estimate_string_tokens(name);
            }
            if let Some(description) = tool.get("description").and_then(Value::as_str) {
                total += estimate_string_tokens(description);
            }
            if let Some(schema) = tool.get("input_schema") {
                total += estimate_string_tokens(&canonical_json_string(schema)) / 2;
            }
        }
    }
    if let Some(budget) = request
        .pointer("/thinking/budget_tokens")
        .and_then(Value::as_u64)
    {
        total += budget / 4;
    }
    total
}

fn estimate_message_tokens(message: &Value) -> u64 {
    match message.get("content") {
        Some(Value::String(text)) => estimate_string_tokens(text),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| {
                let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "text" => block
                        .get("text")
                        .and_then(Value::as_str)
                        .map(estimate_string_tokens)
                        .unwrap_or(0),
                    "thinking" | "redacted_thinking" => {
                        block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .map(estimate_string_tokens)
                            .unwrap_or(0)
                            + 8
                    }
                    "tool_use" => {
                        8 + block
                            .get("name")
                            .and_then(Value::as_str)
                            .map(estimate_string_tokens)
                            .unwrap_or(0)
                            + block
                                .get("input")
                                .map(|input| estimate_string_tokens(&canonical_json_string(input)))
                                .unwrap_or(0)
                    }
                    "tool_result" => {
                        8 + match block.get("content") {
                            Some(Value::String(text)) => estimate_string_tokens(text),
                            Some(Value::Array(parts)) => parts
                                .iter()
                                .filter_map(|p| p.get("text").and_then(Value::as_str))
                                .map(estimate_string_tokens)
                                .sum(),
                            _ => 0,
                        }
                    }
                    "image" | "document" => 1024, // base64 media: rough conservative floor
                    _ => 16,
                }
            })
            .sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- billing header strip ---

    #[test]
    fn billing_header_variants_strip_leading_line_only() {
        let header = "x-anthropic-billing-header: {\"cch\":\"AAA\"}";
        for (text, expected_removed, expected_remaining) in [
            (
                format!("{header}\nYou are Claude Code."),
                header.len() + 1,
                "You are Claude Code.".to_owned(),
            ),
            (
                format!("{header}\r\nYou are Claude Code."),
                header.len() + 2,
                "You are Claude Code.".to_owned(),
            ),
            (
                format!("{header}\rYou are Claude Code."),
                header.len() + 1,
                "You are Claude Code.".to_owned(),
            ),
            (header.to_owned(), header.len(), String::new()),
            (
                format!("{header}\n\nSystem body."),
                header.len() + 1,
                "\nSystem body.".to_owned(),
            ),
        ] {
            let removed = strip_leading_billing_header(&text);
            assert_eq!(removed, expected_removed, "input {text:?}");
            assert_eq!(&text[removed..], expected_remaining);
        }
    }

    #[test]
    fn billing_header_not_at_start_is_never_removed() {
        assert_eq!(
            strip_leading_billing_header("System.\nx-anthropic-billing-header: x"),
            0
        );
        assert_eq!(
            strip_leading_billing_header("\nx-anthropic-billing-header: x\nbody"),
            0
        );
        assert_eq!(
            strip_leading_billing_header("x-anthropic-billing: x\nbody"),
            0
        );
        assert_eq!(strip_leading_billing_header(""), 0);
    }

    #[test]
    fn anthropic_system_strip_matches_wire_normalization() {
        let mut request = json!({
            "system": "x-anthropic-billing-header: {\"cch\":\"AAA\"}\nSystem body.",
            "messages": []
        });
        strip_billing_in_anthropic_system(&mut request);
        assert_eq!(request["system"], "System body.");
        let mut request = json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: x\r\nFirst."},
                {"type": "text", "text": "x-anthropic-billing-header mentioned later stays."}
            ],
            "messages": []
        });
        strip_billing_in_anthropic_system(&mut request);
        assert_eq!(request["system"][0]["text"], "First.");
        assert_eq!(
            request["system"][1]["text"],
            "x-anthropic-billing-header mentioned later stays."
        );
    }

    // --- canonical JSON ---

    #[test]
    fn canonical_json_sorts_keys_recursively_preserves_arrays() {
        let value = json!({
            "b": 2,
            "a": 1,
            "nested": {"z": true, "a": [4, {"d": 4, "c": 3}, "x"]},
            "s": "text, untouched",
            "n": 1.5,
            "nil": null
        });
        assert_eq!(
            canonical_json_string(&value),
            "{\"a\":1,\"b\":2,\"n\":1.5,\"nested\":{\"a\":[4,{\"c\":3,\"d\":4},\"x\"],\"z\":true},\"nil\":null,\"s\":\"text, untouched\"}"
        );
    }

    #[test]
    fn canonicalize_rewrites_only_historical_calls() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {
                        "name": "Edit", "arguments": "{\"path\":\"a\",\"line\":1}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "ok"},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "call_2", "type": "function", "function": {
                        "name": "Read", "arguments": "{\"line\":2,\"path\":\"b\"}"}}
                ]}
            ]
        });
        let count = canonicalize_tool_arguments(body.as_object_mut().unwrap());
        assert_eq!(count, 1);
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{\"line\":1,\"path\":\"a\"}"
        );
        assert_eq!(
            body["messages"][3]["tool_calls"][0]["function"]["arguments"],
            "{\"line\":2,\"path\":\"b\"}"
        );
    }

    #[test]
    fn canonicalize_skips_malformed_and_keeps_results_verbatim() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "call_x", "type": "function", "function": {
                        "name": "Bash", "arguments": "not json at all"}}
                ]},
                {"role": "tool", "tool_call_id": "call_x",
                 "content": "output\nwith    spacing\n\tand tabs"}
            ]
        });
        assert_eq!(
            canonicalize_tool_arguments(body.as_object_mut().unwrap()),
            0
        );
        assert_eq!(
            body["messages"][2]["content"],
            "output\nwith    spacing\n\tand tabs"
        );
    }

    #[test]
    fn canonicalize_is_idempotent() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "c", "type": "function", "function": {
                        "name": "Read", "arguments": "{\"a\":1,\"b\":2}"}}
                ]}
            ]
        });
        assert_eq!(
            canonicalize_tool_arguments(body.as_object_mut().unwrap()),
            0
        );
    }

    // --- reasoning epoch ---

    #[test]
    fn strip_historical_reasoning_keeps_current_epoch() {
        let mut body = json!({
            "messages": [
                {"role": "user", "content": "first task"},
                {"role": "assistant", "content": "", "reasoning_content": "old reasoning", "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "Read", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "ok"},
                {"role": "user", "content": "new task"},
                {"role": "assistant", "content": "", "reasoning_content": "current reasoning", "tool_calls": [
                    {"id": "c2", "type": "function", "function": {"name": "Edit", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "c2", "content": "done"}
            ]
        });
        let removed = strip_historical_reasoning(body.as_object_mut().unwrap());
        assert!(removed > 0);
        assert_eq!(body["messages"][1].get("reasoning_content"), None);
        assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "c1");
        assert_eq!(
            body["messages"][4]["reasoning_content"],
            "current reasoning"
        );
        assert_eq!(body["messages"][2]["content"], "ok");
    }

    #[test]
    fn strip_anthropic_thinking_keeps_current_epoch() {
        let mut request = json!({
            "messages": [
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "old"},
                    {"type": "text", "text": "answer"}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
                ]},
                {"role": "user", "content": "next turn"}
            ]
        });
        let removed = strip_anthropic_thinking(&mut request);
        assert_eq!(removed, 1);
        let blocks = request["messages"][1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
    }

    #[test]
    fn prefix_hash_changes_with_content_and_is_stable_otherwise() {
        let a =
            json!({"messages": [{"role": "user", "content": "one"}], "system": "s", "tools": []});
        let b =
            json!({"messages": [{"role": "user", "content": "two"}], "system": "s", "tools": []});
        assert_ne!(
            stable_prefix_hash(a.as_object().unwrap()).0,
            stable_prefix_hash(b.as_object().unwrap()).0
        );
        let a2 = a.clone();
        assert_eq!(
            stable_prefix_hash(a.as_object().unwrap()).0,
            stable_prefix_hash(a2.as_object().unwrap()).0
        );
    }

    // --- estimation ---

    #[test]
    fn estimator_is_conservative_and_monotonic() {
        let ascii = estimate_string_tokens("hello world hello world");
        let cjk = estimate_string_tokens("你好世界");
        assert!((5..=10).contains(&ascii));
        assert_eq!(cjk, 8);
        assert!(estimate_string_tokens("") == 0);
    }

    #[test]
    fn estimate_anthropic_tokens_counts_everything() {
        let request = json!({
            "model": "deepseek-flash",
            "max_tokens": 100,
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "hello world, this is a longer message for counting"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "a.rs"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "file content here"}
                ]}
            ],
            "tools": [{"name": "Read", "description": "Read a file", "input_schema": {"type": "object"}}]
        });
        let tokens = estimate_anthropic_tokens(&request);
        assert!(tokens > 20, "got {tokens}");
    }
}
