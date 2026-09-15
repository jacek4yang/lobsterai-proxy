//! Responses API request �?OpenAI Chat Completions conversion.
//!
//! Compatibility target: Grok Build (`api_backend = "responses"`), verified
//! against `xai-org/grok-build` `conversation/responses.rs` (request mapping)
//! and the Responses wire schema. The request is normalized (never forwarded
//! blindly) into the same OpenAI Chat Completions body the Anthropic frontend
//! produces, so all shared policy (deepseek reasoning epochs, prefix
//! stability, canonical tool arguments) applies unchanged.
//!
//! Conversion rules:
//! - `input` string �?one user message; array items convert in order
//!   (ordering is replayed conversation state �?never reordered);
//!   `input_image` content parts (message or tool-output) become OpenAI
//!   `image_url` parts (same multimodal shape the Anthropic frontend emits);
//! - `function_call` history �?assistant `tool_calls` with the SAME
//!   `call_id`; consecutive calls of one assistant turn are merged into one
//!   message (matching how the model emits them); `function_call_output` �?
//!   `role=tool` with `tool_call_id = call_id`;
//! - `reasoning` input items are dropped (historical reasoning is stripped
//!   by policy for every frontend; text/tool semantics are unaffected);
//! - flat Responses function tools �?nested OpenAI function tools, schemas
//!   byte-identical;
//! - `max_output_tokens` �?`max_tokens` (never dropped);
//! - `prompt_cache_key` �?forwarded verbatim to the upstream passthrough;
//! - unknown optional fields (`store`, `metadata`, `previous_response_id`,
//!   `truncation`, `stream_options`, �? are ignored safely;
//! - hosted tools (`web_search`, `x_search`, `code_interpreter`, MCP, ...) are
//!   dropped, not forwarded: this proxy cannot execute them and never fakes
//!   results, and the model must not be offered a tool whose call could never
//!   be answered (Grok Build sends a default `web_search` declaration
//!   routinely; unsupported entries are dropped so the request can proceed
//!   with the client function tools).
//!   be answered (Grok Build sends a default `web_search` declaration
//!   routinely; unsupported entries are dropped so the request can proceed
//!   with the client function tools).

use serde_json::{json, Map, Value};

use super::types::ProtocolError;

/// A converted Responses request, ready for the shared generation path.
#[derive(Debug, Clone)]
pub struct Converted {
    /// OpenAI Chat Completions body (without `stream` �?forced upstream).
    pub chat_body: Value,
    /// Whether the client requested reasoning exposure via
    /// `reasoning.summary` (Responses equivalent of Anthropic thinking).
    pub thinking_requested: bool,
    /// The client-requested model id (informational only).
    pub client_model: Option<String>,
    /// The raw `prompt_cache_key` (fingerprinted by the caller before any
    /// use as a session identity; forwarded verbatim to the upstream).
    pub prompt_cache_key: Option<String>,
}

/// Convert a Responses request into the normalized chat body.
pub fn convert_request(request: &Value, default_model: &str) -> Result<Converted, ProtocolError> {
    let object = request
        .as_object()
        .ok_or_else(|| ProtocolError::invalid("request body must be a JSON object"))?;

    // --- model & sampling ---
    let client_model = object
        .get("model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let mut chat = Map::new();
    chat.insert("model".into(), Value::String(default_model.to_owned()));

    // max_output_tokens �?max_tokens. Optional in Responses (unlike Anthropic)
    // but never silently dropped when present.
    if let Some(max) = object.get("max_output_tokens") {
        let max = max.as_u64().filter(|v| *v > 0).ok_or_else(|| {
            ProtocolError::invalid("max_output_tokens must be a positive integer")
        })?;
        chat.insert("max_tokens".into(), json!(max));
    }
    if let Some(temperature) = object.get("temperature").and_then(Value::as_f64) {
        chat.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = object.get("top_p").and_then(Value::as_f64) {
        chat.insert("top_p".into(), json!(top_p));
    }
    // prompt_cache_key: sticky-routing + upstream prompt-cache hint. Forwarded
    // verbatim (the upstream passthrough already carries it); the caller
    // fingerprints it before using it as a session identity.
    let prompt_cache_key = object
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned);
    if let Some(key) = &prompt_cache_key {
        chat.insert("prompt_cache_key".into(), json!(key));
    }

    // --- reasoning: map effort to the existing upstream mechanism ---
    // Grok Build sends `reasoning: {effort, summary}` (summary always
    // "concise" in practice). There is no second reasoning system here:
    // - effort low/medium/high/xhigh �?`reasoning_effort` on the wire (the
    //   same field the GLM policy sets; deepseek reads it as a hint and the
    //   passthrough forwards it when present). `minimal`/`none` mean "do not
    //   emphasize reasoning" and map to `low` rather than being dropped, so
    //   an explicit request never silently upgrades to the upstream default.
    // - summary present �?reasoning may be EXPOSED to the client as a
    //   Responses reasoning item (requested-only exposure, matching the
    //   Anthropic frontend's `thinking: enabled` gate).
    let mut thinking_requested = false;
    if let Some(reasoning) = object.get("reasoning") {
        let reasoning = reasoning
            .as_object()
            .ok_or_else(|| ProtocolError::invalid("reasoning must be an object"))?;
        if let Some(effort) = reasoning.get("effort").and_then(Value::as_str) {
            let wire = match effort {
                "none" | "minimal" => "low",
                "low" => "low",
                "medium" | "high" | "xhigh" | "max" => "high",
                other => {
                    return Err(ProtocolError::invalid(format!(
                        "reasoning.effort {other:?} is not supported (expected one of none, minimal, low, medium, high, xhigh, max)"
                    )))
                }
            };
            chat.insert("reasoning_effort".into(), json!(wire));
        }
        thinking_requested = reasoning
            .get("summary")
            .and_then(Value::as_str)
            .is_some_and(|summary| summary != "none")
            || reasoning.get("effort").and_then(Value::as_str).is_none();
    }

    // --- messages ---
    chat.insert("messages".into(), Value::Array(convert_input(object)?));

    // --- tools ---
    // Backend-hosted tools (`web_search`, `x_search`, code_interpreter, MCP,
    // ��) are dropped, not forwarded: this proxy cannot execute them and never
    // fakes results, and the model must not be offered a tool whose call
    // could never be answered. Client function tools pass through.
    if let Some(tools) = object.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| ProtocolError::invalid("tools must be an array"))?;
        if !tools.is_empty() {
            let converted = convert_tools(tools)?;
            if !converted.is_empty() {
                chat.insert("tools".into(), Value::Array(converted));
            }
        }
    }
    if let Some(choice) = object.get("tool_choice") {
        if let Some(converted) = convert_tool_choice(choice)? {
            chat.insert("tool_choice".into(), converted);
        }
    }

    Ok(Converted {
        chat_body: Value::Object(chat),
        thinking_requested,
        client_model,
        prompt_cache_key,
    })
}

// ---------------------------------------------------------------------------
// input items �?chat messages
// ---------------------------------------------------------------------------

/// Convert the Responses `input` field. Supports the string convenience form
/// and the full item array (both `EasyInputMessage` and typed items). Order
/// is preserved exactly �?this is replayed conversation state.
fn convert_input(object: &Map<String, Value>) -> Result<Vec<Value>, ProtocolError> {
    match object.get("input") {
        Some(Value::String(text)) => Ok(vec![json!({"role": "user", "content": text})]),
        Some(Value::Array(items)) => convert_input_items(items),
        Some(_) => Err(ProtocolError::invalid(
            "input must be a string or an array of items",
        )),
        None => Err(ProtocolError::invalid("input is required")),
    }
}

fn convert_input_items(items: &[Value]) -> Result<Vec<Value>, ProtocolError> {
    let mut messages: Vec<Value> = Vec::with_capacity(items.len());
    // Consecutive function_call items belong to one assistant turn; merged
    // into a single message with multiple tool_calls (never merged across a
    // message/output boundary, never given new ids).
    let mut pending_calls: Vec<Value> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| ProtocolError::invalid(format!("input[{index}] must be an object")))?;
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("message");
        match kind {
            "message" => convert_message_item(obj, index, &mut messages)?,
            "function_call" => {
                let call = convert_function_call(obj, index)?;
                pending_calls.push(call);
            }
            "function_call_output" => {
                flush_calls(&mut messages, &mut pending_calls);
                messages.push(convert_function_call_output(obj, index)?);
            }
            "reasoning" => {
                // Historical reasoning is deliberately not replayed onto the
                // chat wire: policy strips historical reasoning for every
                // frontend (epoch boundary), and `encrypted_content` is
                // provider-opaque. Text/tool-call semantics are unaffected;
                // user/assistant/tool ordering is preserved.
                flush_calls(&mut messages, &mut pending_calls);
            }
            "web_search_call" => {
                // A hosted-search call from a previous turn. This proxy never
                // executes searches, so it cannot ground a fresh answer on
                // replayed results: rejected explicitly rather than answered
                // with fabricated content. Sessions that keep Grok Build's
                // default hosted `web_search` enabled hit this; the client
                // (or operator) must disable hosted search for the proxy.
                return Err(ProtocolError::invalid(
                    "input item type \"web_search_call\" is not supported: this proxy does not execute hosted web search",
                ));
            }
            "item_reference" => {
                return Err(ProtocolError::invalid(
                    "input item type \"item_reference\" is not supported (this proxy is stateless; replay full items)",
                ))
            }
            other => {
                return Err(ProtocolError::invalid(format!(
                    "input[{index}]: unsupported item type {other:?}"
                )))
            }
        }
    }
    flush_calls(&mut messages, &mut pending_calls);
    if messages.is_empty() {
        return Err(ProtocolError::invalid("input must not be empty"));
    }
    Ok(messages)
}

fn flush_calls(messages: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if !pending.is_empty() {
        let mut message = Map::new();
        message.insert("role".into(), json!("assistant"));
        message.insert("content".into(), Value::Null);
        message.insert("tool_calls".into(), Value::Array(std::mem::take(pending)));
        messages.push(Value::Object(message));
    }
}

/// One `type: "message"` item: role + string or content-part array.
fn convert_message_item(
    obj: &Map<String, Value>,
    index: usize,
    messages: &mut Vec<Value>,
) -> Result<(), ProtocolError> {
    let role = obj.get("role").and_then(Value::as_str).ok_or_else(|| {
        ProtocolError::invalid(format!("input[{index}]: message requires a role"))
    })?;
    let content = obj.get("content").cloned().unwrap_or(Value::Null);
    let (text, images) = match content {
        Value::String(text) => (text, Vec::new()),
        Value::Array(parts) => {
            let mut text_parts = Vec::new();
            let mut images = Vec::new();
            for (part_index, part) in parts.iter().enumerate() {
                let part = part.as_object().ok_or_else(|| {
                    ProtocolError::invalid(format!(
                        "input[{index}].content[{part_index}] must be an object"
                    ))
                })?;
                match part.get("type").and_then(Value::as_str) {
                    // input_text (input shapes) and output_text (assistant
                    // replay) both carry plain text.
                    Some("input_text") | Some("output_text") => {
                        text_parts.push(
                            part.get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        );
                    }
                    Some("input_image") => {
                        images.push(convert_input_image(part, index, part_index)?)
                    }
                    Some("refusal") => text_parts.push(
                        part.get("refusal")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    ),
                    Some(other) => {
                        return Err(ProtocolError::invalid(format!(
                        "input[{index}].content[{part_index}]: unsupported content type {other:?}"
                    )))
                    }
                    None => {
                        return Err(ProtocolError::invalid(format!(
                            "input[{index}].content[{part_index}] requires a type"
                        )))
                    }
                }
            }
            (text_parts.join("\n"), images)
        }
        Value::Null => (String::new(), Vec::new()),
        _ => {
            return Err(ProtocolError::invalid(format!(
                "input[{index}].content must be a string or an array"
            )))
        }
    };
    match role {
        "system" | "developer" => {
            if !images.is_empty() {
                return Err(ProtocolError::invalid(format!(
                    "input[{index}]: system/developer messages cannot carry images"
                )));
            }
            messages.push(json!({"role": "system", "content": text}))
        }
        "user" => {
            if images.is_empty() {
                messages.push(json!({"role": "user", "content": text}));
            } else {
                // Multimodal user message: text plus image parts.
                let mut content_parts: Vec<Value> = Vec::new();
                if !text.is_empty() {
                    content_parts.push(json!({"type": "text", "text": text}));
                }
                content_parts.extend(images);
                messages.push(json!({"role": "user", "content": content_parts}));
            }
        }
        "assistant" => {
            if !images.is_empty() {
                return Err(ProtocolError::invalid(format!(
                    "input[{index}]: assistant messages cannot carry images"
                )));
            }
            if text.is_empty() {
                // Assistant replay with no text: keep the turn valid without
                // inventing content.
                messages.push(json!({"role": "assistant", "content": Value::Null}));
            } else {
                messages.push(json!({"role": "assistant", "content": text}));
            }
        }
        other => {
            return Err(ProtocolError::invalid(format!(
                "input[{index}]: unsupported message role {other:?}"
            )))
        }
    }
    Ok(())
}

/// `type: "function_call"` �?one OpenAI tool call. `call_id` is preserved
/// byte-identical end-to-end (never regenerated).
fn convert_function_call(obj: &Map<String, Value>, index: usize) -> Result<Value, ProtocolError> {
    let call_id = obj
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            ProtocolError::invalid(format!("input[{index}]: function_call requires call_id"))
        })?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            ProtocolError::invalid(format!("input[{index}]: function_call requires name"))
        })?;
    // Arguments are a JSON-encoded string on the wire. Malformed strings are
    // forwarded as-is (the model authored them; rewriting could corrupt).
    let arguments = obj
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}")
        .to_owned();
    Ok(json!({
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": arguments}
    }))
}

/// `type: "function_call_output"` �?`role=tool` with the same `call_id`.
fn convert_function_call_output(
    obj: &Map<String, Value>,
    index: usize,
) -> Result<Value, ProtocolError> {
    let call_id = obj
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            ProtocolError::invalid(format!(
                "input[{index}]: function_call_output requires call_id"
            ))
        })?;
    // Output may be a plain string or a content-part list; text parts are
    // joined in order, `input_image` parts become OpenAI `image_url` parts
    // (same shape the Anthropic frontend emits for multimodal tool results).
    // Tool results are never truncated or rewritten.
    let content = match obj.get("output") {
        None | Some(Value::Null) => Value::String(String::new()),
        Some(Value::String(text)) => Value::String(text.clone()),
        Some(Value::Array(parts)) => {
            let mut text_parts = Vec::new();
            let mut images = Vec::new();
            for (part_index, part) in parts.iter().enumerate() {
                let part = part.as_object().ok_or_else(|| {
                    ProtocolError::invalid(format!(
                        "input[{index}].output[{part_index}] must be an object"
                    ))
                })?;
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("output_text") => text_parts.push(
                        part.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    ),
                    Some("input_image") => {
                        images.push(convert_input_image(part, index, part_index)?)
                    }
                    Some(other) => {
                        return Err(ProtocolError::invalid(format!(
                        "input[{index}].output[{part_index}]: unsupported content type {other:?}"
                    )))
                    }
                    None => {
                        return Err(ProtocolError::invalid(format!(
                            "input[{index}].output[{part_index}] requires a type"
                        )))
                    }
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
                "input[{index}]: function_call_output.output must be a string or an array"
            )))
        }
    };
    Ok(json!({"role": "tool", "tool_call_id": call_id, "content": content}))
}

/// Responses `input_image` content part �?OpenAI `image_url` part. Accepts
/// both the object form (`{"type":"input_image","image_url":"data:..."}` or a
/// nested `{"url": ...}`) and mirrors the Anthropic frontend's passthrough
/// behavior for URL sources. Grok Build sends base64 data URLs extracted from
/// tool results (`read_file` on an image/PDF, MCP image content); it
/// pre-normalizes them under ~1.5 MB / 2000 px, so no size policy is applied
/// here.
fn convert_input_image(
    part: &Map<String, Value>,
    index: usize,
    part_index: usize,
) -> Result<Value, ProtocolError> {
    let url = match part.get("image_url") {
        Some(Value::String(url)) => url.clone(),
        Some(value @ Value::Object(_)) => value
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProtocolError::invalid(format!(
                    "input[{index}].output[{part_index}]: input_image.image_url object requires url"
                ))
            })?
            .to_owned(),
        Some(Value::Null) | None => {
            // Plain form: {"type":"input_image","url":"data:..."}.
            part.get("url")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    ProtocolError::invalid(format!(
                        "input[{index}].output[{part_index}]: input_image requires image_url or url"
                    ))
                })?
        }
        Some(_) => {
            return Err(ProtocolError::invalid(format!(
            "input[{index}].output[{part_index}]: input_image.image_url must be a string or object"
        )))
        }
    };
    if url.is_empty() {
        return Err(ProtocolError::invalid(format!(
            "input[{index}].output[{part_index}]: input_image url is empty"
        )));
    }
    Ok(json!({"type": "image_url", "image_url": {"url": url}}))
}

// ---------------------------------------------------------------------------
// tools & tool_choice
// ---------------------------------------------------------------------------

/// Flat Responses function tool �?nested OpenAI function tool. The `parameters`
/// schema passes through byte-identical (never truncated or rewritten).
fn convert_tools(tools: &[Value]) -> Result<Vec<Value>, ProtocolError> {
    let mut out = Vec::with_capacity(tools.len());
    for (index, tool) in tools.iter().enumerate() {
        let obj = tool
            .as_object()
            .ok_or_else(|| ProtocolError::invalid(format!("tools[{index}] must be an object")))?;
        match obj.get("type").and_then(Value::as_str) {
            // Already OpenAI-shaped (defensive; Grok never sends this).
            Some("function") if obj.contains_key("function") => out.push(tool.clone()),
            Some("function") | None => {
                let name = obj
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        ProtocolError::invalid(format!("tools[{index}].name is required"))
                    })?;
                let mut function = Map::new();
                function.insert("name".into(), json!(name));
                if let Some(description) = obj.get("description").and_then(Value::as_str) {
                    function.insert("description".into(), json!(description));
                }
                if let Some(schema) = obj.get("parameters") {
                    if !schema.is_object() {
                        return Err(ProtocolError::invalid(format!(
                            "tools[{index}].parameters must be an object"
                        )));
                    }
                    function.insert("parameters".into(), schema.clone());
                }
                if let Some(strict) = obj.get("strict") {
                    if strict.is_boolean() {
                        function.insert("strict".into(), strict.clone());
                    }
                }
                out.push(json!({"type": "function", "function": function}));
            }
            // Backend-hosted tools (web_search, x_search,
            // code_interpreter, MCP, �?: this proxy cannot execute them and
            // never fakes results. Drop the declaration instead of failing
            // the request �?Grok Build sends a default `web_search` entry
            // routinely, and a hard 400 would break every session with
            // backend search left at its default. Unsupported entries are
            // dropped so the model never sees a tool it could call but whose
            // result could never be produced; client function tools continue
            // to pass through.
            Some(other) => {
                tracing::debug!(
                    tool_type = other,
                    tool_index = index,
                    "dropped backend-hosted tool declaration (not executable by this proxy)"
                );
            }
        }
    }
    Ok(out)
}

/// Responses `tool_choice` �?OpenAI `tool_choice`. Grok sends the string
/// modes or `{type: "function", name}`; unsupported shapes are rejected.
/// The upstream only accepts the string forms: a forced function
/// degrades to `"auto"` (the model still sees the function in `tools`).
fn convert_tool_choice(choice: &Value) -> Result<Option<Value>, ProtocolError> {
    match choice {
        Value::String(mode) => match mode.as_str() {
            "auto" | "none" | "required" => Ok(Some(json!(mode))),
            other => Err(ProtocolError::invalid(format!(
                "unsupported tool_choice mode {other:?}"
            ))),
        },
        Value::Object(obj) => match obj.get("type").and_then(Value::as_str) {
            Some("function") => {
                obj.get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        ProtocolError::invalid("tool_choice.type=function requires name")
                    })?;
                // Upstream rejects the object form (`Request.tool_choice of
                // type string`); degrade to auto rather than fail.
                Ok(Some(json!("auto")))
            }
            Some("auto") | Some("none") | Some("required") | Some("allowed_tools") => {
                Ok(Some(json!("auto")))
            }
            Some(other) => Err(ProtocolError::invalid(format!(
                "unsupported tool_choice type {other:?}"
            ))),
            None => Err(ProtocolError::invalid("tool_choice requires a type")),
        },
        Value::Null => Ok(None),
        _ => Err(ProtocolError::invalid(
            "tool_choice must be a string or object",
        )),
    }
}
