//! Anthropic Messages request �?OpenAI Chat Completions conversion.
//!
//! Correctness rules:
//! - tool_use �?tool_calls and tool_result �?role=tool with the SAME ids;
//!   a user message mixing tool results and text emits the tool messages
//!   first (OpenAI requires them adjacent to the assistant tool_calls) and
//!   the remaining content as the following user message;
//! - tool schemas pass through byte-identical (never truncated/edited);
//! - unknown or unsupported block types are rejected with a clear
//!   invalid_request_error �?nothing is silently dropped;
//! - lossless normalizations only: single text block �?string, empty text
//!   blocks dropped, Anthropic-only metadata (cache_control, metadata,
//!   thinking budget) not forwarded.

use serde_json::{json, Map, Value};

use crate::deepseek::policy::strip_billing_in_anthropic_system;
use crate::deepseek::reasoning::{resolve_exposure, ThinkingExposure};

use super::types::ProtocolError;

/// A fully converted request, ready for the DeepSeek policy pass.
#[derive(Debug, Clone)]
pub struct Converted {
    /// OpenAI Chat Completions body (without `stream` flags �?added later).
    pub chat_body: Value,
    /// Whether the client explicitly requested visible thinking.
    pub thinking_requested: bool,
    /// The model id the client asked for (logged only; the upstream model is
    /// the configured default).
    pub client_model: Option<String>,
}

/// Convert an Anthropic Messages request. `request` is mutated in place only
/// for lossless normalization (billing-header strip) so callers can reuse it
/// for token accounting.
pub fn convert_request(
    request: &mut Value,
    default_model: &str,
) -> Result<Converted, ProtocolError> {
    request
        .as_object_mut()
        .ok_or_else(|| ProtocolError::invalid("request body must be a JSON object"))?;

    // Lossless normalization: strip the volatile billing header first so the
    // system prefix is byte-stable across turns.
    strip_billing_in_anthropic_system(request);
    let object = request.as_object().expect("still an object");

    // --- model & sampling ---
    let client_model = object
        .get("model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let mut chat = Map::new();
    chat.insert("model".into(), Value::String(default_model.to_owned()));

    let max_tokens = object
        .get("max_tokens")
        .and_then(Value::as_u64)
        .filter(|v| *v > 0)
        .ok_or_else(|| {
            ProtocolError::invalid("max_tokens is required and must be a positive integer")
        })?;
    chat.insert("max_tokens".into(), json!(max_tokens));

    if let Some(temperature) = object.get("temperature").and_then(Value::as_f64) {
        chat.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = object.get("top_p").and_then(Value::as_f64) {
        chat.insert("top_p".into(), json!(top_p));
    }
    if let Some(top_k) = object.get("top_k").and_then(Value::as_u64) {
        chat.insert("top_k".into(), json!(top_k));
    }
    if let Some(stop) = object.get("stop_sequences") {
        let sequences = stop
            .as_array()
            .ok_or_else(|| ProtocolError::invalid("stop_sequences must be an array"))?;
        for sequence in sequences {
            if sequence.as_str().is_none() {
                return Err(ProtocolError::invalid(
                    "stop_sequences entries must be strings",
                ));
            }
        }
        if !sequences.is_empty() {
            chat.insert("stop".into(), Value::Array(sequences.clone()));
        }
    }

    // --- messages ---
    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ProtocolError::invalid("messages must be an array"))?;
    if messages.is_empty() {
        return Err(ProtocolError::invalid("messages must not be empty"));
    }
    chat.insert("messages".into(), Value::Array(convert_messages(messages)?));

    // --- tools ---
    if let Some(tools) = object.get("tools") {
        if !tools.as_array().is_none_or(Vec::is_empty) {
            chat.insert(
                "tools".into(),
                Value::Array(convert_tools(tools.as_array().unwrap())?),
            );
        }
    }
    // Whether the client explicitly requested visible thinking �?decided
    // before tool conversion (thinking mode constrains tool_choice).
    let thinking_requested =
        resolve_exposure(object.get("thinking")) == ThinkingExposure::Requested;
    if let Some(choice) = object.get("tool_choice") {
        let converted = convert_tool_choice(choice, thinking_requested)?;
        if let Some(converted) = converted {
            chat.insert("tool_choice".into(), converted);
        }
    }

    Ok(Converted {
        chat_body: Value::Object(chat),
        thinking_requested,
        client_model,
    })
}

// ---------------------------------------------------------------------------
// messages
// ---------------------------------------------------------------------------

fn convert_messages(messages: &[Value]) -> Result<Vec<Value>, ProtocolError> {
    let mut out = Vec::with_capacity(messages.len());
    // Official deepseek-recipe semantics: every tool_use must be resolved by
    // exactly one tool_result in the immediately following user message.
    let mut pending_tool_uses: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for (index, message) in messages.iter().enumerate() {
        let object = message.as_object().ok_or_else(|| {
            ProtocolError::invalid(format!("messages[{index}] must be an object"))
        })?;
        let role = object
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match role {
            "user" => {
                let converted = convert_user_message(object, index)?;
                for converted_message in &converted {
                    if converted_message.get("role").and_then(Value::as_str) == Some("tool") {
                        let tool_call_id = converted_message
                            .get("tool_call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !pending_tool_uses.contains_key(tool_call_id) {
                            return Err(ProtocolError::invalid(format!(
                                "messages[{index}]: tool_result references unknown tool_use_id {tool_call_id:?}; \
                                 each tool_result must follow the assistant message containing its tool_use"
                            )));
                        }
                        pending_tool_uses.remove(tool_call_id);
                    }
                }
                out.extend(converted);
            }
            "assistant" => {
                let converted = convert_assistant_message(object, index)?;
                // A split assistant message may contain several turns:
                // assistant(server tool_calls) + tool(server results) +
                // assistant(client tool_calls). Register every tool_call id
                // and settle the internally paired server results.
                for converted_message in &converted {
                    if let Some(Value::Array(tool_calls)) = converted_message.get("tool_calls") {
                        for tool_call in tool_calls {
                            let id = tool_call
                                .pointer("/id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            if pending_tool_uses.insert(id.clone(), index).is_some() {
                                return Err(ProtocolError::invalid(format!(
                                    "messages[{index}]: duplicate tool_use id {id:?}"
                                )));
                            }
                        }
                    }
                    if converted_message.get("role").and_then(Value::as_str) == Some("tool") {
                        let id = converted_message
                            .get("tool_call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        pending_tool_uses.remove(id);
                    }
                }
                out.extend(converted);
            }
            // Mid-history system-role messages (rare) are preserved as
            // system-reminder user content, matching official semantics.
            "system" | "developer" => {
                let text = match object.get("content") {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Array(blocks)) => blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                out.push(json!({
                    "role": "user",
                    "content": format!("<system-reminder>\n{text}\n</system-reminder>")
                }));
            }
            other => {
                return Err(ProtocolError::invalid(format!(
                    "messages[{index}].role {other:?} is not supported (expected user or assistant)"
                )))
            }
        }
    }
    if let Some((id, index)) = pending_tool_uses.iter().next() {
        return Err(ProtocolError::invalid(format!(
            "messages[{index}]: tool_use id {id:?} has no tool_result in the following message"
        )));
    }
    Ok(out)
}

/// Strip Anthropic-only block metadata for forwarding.
fn clean_block(block: &Value) -> Value {
    match block.as_object() {
        Some(object) => {
            let mut cleaned = object.clone();
            cleaned.remove("cache_control");
            cleaned.remove("citations");
            Value::Object(cleaned)
        }
        None => block.clone(),
    }
}

fn convert_user_message(
    message: &Map<String, Value>,
    index: usize,
) -> Result<Vec<Value>, ProtocolError> {
    let content = message.get("content").cloned().unwrap_or(Value::Null);
    if let Value::String(text) = content {
        return Ok(vec![json!({"role": "user", "content": text})]);
    }
    let blocks = content.as_array().ok_or_else(|| {
        ProtocolError::invalid(format!(
            "messages[{index}].content must be a string or block array"
        ))
    })?;
    let mut tool_messages = Vec::new();
    let mut ordinary: Vec<Value> = Vec::new();
    for block in blocks {
        let block = clean_block(block);
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "text" => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !text.is_empty() {
                    ordinary.push(json!({"type": "text", "text": text}));
                }
            }
            "image" => ordinary.push(convert_image_block(&block, index)?),
            "document" => ordinary.push(convert_document_block(&block, index)?),
            "tool_result" => tool_messages.push(convert_tool_result(&block, index)?),
            "thinking" | "redacted_thinking" => {
                return Err(ProtocolError::invalid(format!(
                    "messages[{index}]: thinking blocks are not valid in user messages"
                )))
            }
            other => {
                return Err(ProtocolError::invalid(format!(
                    "messages[{index}]: unsupported content block type {other:?}"
                )))
            }
        }
    }
    // Tool messages first (must follow the assistant tool_calls message),
    // then any remaining human content as the user message.
    let mut out = tool_messages;
    match ordinary.len() {
        0 => {}
        1 if ordinary[0].get("type").and_then(Value::as_str) == Some("text") => {
            out.push(json!({"role": "user", "content": ordinary[0]["text"].as_str().unwrap_or_default()}));
        }
        _ => {
            out.push(json!({"role": "user", "content": ordinary}));
        }
    }
    Ok(out)
}

fn convert_tool_result(block: &Value, index: usize) -> Result<Value, ProtocolError> {
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            ProtocolError::invalid(format!(
                "messages[{index}]: tool_result requires tool_use_id"
            ))
        })?;
    let content = match block.get("content") {
        None | Some(Value::Null) => Value::String(String::new()),
        Some(Value::String(text)) => Value::String(text.clone()),
        Some(Value::Array(parts)) => {
            let mut text_parts = Vec::new();
            let mut images = Vec::new();
            for part in parts {
                let part = clean_block(part);
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => text_parts.push(
                        part.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    ),
                    Some("image") => images.push(convert_image_block(&part, index)?),
                    Some("document") => images.push(convert_document_block(&part, index)?),
                    // Deferred-tool pointer (`{type: "tool_reference",
                    // tool_name}`): carries no content of its own �?it marks
                    // which tool the result belongs to / makes available.
                    // Represented as a text marker; the tool's full schema is
                    // already in the request's tools array.
                    Some("tool_reference") => {
                        let name = part.get("tool_name").and_then(Value::as_str).unwrap_or("?");
                        text_parts.push(format!("[tool reference: {name}]"));
                    }
                    // Anthropic `search_result` blocks: preserve title/URL
                    // verbatim as text; the snippet list stays readable.
                    Some("search_result") => {
                        let title = part.get("title").and_then(Value::as_str).unwrap_or("");
                        let url = part.get("url").and_then(Value::as_str).unwrap_or("");
                        text_parts.push(format!(
                            "{title}
{url}"
                        ));
                        if let Some(quote) = part.get("content").and_then(Value::as_str) {
                            text_parts.push(quote.to_owned());
                        }
                    }
                    Some(other) => {
                        return Err(ProtocolError::invalid(format!(
                            "messages[{index}]: unsupported tool_result content block {other:?}"
                        )))
                    }
                    None => {}
                }
            }
            if images.is_empty() {
                Value::String(text_parts.join("\n"))
            } else {
                // Multimodal tool results: text plus image parts.
                let mut content_parts: Vec<Value> = Vec::new();
                if !text_parts.is_empty() {
                    content_parts.push(json!({"type": "text", "text": text_parts.join("\n")}));
                }
                content_parts.extend(images);
                Value::Array(content_parts)
            }
        }
        Some(_) => {
            return Err(ProtocolError::invalid(format!(
                "messages[{index}]: invalid tool_result content"
            )))
        }
    };
    Ok(json!({"role": "tool", "tool_call_id": tool_use_id, "content": content}))
}

fn convert_assistant_message(
    message: &Map<String, Value>,
    index: usize,
) -> Result<Vec<Value>, ProtocolError> {
    let content = message.get("content").cloned().unwrap_or(Value::Null);
    if let Value::String(text) = content {
        return Ok(vec![json!({"role": "assistant", "content": text})]);
    }
    let blocks = content.as_array().ok_or_else(|| {
        ProtocolError::invalid(format!(
            "messages[{index}].content must be a string or block array"
        ))
    })?;

    // A single assistant message may semantically contain several turns
    // (text, thinking, parallel tool calls); consecutive text/thinking
    // blocks join into one assistant message, tool_calls attach to it.
    #[derive(Default)]
    struct Segment {
        text_parts: Vec<String>,
        reasoning_parts: Vec<String>,
        tool_calls: Vec<Value>,
    }
    impl Segment {
        fn is_empty(&self) -> bool {
            self.text_parts.is_empty()
                && self.reasoning_parts.is_empty()
                && self.tool_calls.is_empty()
        }
        fn into_message(self) -> Value {
            let mut out_message = Map::new();
            out_message.insert("role".into(), json!("assistant"));
            // Official semantics: consecutive text/thinking blocks join with "\n\n".
            let text = self.text_parts.join("\n\n");
            out_message.insert(
                "content".into(),
                if text.is_empty() {
                    Value::Null
                } else {
                    Value::String(text)
                },
            );
            if !self.reasoning_parts.is_empty() {
                out_message.insert(
                    "reasoning_content".into(),
                    Value::String(self.reasoning_parts.join("\n\n")),
                );
            }
            if !self.tool_calls.is_empty() {
                out_message.insert("tool_calls".into(), Value::Array(self.tool_calls));
            }
            Value::Object(out_message)
        }
    }

    let mut output: Vec<Value> = Vec::new();
    let mut segment = Segment::default();
    let flush = |output: &mut Vec<Value>, segment: &mut Segment| {
        if !segment.is_empty() {
            output.push(std::mem::take(segment).into_message());
        }
    };

    for block in blocks {
        let block = clean_block(block);
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "text" => segment.text_parts.push(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ),
            "thinking" => {
                if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                    segment.reasoning_parts.push(text.to_owned());
                }
            }
            "redacted_thinking" => {
                // Encrypted thinking is opaque to this proxy; skip (documented).
            }
            "tool_use" => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        ProtocolError::invalid(format!("messages[{index}]: tool_use requires id"))
                    })?;
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        ProtocolError::invalid(format!("messages[{index}]: tool_use requires name"))
                    })?;
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                if !input.is_object() {
                    return Err(ProtocolError::invalid(format!(
                        "messages[{index}]: tool_use input must be an object"
                    )));
                }
                segment.tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": serde_json::to_string(&input)
                            .map_err(|_| ProtocolError::invalid("tool_use input is not serializable"))?,
                    }
                }));
            }
            other => {
                return Err(ProtocolError::invalid(format!(
                    "messages[{index}]: unsupported assistant content block {other:?}"
                )))
            }
        }
    }
    flush(&mut output, &mut segment);
    if output.is_empty() {
        // Degenerate message (e.g. empty block array): keep a valid
        // assistant turn rather than dropping it from the chain.
        output.push(json!({"role": "assistant", "content": Value::Null}));
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// blocks
// ---------------------------------------------------------------------------

fn convert_image_block(block: &Value, index: usize) -> Result<Value, ProtocolError> {
    let source = block
        .get("source")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            ProtocolError::invalid(format!("messages[{index}]: image source must be an object"))
        })?;
    let url = match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png");
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!("data:{media_type};base64,{data}")
        }
        Some("url") => source
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        other => {
            return Err(ProtocolError::invalid(format!(
                "messages[{index}]: unsupported image source {other:?}"
            )))
        }
    };
    Ok(json!({"type": "image_url", "image_url": {"url": url}}))
}

fn convert_document_block(block: &Value, index: usize) -> Result<Value, ProtocolError> {
    let source = block
        .get("source")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            ProtocolError::invalid(format!(
                "messages[{index}]: document source must be an object"
            ))
        })?;
    let filename = block
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("document")
        .to_owned();
    let file_data = match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("application/pdf");
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!("data:{media_type};base64,{data}")
        }
        Some("text") => {
            let text = source
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let encoded =
                crate::deepseek::policy::canonical_json_string(&Value::String(text.to_owned()));
            format!("data:text/plain;base64,{}", encoded.trim_matches('"'))
        }
        other => {
            return Err(ProtocolError::invalid(format!(
                "messages[{index}]: unsupported document source {other:?}"
            )))
        }
    };
    Ok(json!({"type": "file", "file": {"filename": filename, "file_data": file_data}}))
}

// ---------------------------------------------------------------------------
// tools & tool_choice
// ---------------------------------------------------------------------------

fn convert_tools(tools: &[Value]) -> Result<Vec<Value>, ProtocolError> {
    let mut out = Vec::with_capacity(tools.len());
    for (index, tool) in tools.iter().enumerate() {
        let object = tool
            .as_object()
            .ok_or_else(|| ProtocolError::invalid(format!("tools[{index}] must be an object")))?;
        // Passthrough if already OpenAI-shaped (never happens from Claude Code).
        if object.contains_key("function") {
            out.push(tool.clone());
            continue;
        }
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("custom");
        if kind != "custom" {
            // Server tools (`web_search_*`, code execution, …) are not
            // executable by this proxy: rejected explicitly rather than
            // offered to the model with no way to answer the call.
            return Err(ProtocolError::invalid(format!(
                "tools[{index}]: tool type {kind:?} is not supported"
            )));
        }
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| ProtocolError::invalid(format!("tools[{index}].name is required")))?;
        let mut function = Map::new();
        function.insert("name".into(), json!(name));
        if let Some(description) = object.get("description").and_then(Value::as_str) {
            function.insert("description".into(), json!(description));
        }
        if let Some(schema) = object.get("input_schema") {
            if !schema.is_object() {
                return Err(ProtocolError::invalid(format!(
                    "tools[{index}].input_schema must be an object"
                )));
            }
            // Pass the schema through byte-identical (never edited).
            function.insert("parameters".into(), schema.clone());
        }
        out.push(json!({"type": "function", "function": function}));
    }
    Ok(out)
}

fn convert_tool_choice(
    choice: &Value,
    thinking_requested: bool,
) -> Result<Option<Value>, ProtocolError> {
    let object = choice
        .as_object()
        .ok_or_else(|| ProtocolError::invalid("tool_choice must be an object"))?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // Official semantics: thinking mode only supports auto tool choice.
    if thinking_requested && kind != "auto" {
        return Err(ProtocolError::invalid(
            "thinking mode only supports tool_choice type \"auto\"",
        ));
    }
    let converted = match kind {
        "auto" => json!("auto"),
        "any" => json!("required"),
        "none" => json!("none"),
        "tool" => {
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| ProtocolError::invalid("tool_choice.type=tool requires name"))?;
            // The upstream only accepts string tool_choice forms; degrade a
            // forced function to auto (the function stays in `tools`).
            let _ = name;
            json!("auto")
        }
        other => {
            return Err(ProtocolError::invalid(format!(
                "unsupported tool_choice type {other:?}"
            )))
        }
    };
    Ok(Some(converted))
}
