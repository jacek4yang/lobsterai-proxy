//! OpenAI Chat Completions SSE → Anthropic Messages SSE conversion, with
//! strict block lifecycles and full accumulation for non-stream responses
//! and the reasoning shadow.
//!
//! Block ordering mirrors upstream semantics: thinking deltas open a thinking
//! block that is closed when text or tool content begins; parallel tool calls
//! each get their own block with `input_json_delta` fragments. `finish`
//! closes every open block, emits `message_delta` (stop reason + usage) and
//! `message_stop`.

use serde_json::{json, Map, Value};

use super::types::{map_usage, sse_event, stop_reason_from_finish};

/// Aggregated upstream turn data (for non-stream responses and shadowing).
#[derive(Debug, Default, Clone)]
pub struct Accumulated {
    pub reasoning: String,
    pub text: String,
    pub tool_calls: Vec<ToolCallAccum>,
    pub finish_reason: Option<String>,
    /// (input_tokens, output_tokens, cached_tokens)
    pub usage: Option<(u64, u64, u64)>,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolCallAccum {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Default)]
struct ToolSlot {
    id: String,
    name: String,
    arguments: String,
    block_idx: usize,
    started: bool,
}

/// The streaming converter. Feed parsed OpenAI SSE chunks; emitted Anthropic
/// SSE bytes accumulate in the caller's buffer.
pub struct StreamConverter {
    msg_id: String,
    /// Whether thinking blocks are surfaced to the client.
    expose_thinking: bool,
    started: bool,
    next_block_idx: usize,

    thinking_open: bool,
    thinking_idx: usize,
    thinking_accum: String,

    text_open: bool,
    text_idx: usize,
    text_accum: String,

    tools: std::collections::BTreeMap<i64, ToolSlot>,

    finish_reason: Option<String>,
    usage: Option<(u64, u64, u64)>,
    model: Option<String>,
}

impl StreamConverter {
    pub fn new(expose_thinking: bool) -> Self {
        Self {
            msg_id: format!("msg_{}", crate::session::random_hex_16()),
            expose_thinking,
            started: false,
            next_block_idx: 0,
            thinking_open: false,
            thinking_idx: 0,
            thinking_accum: String::new(),
            text_open: false,
            text_idx: 0,
            text_accum: String::new(),
            tools: std::collections::BTreeMap::new(),
            finish_reason: None,
            usage: None,
            model: None,
        }
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn finish_reason(&self) -> Option<&str> {
        self.finish_reason.as_deref()
    }

    /// Cheap progress probes for the pump (avoid cloning accumulators).
    pub fn has_reasoning(&self) -> bool {
        !self.thinking_accum.is_empty()
    }

    pub fn has_text(&self) -> bool {
        !self.text_accum.is_empty()
    }

    pub fn has_tools(&self) -> bool {
        !self.tools.is_empty()
    }

    /// Everything accumulated so far (safe to call multiple times).
    pub fn accumulated(&self) -> Accumulated {
        Accumulated {
            reasoning: self.thinking_accum.clone(),
            text: self.text_accum.clone(),
            tool_calls: self
                .tools
                .values()
                .map(|slot| ToolCallAccum {
                    id: slot.id.clone(),
                    name: slot.name.clone(),
                    arguments: slot.arguments.clone(),
                })
                .collect(),
            finish_reason: self.finish_reason.clone(),
            usage: self.usage,
            model: self.model.clone(),
        }
    }

    /// Feed one parsed OpenAI SSE chunk; append emitted events to `out`.
    /// Returns true when the chunk carried semantic progress.
    pub fn feed_chunk(&mut self, chunk: &Value, out: &mut Vec<u8>) -> bool {
        let mut semantic = false;
        if let Some(model) = chunk.get("model").and_then(Value::as_str) {
            self.model = Some(model.to_owned());
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(map_usage(usage));
        }
        if !self.started {
            self.started = true;
            let model = self.model.clone().unwrap_or_default();
            let event = sse_event(
                "message_start",
                &json!({
                    "type": "message_start",
                    "message": {
                        "id": self.msg_id,
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": model,
                        "stop_reason": Value::Null,
                        "stop_sequence": Value::Null,
                        "usage": {"input_tokens": 0, "output_tokens": 0}
                    }
                }),
            );
            out.extend_from_slice(event.as_bytes());
        }

        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            return semantic;
        };
        for choice in choices {
            if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(finish.to_owned());
                semantic = true;
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            // reasoning_content → thinking block (only when requested).
            if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
                if !reasoning.is_empty() {
                    semantic = true;
                    self.thinking_accum.push_str(reasoning);
                    if self.expose_thinking {
                        if !self.thinking_open {
                            self.thinking_idx = self.next_block_idx;
                            self.next_block_idx += 1;
                            self.thinking_open = true;
                            out.extend_from_slice(
                                sse_event(
                                    "content_block_start",
                                    &json!({
                                        "type": "content_block_start",
                                        "index": self.thinking_idx,
                                        "content_block": {"type": "thinking", "thinking": ""}
                                    }),
                                )
                                .as_bytes(),
                            );
                        }
                        out.extend_from_slice(
                            sse_event(
                                "content_block_delta",
                                &json!({
                                    "type": "content_block_delta",
                                    "index": self.thinking_idx,
                                    "delta": {"type": "thinking_delta", "thinking": reasoning}
                                }),
                            )
                            .as_bytes(),
                        );
                    }
                }
            }
            // content → text block (closes an open thinking block first).
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                if !content.is_empty() {
                    semantic = true;
                    if self.thinking_open {
                        self.close_thinking(out);
                    }
                    self.text_accum.push_str(content);
                    if !self.text_open {
                        self.text_idx = self.next_block_idx;
                        self.next_block_idx += 1;
                        self.text_open = true;
                        out.extend_from_slice(
                            sse_event(
                                "content_block_start",
                                &json!({
                                    "type": "content_block_start",
                                    "index": self.text_idx,
                                    "content_block": {"type": "text", "text": ""}
                                }),
                            )
                            .as_bytes(),
                        );
                    }
                    out.extend_from_slice(
                        sse_event(
                            "content_block_delta",
                            &json!({
                                "type": "content_block_delta",
                                "index": self.text_idx,
                                "delta": {"type": "text_delta", "text": content}
                            }),
                        )
                        .as_bytes(),
                    );
                }
            }
            // tool_calls deltas (parallel-capable, fragmented JSON args).
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for tool_call in tool_calls {
                    semantic = true;
                    if self.thinking_open {
                        self.close_thinking(out);
                    }
                    let index = tool_call.get("index").and_then(Value::as_i64).unwrap_or(0);
                    if !self.tools.contains_key(&index) {
                        let name = tool_call
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        let block_idx = self.next_block_idx;
                        self.next_block_idx += 1;
                        self.tools.insert(
                            index,
                            ToolSlot {
                                id: tool_call
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                                name,
                                arguments: String::new(),
                                block_idx,
                                started: false,
                            },
                        );
                    }
                    let slot = self.tools.get_mut(&index).expect("just inserted");
                    if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
                        if !id.is_empty() {
                            slot.id = id.to_owned();
                        }
                    }
                    if let Some(name) = tool_call.pointer("/function/name").and_then(Value::as_str)
                    {
                        if !name.is_empty() {
                            slot.name = name.to_owned();
                        }
                    }
                    if let Some(arguments) = tool_call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                    {
                        slot.arguments.push_str(arguments);
                    }
                    // Start the block as soon as we know name or args so the
                    // client sees tool progress early; the first delta
                    // carries the name in practice.
                    if !slot.started {
                        slot.started = true;
                        let id = slot.id.clone();
                        let name = slot.name.clone();
                        out.extend_from_slice(
                            sse_event(
                                "content_block_start",
                                &json!({
                                    "type": "content_block_start",
                                    "index": slot.block_idx,
                                    "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
                                }),
                            )
                            .as_bytes(),
                        );
                    }
                    if let Some(arguments) = tool_call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                    {
                        if !arguments.is_empty() {
                            let block_idx = slot.block_idx;
                            out.extend_from_slice(
                                sse_event(
                                    "content_block_delta",
                                    &json!({
                                        "type": "content_block_delta",
                                        "index": block_idx,
                                        "delta": {"type": "input_json_delta", "partial_json": arguments}
                                    }),
                                )
                                .as_bytes(),
                            );
                        }
                    }
                }
            }
        }
        semantic
    }

    fn close_thinking(&mut self, out: &mut Vec<u8>) {
        self.thinking_open = false;
        out.extend_from_slice(
            sse_event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": self.thinking_idx}),
            )
            .as_bytes(),
        );
    }

    /// Emit all trailing events: close open blocks, message_delta, message_stop.
    /// Appends to `out`; safe to call once per stream.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        let stop_reason = stop_reason_from_finish(self.finish_reason.as_deref());
        self.finish_with_stop(out, stop_reason);
    }

    fn finish_with_stop(&mut self, out: &mut Vec<u8>, stop_reason: &str) {
        if self.thinking_open {
            self.close_thinking(out);
        }
        if self.text_open {
            self.text_open = false;
            out.extend_from_slice(
                sse_event(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": self.text_idx}),
                )
                .as_bytes(),
            );
        }
        for slot in self.tools.values_mut() {
            if slot.started {
                slot.started = false;
                out.extend_from_slice(
                    sse_event(
                        "content_block_stop",
                        &json!({"type": "content_block_stop", "index": slot.block_idx}),
                    )
                    .as_bytes(),
                );
            }
        }
        let usage_json = match self.usage {
            Some((_, output, _)) => json!({"output_tokens": output}),
            None => json!({"output_tokens": 0}),
        };
        out.extend_from_slice(
            sse_event(
                "message_delta",
                &json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null},
                    "usage": usage_json
                }),
            )
            .as_bytes(),
        );
        out.extend_from_slice(
            sse_event("message_stop", &json!({"type": "message_stop"})).as_bytes(),
        );
    }

    /// Assemble the full non-stream Anthropic Message JSON from accumulated
    /// state (thinking only when exposed, tool inputs parsed from JSON).
    pub fn nonstream_response(&self) -> Value {
        let accumulated = self.accumulated();
        let mut content: Vec<Value> = Vec::new();
        if self.expose_thinking && !accumulated.reasoning.is_empty() {
            content.push(
                json!({"type": "thinking", "thinking": accumulated.reasoning, "signature": ""}),
            );
        }
        if !accumulated.text.is_empty() {
            content.push(json!({"type": "text", "text": accumulated.text}));
        }
        for tool_call in &accumulated.tool_calls {
            let input = serde_json::from_str::<Value>(&tool_call.arguments)
                .ok()
                .filter(|v| v.is_object())
                .unwrap_or_else(|| json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": tool_call.id,
                "name": tool_call.name,
                "input": input
            }));
        }
        let mut message = Map::new();
        message.insert("id".into(), json!(self.msg_id));
        message.insert("type".into(), json!("message"));
        message.insert("role".into(), json!("assistant"));
        message.insert("content".into(), Value::Array(content));
        message.insert(
            "model".into(),
            json!(self.model.clone().unwrap_or_default()),
        );
        message.insert(
            "stop_reason".into(),
            json!(stop_reason_from_finish(self.finish_reason.as_deref())),
        );
        message.insert("stop_sequence".into(), Value::Null);
        if let Some((input, output, cached)) = self.usage {
            message.insert(
                "usage".into(),
                json!({
                    "input_tokens": input,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": cached,
                    "output_tokens": output
                }),
            );
        } else {
            message.insert(
                "usage".into(),
                json!({
                    "input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "output_tokens": 0
                }),
            );
        }
        Value::Object(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(delta: Value, finish: Option<&str>, usage: Option<Value>) -> Value {
        let mut chunk = json!({
            "id": "c1",
            "model": "deepseek-flash",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        });
        if let Some(usage) = usage {
            chunk["usage"] = usage;
        }
        chunk
    }

    fn parse_events(raw: &[u8]) -> Vec<(String, Value)> {
        let text = String::from_utf8_lossy(raw);
        let mut events = Vec::new();
        for block in text.split("\n\n") {
            if block.trim().is_empty() {
                continue;
            }
            let mut name = String::new();
            for line in block.lines() {
                if let Some(event) = line.strip_prefix("event: ") {
                    name = event.to_owned();
                } else if let Some(data) = line.strip_prefix("data: ") {
                    if data != "[DONE]" {
                        events.push((name.clone(), serde_json::from_str(data).unwrap()));
                    }
                }
            }
        }
        events
    }

    #[test]
    fn text_stream_lifecycle() {
        let mut converter = StreamConverter::new(false);
        let mut out = Vec::new();
        // A role-only delta carries no semantic progress.
        assert!(!converter.feed_chunk(&chunk(json!({"role": "assistant"}), None, None), &mut out));
        assert!(converter.feed_chunk(&chunk(json!({"content": "Hel"}), None, None), &mut out));
        assert!(converter.feed_chunk(&chunk(json!({"content": "lo"}), None, None), &mut out));
        assert!(converter.feed_chunk(&chunk(json!({}), Some("stop"), None), &mut out));
        converter.finish(&mut out);
        let events = parse_events(&out);
        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        let text_delta = events
            .iter()
            .filter(|(n, _)| n == "content_block_delta")
            .map(|(_, v)| v["delta"]["text"].as_str().unwrap().to_owned())
            .collect::<String>();
        assert_eq!(text_delta, "Hello");
        let stop = events.iter().find(|(n, _)| n == "message_delta").unwrap();
        assert_eq!(stop.1["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn reasoning_mapped_to_thinking_only_when_requested() {
        let sse = [
            chunk(json!({"reasoning_content": "think A"}), None, None),
            chunk(json!({"reasoning_content": "think B"}), None, None),
            chunk(json!({"content": "answer"}), None, None),
            chunk(json!({}), Some("stop"), None),
        ];
        // Requested: thinking block emitted and closed before text.
        let mut converter = StreamConverter::new(true);
        let mut out = Vec::new();
        for chunk in &sse {
            converter.feed_chunk(chunk, &mut out);
        }
        converter.finish(&mut out);
        let events = parse_events(&out);
        let starts: Vec<(usize, &str)> = events
            .iter()
            .filter(|(n, _)| n == "content_block_start")
            .map(|(_, v)| {
                (
                    v["index"].as_u64().unwrap() as usize,
                    v["content_block"]["type"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(starts, vec![(0, "thinking"), (1, "text")]);
        let thinking: String = events
            .iter()
            .filter(|(n, _)| n == "content_block_delta")
            .filter(|(_, v)| v["delta"]["type"] == "thinking_delta")
            .map(|(_, v)| v["delta"]["thinking"].as_str().unwrap())
            .collect();
        assert_eq!(thinking, "think Athink B");
        let stops: Vec<u64> = events
            .iter()
            .filter(|(n, _)| n == "content_block_stop")
            .map(|(_, v)| v["index"].as_u64().unwrap())
            .collect();
        assert_eq!(stops, vec![0, 1]);
        // Non-stream view keeps both blocks.
        let message = converter.nonstream_response();
        assert_eq!(message["content"][0]["type"], "thinking");
        assert_eq!(message["content"][1]["type"], "text");

        // Not requested: no thinking events at all.
        let mut converter = StreamConverter::new(false);
        let mut out = Vec::new();
        for chunk_value in &sse {
            converter.feed_chunk(chunk_value, &mut out);
        }
        converter.finish(&mut out);
        let rendered = String::from_utf8_lossy(&out);
        assert!(!rendered.contains("thinking"));
        assert!(!rendered.contains("think A"));
        // But the shadow accumulator still holds the reasoning.
        assert_eq!(converter.accumulated().reasoning, "think Athink B");
    }

    #[test]
    fn tool_stream_with_parallel_calls_and_fragments() {
        let mut converter = StreamConverter::new(false);
        let mut out = Vec::new();
        converter.feed_chunk(&chunk(json!({}), None, None), &mut out);
        converter.feed_chunk(
            &chunk(
                json!({"tool_calls": [
                    {"index": 0, "id": "call_a", "type": "function",
                     "function": {"name": "Read", "arguments": "{\"pa"}},
                    {"index": 1, "id": "call_b", "type": "function",
                     "function": {"name": "Edit", "arguments": "{\"pa"}}
                ]}),
                None,
                None,
            ),
            &mut out,
        );
        converter.feed_chunk(
            &chunk(
                json!({"tool_calls": [
                    {"index": 0, "function": {"arguments": "th\": \"a.rs\"}"}},
                    {"index": 1, "function": {"arguments": "th\": \"b.rs\"}"}}
                ]}),
                None,
                None,
            ),
            &mut out,
        );
        converter.feed_chunk(&chunk(json!({}), Some("tool_calls"), None), &mut out);
        converter.finish(&mut out);
        let events = parse_events(&out);
        let starts: Vec<(usize, String, String)> = events
            .iter()
            .filter(|(n, _)| n == "content_block_start")
            .map(|(_, v)| {
                (
                    v["index"].as_u64().unwrap() as usize,
                    v["content_block"]["id"].as_str().unwrap().to_owned(),
                    v["content_block"]["name"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0], (0usize, "call_a".to_owned(), "Read".to_owned()));
        assert_eq!(starts[1], (1usize, "call_b".to_owned(), "Edit".to_owned()));
        // Arguments are routed to the right block indices.
        let args_by_block: std::collections::BTreeMap<u64, String> = events
            .iter()
            .filter(|(n, _)| n == "content_block_delta")
            .map(|(_, v)| {
                (
                    v["index"].as_u64().unwrap(),
                    v["delta"]["partial_json"].as_str().unwrap().to_owned(),
                )
            })
            .fold(
                std::collections::BTreeMap::new(),
                |mut map, (index, arg)| {
                    map.entry(index).or_default().push_str(&arg);
                    map
                },
            );
        assert_eq!(args_by_block[&0], "{\"path\": \"a.rs\"}");
        assert_eq!(args_by_block[&1], "{\"path\": \"b.rs\"}");
        let message_delta = events.iter().find(|(n, _)| n == "message_delta").unwrap();
        assert_eq!(message_delta.1["delta"]["stop_reason"], "tool_use");
        // Non-stream assembly parses tool inputs.
        let message = converter.nonstream_response();
        assert_eq!(message["content"][0]["type"], "tool_use");
        assert_eq!(message["content"][0]["input"]["path"], "a.rs");
        assert_eq!(message["content"][1]["input"]["path"], "b.rs");
    }

    #[test]
    fn finish_reason_length_maps_to_max_tokens() {
        let mut converter = StreamConverter::new(false);
        let mut out = Vec::new();
        converter.feed_chunk(
            &chunk(json!({"content": "abc"}), Some("length"), None),
            &mut out,
        );
        converter.finish(&mut out);
        let events = parse_events(&out);
        assert_eq!(
            events.iter().find(|(n, _)| n == "message_delta").unwrap().1["delta"]["stop_reason"],
            "max_tokens"
        );
    }

    #[test]
    fn usage_chunk_without_choices_is_recorded() {
        let mut converter = StreamConverter::new(false);
        let mut out = Vec::new();
        converter.feed_chunk(&chunk(json!({"content": "hi"}), None, None), &mut out);
        converter.feed_chunk(
            &json!({
                "id": "c1",
                "choices": [],
                "usage": {"prompt_tokens": 10, "completion_tokens": 4,
                          "prompt_tokens_details": {"cached_tokens": 6}}
            }),
            &mut out,
        );
        converter.finish(&mut out);
        let accumulated = converter.accumulated();
        assert_eq!(accumulated.usage, Some((4, 4, 6)));
    }
}
