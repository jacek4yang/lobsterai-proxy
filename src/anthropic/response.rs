//! Anthropic response assembly (non-stream path).

use serde_json::Value;

/// Build the complete non-stream Anthropic Message JSON from the converter's
/// accumulated state (thinking only when requested, tool inputs parsed).
pub fn build_nonstream_message(converter: &super::stream::StreamConverter) -> Value {
    converter.nonstream_response()
}
