//! Pinned Codex event mapping and shared Responses stream state machine.
use crate::providers::{NormalizedUsage, ProviderEvent as Event, StopReason};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}
fn list(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn field(v: &Value, k: &str) -> String {
    v.get(k)
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_else(|| "undefined".into())
}
struct Slot {
    index: usize,
    partial: Option<String>,
    custom: Option<CustomInput>,
}
struct CustomInput {
    input: String,
    buffer: String,
    started: bool,
    closed: bool,
}
impl CustomInput {
    fn append(&mut self, next: &str, close: bool) -> Result<Option<String>> {
        if self.closed {
            if close && next == self.buffer {
                return Ok(None);
            }
            bail!("grammar tool input for property \"input\" changed after it was closed");
        }
        let Some(delta) = next.strip_prefix(&self.buffer) else {
            bail!("grammar tool input for property \"input\" changed non-monotonically");
        };
        if !close && delta.is_empty() {
            return Ok(None);
        }
        let escaped = serde_json::to_string(delta)?;
        let mut output = if self.started {
            String::new()
        } else {
            "{\"input\":\"".into()
        };
        self.started = true;
        output.push_str(&escaped[1..escaped.len() - 1]);
        self.buffer = next.into();
        if close {
            output.push_str("\"}");
            self.closed = true;
        }
        self.input = next.into();
        Ok(Some(output))
    }
}
pub struct State {
    pub done: bool,
    model: String,
    service_tier: Option<String>,
    slots: HashMap<Option<u64>, Slot>,
    blocks: Vec<Value>,
    reasoning: HashMap<String, usize>,
}
impl Default for State {
    fn default() -> Self {
        Self::new("gpt-5.5".into())
    }
}
impl State {
    pub fn new(model: String) -> Self {
        Self {
            done: false,
            model,
            service_tier: None,
            slots: HashMap::new(),
            blocks: Vec::new(),
            reasoning: HashMap::new(),
        }
    }
    pub fn with_service_tier(mut self, tier: Option<String>) -> Self {
        self.service_tier = tier;
        self
    }
    pub fn with_blocks(model: String, blocks: Vec<Value>) -> Self {
        Self {
            blocks,
            ..Self::new(model)
        }
    }
    fn emit(&self, index: usize, events: &mut Vec<Event>) {
        events.push(Event::BlockContent {
            index: index as u64,
            block: self.blocks[index].clone(),
        });
    }
    fn create(&mut self, key: Option<u64>, item: &Value, events: &mut Vec<Event>) -> Option<usize> {
        let index = self.blocks.len();
        let mut partial = None;
        let mut custom = None;
        let mut block = match s(&item["type"]) {
            "reasoning" => json!({"type":"thinking","thinking":""}),
            "message" => json!({"type":"text","text":""}),
            "function_call" | "custom_tool_call" => {
                let is_custom = item["type"] == "custom_tool_call";
                let id = format!("{}|{}", field(item, "call_id"), field(item, "id"));
                let arguments = if is_custom {
                    let input = s(&item["input"]).to_owned();
                    custom = Some(CustomInput {
                        input: input.clone(),
                        buffer: String::new(),
                        started: false,
                        closed: false,
                    });
                    json!({"input":input})
                } else {
                    partial = Some(s(&item["arguments"]).into());
                    json!({})
                };
                events.push(Event::ToolCallStart {
                    index: index as u64,
                    id: id.clone(),
                    name: s(&item["name"]).into(),
                });
                json!({"type":"toolCall","id":id,"name":item["name"],"arguments":arguments})
            }
            _ => return None,
        };
        if block["type"] == "toolCall"
            && let Some(namespace) = item.get("namespace")
        {
            block["namespace"] = namespace.clone();
        }
        self.blocks.push(block);
        self.slots.insert(
            key,
            Slot {
                index,
                partial,
                custom,
            },
        );
        self.emit(index, events);
        Some(index)
    }
    pub fn push(&mut self, event: &Value) -> Result<Vec<Event>> {
        let kind = s(&event["type"]);
        let key = event["output_index"].as_u64();
        let mut events = Vec::new();
        match kind {
            "error" => {
                let code = event["code"]
                    .as_str()
                    .or_else(|| event["error"]["code"].as_str())
                    .unwrap_or_default();
                let message = event["message"]
                    .as_str()
                    .or_else(|| event["error"]["message"].as_str())
                    .filter(|v| !v.is_empty());
                let detail = message
                    .or_else(|| (!code.is_empty()).then_some(code))
                    .map(str::to_owned)
                    .unwrap_or_else(|| event.to_string());
                bail!("Codex error: {detail}");
            }
            "response.failed" => bail!(
                "{}",
                event["response"]["error"]["message"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .unwrap_or("Codex response failed")
            ),
            "response.created" => events.push(Event::ResponseInfo {
                response_id: event["response"]["id"].as_str().map(str::to_owned),
                end_turn: None,
                cost_multiplier: None,
            }),
            "response.output_item.added" => {
                self.create(key, &event["item"], &mut events);
            }
            "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning_summary_part.done"
            | "response.output_text.delta"
            | "response.refusal.delta" => {
                if let Some(slot) = self.slots.get(&key) {
                    let thinking = kind.starts_with("response.reasoning");
                    let expected = if thinking { "thinking" } else { "text" };
                    if self.blocks[slot.index]["type"] == expected {
                        let delta = if kind == "response.reasoning_summary_part.done" {
                            "\n\n"
                        } else {
                            s(&event["delta"])
                        };
                        let value = s(&self.blocks[slot.index][expected]).to_owned() + delta;
                        self.blocks[slot.index][expected] = json!(value);
                        events.push(if thinking {
                            Event::ThinkingDelta {
                                index: slot.index as u64,
                                delta: delta.into(),
                            }
                        } else {
                            Event::TextDelta {
                                index: slot.index as u64,
                                delta: delta.into(),
                            }
                        });
                    }
                }
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                if let Some(slot) = self.slots.get_mut(&key)
                    && let Some(partial) = &mut slot.partial
                {
                    let delta = if kind.ends_with(".delta") {
                        let delta = s(&event["delta"]).to_owned();
                        partial.push_str(&delta);
                        Some(delta)
                    } else {
                        let next = s(&event["arguments"]);
                        let delta = next
                            .strip_prefix(partial.as_str())
                            .filter(|v| !v.is_empty())
                            .map(str::to_owned);
                        *partial = next.into();
                        delta
                    };
                    self.blocks[slot.index]["arguments"] =
                        crate::streaming_json::parse_streaming_json(partial);
                    let index = slot.index;
                    if let Some(arguments_delta) = delta {
                        events.push(Event::ToolCallDelta {
                            index: index as u64,
                            arguments_delta,
                        });
                    }
                    self.emit(index, &mut events);
                }
            }
            "response.custom_tool_call_input.delta" | "response.custom_tool_call_input.done" => {
                if let Some(slot) = self.slots.get_mut(&key)
                    && let Some(input) = &mut slot.custom
                {
                    let next = if kind.ends_with(".delta") {
                        input.input.clone() + s(&event["delta"])
                    } else {
                        s(&event["input"]).into()
                    };
                    if let Some(arguments_delta) = input.append(&next, kind.ends_with(".done"))? {
                        events.push(Event::ToolCallDelta {
                            index: slot.index as u64,
                            arguments_delta,
                        });
                    }
                    self.blocks[slot.index]["arguments"] = json!({"input":next});
                    let index = slot.index;
                    self.emit(index, &mut events);
                }
            }
            "response.output_item.done" => {
                let item = &event["item"];
                let index = self
                    .slots
                    .get(&key)
                    .map(|v| v.index)
                    .or_else(|| self.create(key, item, &mut events));
                if let Some(index) = index {
                    let block = &mut self.blocks[index];
                    let mut finished = false;
                    match s(&item["type"]) {
                        "reasoning" if block["type"] == "thinking" => {
                            let joined = |key: &str| {
                                list(&item[key])
                                    .iter()
                                    .map(|v| s(&v["text"]))
                                    .collect::<Vec<_>>()
                                    .join("\n\n")
                            };
                            let summary = joined("summary");
                            let content = joined("content");
                            if !summary.is_empty() {
                                block["thinking"] = json!(summary);
                            } else if !content.is_empty() {
                                block["thinking"] = json!(content);
                            }
                            block["thinkingSignature"] = json!(item.to_string());
                            self.reasoning.insert(field(item, "id"), index);
                            finished = true;
                        }
                        "message" if block["type"] == "text" => {
                            block["text"] = json!(
                                list(&item["content"])
                                    .iter()
                                    .map(|v| if v["type"] == "output_text" {
                                        s(&v["text"])
                                    } else {
                                        s(&v["refusal"])
                                    })
                                    .collect::<String>()
                            );
                            let mut signature = json!({"v":1});
                            if let Some(id) = item.get("id") {
                                signature["id"] = id.clone();
                            }
                            if let Some(phase) = item.get("phase").filter(|v| !v.is_null()) {
                                signature["phase"] = phase.clone();
                            }
                            block["textSignature"] = json!(signature.to_string());
                            finished = true;
                        }
                        "function_call" if block["type"] == "toolCall" => {
                            if let Some(partial) =
                                self.slots.get(&key).and_then(|v| v.partial.as_deref())
                            {
                                let arguments = item["arguments"]
                                    .as_str()
                                    .filter(|v| !v.is_empty())
                                    .or_else(|| (!partial.is_empty()).then_some(partial))
                                    .unwrap_or("{}");
                                block["arguments"] =
                                    crate::streaming_json::parse_streaming_json(arguments);
                                if let Some(namespace) = item.get("namespace") {
                                    block["namespace"] = namespace.clone();
                                }
                                events.push(Event::ToolCallDone {
                                    index: index as u64,
                                    id: s(&block["id"]).into(),
                                    name: s(&block["name"]).into(),
                                    arguments: block["arguments"].to_string(),
                                });
                                finished = true;
                            }
                        }
                        "custom_tool_call" if block["type"] == "toolCall" => {
                            if let Some(input) =
                                self.slots.get_mut(&key).and_then(|v| v.custom.as_mut())
                            {
                                let next =
                                    item["input"].as_str().unwrap_or(&input.input).to_owned();
                                if let Some(arguments_delta) = input.append(&next, true)? {
                                    events.push(Event::ToolCallDelta {
                                        index: index as u64,
                                        arguments_delta,
                                    });
                                }
                                block["arguments"] = json!({"input":next});
                                if let Some(namespace) = item.get("namespace") {
                                    block["namespace"] = namespace.clone();
                                }
                                events.push(Event::ToolCallDone {
                                    index: index as u64,
                                    id: s(&block["id"]).into(),
                                    name: s(&block["name"]).into(),
                                    arguments: block["arguments"].to_string(),
                                });
                                finished = true;
                            }
                        }
                        _ => {}
                    }
                    if finished {
                        self.emit(index, &mut events);
                        self.slots.remove(&key);
                    }
                }
            }
            "response.done" | "response.completed" | "response.incomplete" => {
                let response = &event["response"];
                for item in list(&response["output"]) {
                    if item["type"] == "reasoning"
                        && !s(&item["encrypted_content"]).is_empty()
                        && let Some(index) = self.reasoning.get(s(&item["id"])).copied()
                    {
                        let mut stored: Value =
                            serde_json::from_str(s(&self.blocks[index]["thinkingSignature"]))?;
                        if s(&stored["encrypted_content"]).is_empty() {
                            stored["encrypted_content"] = item["encrypted_content"].clone();
                            self.blocks[index]["thinkingSignature"] = json!(stored.to_string());
                            self.emit(index, &mut events);
                        }
                    }
                }
                let reported = response["service_tier"].as_str();
                let tier = if reported == Some("default")
                    && matches!(self.service_tier.as_deref(), Some("flex" | "priority"))
                {
                    self.service_tier.as_deref()
                } else {
                    reported.or(self.service_tier.as_deref())
                };
                let multiplier = match tier.unwrap_or_default() {
                    "flex" => 0.5,
                    "priority" => {
                        if self.model == "gpt-5.5" {
                            2.5
                        } else {
                            2.0
                        }
                    }
                    _ => 1.0,
                };
                events.push(Event::ResponseInfo {
                    response_id: response["id"]
                        .as_str()
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned),
                    end_turn: response["end_turn"].as_bool(),
                    cost_multiplier: Some(multiplier),
                });
                if !response["usage"].is_null() {
                    events.push(Event::Usage {
                        usage: normalize_usage(&response["usage"]),
                    });
                }
                let status = response["status"].as_str().filter(|v| {
                    matches!(
                        *v,
                        "completed"
                            | "incomplete"
                            | "failed"
                            | "cancelled"
                            | "queued"
                            | "in_progress"
                    )
                });
                let incomplete = response["incomplete_details"]["reason"]
                    .as_str()
                    .filter(|v| !v.is_empty());
                let raw_reason = incomplete
                    .map(|v| format!("{}.{v}", status.unwrap_or("undefined")))
                    .or_else(|| status.map(str::to_owned));
                let (mut reason, error) = match (status, incomplete) {
                    (Some("incomplete"), Some("max_output_tokens")) => (StopReason::Length, None),
                    (Some("incomplete"), reason) => (
                        StopReason::Error,
                        Some(
                            reason
                                .map(|r| format!("Response incomplete: {r}"))
                                .unwrap_or_else(|| {
                                    "Response incomplete without a provider reason".into()
                                }),
                        ),
                    ),
                    (Some("failed" | "cancelled"), _) => {
                        (StopReason::Error, Some("An unknown error occurred".into()))
                    }
                    _ => (StopReason::Stop, None),
                };
                if reason == StopReason::Stop && self.blocks.iter().any(|b| b["type"] == "toolCall")
                {
                    reason = StopReason::ToolUse;
                }
                if error.is_none() {
                    // The pinned successful path leaves unfinished scratch
                    // buffers intact; only its error catch strips them.
                    for slot in self.slots.values() {
                        if let Some(partial) = &slot.partial {
                            self.blocks[slot.index]["partialJson"] = json!(partial);
                        }
                        if let Some(input) = &slot.custom {
                            self.blocks[slot.index]["customInput"] = json!({"property":"input","jsonBuffer":{"input":input.buffer,"started":input.started,"closed":input.closed}});
                        }
                    }
                    for slot in self.slots.values() {
                        self.emit(slot.index, &mut events);
                    }
                }
                events.push(Event::Done { reason, raw_reason });
                if let Some(message) = error {
                    events.push(Event::Failure { message });
                }
                self.done = true;
            }
            _ => {}
        }
        Ok(events)
    }
}
pub fn normalize_usage(usage: &Value) -> NormalizedUsage {
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let cached = n(&usage["input_tokens_details"]["cached_tokens"]);
    let written = n(&usage["input_tokens_details"]["cache_write_tokens"]);
    NormalizedUsage {
        input_tokens: n(&usage["input_tokens"])
            .saturating_sub(cached)
            .saturating_sub(written),
        output_tokens: n(&usage["output_tokens"]),
        cache_read_tokens: cached,
        cache_write_tokens: written,
        reasoning_tokens: n(&usage["output_tokens_details"]["reasoning_tokens"]),
        reasoning_present: true,
        total_tokens: n(&usage["total_tokens"]),
    }
}

// Pi's Codex SSE reader intentionally frames on LF LF and ignores event: fields.
// Buffer bytes until a whole frame so split UTF-8 sequences survive chunking.
#[derive(Default)]
pub struct Decoder {
    bytes: Vec<u8>,
}
impl Decoder {
    pub fn feed(&mut self, chunk: &[u8], eof: bool) -> Vec<Result<Value>> {
        self.bytes.extend_from_slice(chunk);
        if eof && !self.bytes.is_empty() {
            self.bytes.extend_from_slice(b"\n\n");
        }
        let mut values = Vec::new();
        while let Some(index) = self.bytes.windows(2).position(|v| v == b"\n\n") {
            let chunk = self.bytes.drain(..index + 2).collect::<Vec<_>>();
            let text = String::from_utf8_lossy(&chunk[..index]);
            let data = text
                .split('\n')
                .filter_map(|v| v.strip_prefix("data:"))
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\n");
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let value = serde_json::from_str(data)
                .map_err(|error| anyhow::anyhow!("Invalid Codex SSE JSON: {error}"));
            let failed = value.is_err();
            values.push(value);
            if failed {
                break;
            }
        }
        values
    }
}
