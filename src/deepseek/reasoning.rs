//! deepseek-v4.1-flash reasoning semantics.
//!
//! Wire behavior (established from the reference implementation and live
//! upstream responses — nothing is invented):
//! - reasoning arrives as `delta.reasoning_content` on chat-completions
//!   chunks, before text/tool deltas of the same turn;
//! - reasoning is produced automatically; no client-side effort field is
//!   sent upstream;
//! - `usage.completion_tokens_details.reasoning_tokens` may report reasoning
//!   output tokens.
//!
//! Policy:
//! - `requested_only` exposure: thinking blocks are emitted to the client
//!   only when the client explicitly requested thinking;
//! - unrequested reasoning is consumed in-turn by the reasoning shadow so
//!   the tool loop keeps its continuity without polluting Claude Code
//!   history;
//! - historical epochs are stripped before each request (see `policy.rs`).

/// Whether the client asked for visible thinking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingExposure {
    Requested,
    NotRequested,
}

/// Resolve the exposure decision from the Anthropic `thinking` field.
pub fn resolve_exposure(thinking: Option<&serde_json::Value>) -> ThinkingExposure {
    match thinking {
        Some(value) => match value.get("type").and_then(serde_json::Value::as_str) {
            Some("enabled") => ThinkingExposure::Requested,
            _ => ThinkingExposure::NotRequested,
        },
        None => ThinkingExposure::NotRequested,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exposure_is_requested_only_when_explicitly_enabled() {
        assert_eq!(
            resolve_exposure(Some(&json!({"type": "enabled"}))),
            ThinkingExposure::Requested
        );
        assert_eq!(
            resolve_exposure(Some(&json!({"type": "disabled"}))),
            ThinkingExposure::NotRequested
        );
        assert_eq!(
            resolve_exposure(Some(&json!({}))),
            ThinkingExposure::NotRequested
        );
        assert_eq!(resolve_exposure(None), ThinkingExposure::NotRequested);
        assert_eq!(
            resolve_exposure(Some(&json!({"type": "enabled", "budget_tokens": 4096}))),
            ThinkingExposure::Requested
        );
    }
}
