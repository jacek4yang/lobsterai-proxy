//! Anthropic wire types: stop-reason mapping, usage mapping, SSE event
//! formatting, and request validation error.

use serde_json::{json, Value};

/// Conversion/validation error surfaced as an Anthropic invalid_request_error.
#[derive(Debug, Clone)]
pub struct ProtocolError {
    pub message: String,
}

impl ProtocolError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// OpenAI `finish_reason` → Anthropic `stop_reason`.
pub fn stop_reason_from_finish(finish: Option<&str>) -> &'static str {
    match finish {
        Some("tool_calls") | Some("function_call") => "tool_use",
        Some("length") => "max_tokens",
        Some("stop_sequence") => "stop_sequence",
        _ => "end_turn",
    }
}

/// OpenAI usage → Anthropic usage. Cached reads are subtracted from input.
pub fn map_usage(usage: &Value) -> (u64, u64, u64) {
    let prompt = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    ((prompt.saturating_sub(cached)), completion, cached)
}

/// Format one Anthropic SSE event (`event: X\ndata: {...}\n\n`).
pub fn sse_event(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

pub fn sse_ping() -> String {
    sse_event("ping", &json!({"type": "ping"}))
}

/// Format an Anthropic SSE error event (mid-stream failures).
pub fn sse_error(kind: &str, message: &str) -> String {
    sse_event(
        "error",
        &json!({"type": "error", "error": {"type": kind, "message": message}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(stop_reason_from_finish(Some("stop")), "end_turn");
        assert_eq!(stop_reason_from_finish(Some("tool_calls")), "tool_use");
        assert_eq!(stop_reason_from_finish(Some("length")), "max_tokens");
        assert_eq!(stop_reason_from_finish(Some("content-filter")), "end_turn");
        assert_eq!(stop_reason_from_finish(None), "end_turn");
    }

    #[test]
    fn usage_mapping_subtracts_cache_read() {
        let usage = json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 30}
        });
        let (input, output, cached) = map_usage(&usage);
        assert_eq!((input, output, cached), (70, 20, 30));
        let (input, output, cached) = map_usage(&json!({"prompt_tokens": 5}));
        assert_eq!((input, output, cached), (5, 0, 0));
    }

    #[test]
    fn sse_formatting() {
        let event = sse_event("message_stop", &json!({"type": "message_stop"}));
        assert!(event.starts_with("event: message_stop\ndata: "));
        assert!(event.ends_with("\n\n"));
        assert!(sse_ping().starts_with("event: ping\n"));
    }
}
