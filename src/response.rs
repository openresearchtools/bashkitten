//! Ordered assistant-response assembly. Pi's Responses parser keeps a distinct
//! block for each output item; text/reasoning signatures belong to that block,
//! not to the response as a whole. See pinned openai-responses-shared.ts.

use crate::agent::{
    AgentMessage, ContentBlock, ModelCost, OrderedJsonValue, Usage, calculate_cost,
};
use crate::providers::{NormalizedUsage, ProviderEvent, StopReason};
use chrono::Utc;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct PendingToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

pub struct ResponseAssembly {
    blocks: BTreeMap<u64, ContentBlock>,
    pub calls: BTreeMap<u64, PendingToolCall>,
    response_id: Option<String>,
    usage: NormalizedUsage,
    pub stop_reason: StopReason,
    raw_stop_reason: Option<String>,
    error: Option<String>,
    timestamp: i64,
}

impl Default for ResponseAssembly {
    fn default() -> Self {
        Self {
            blocks: BTreeMap::new(),
            calls: BTreeMap::new(),
            response_id: None,
            usage: NormalizedUsage::default(),
            stop_reason: StopReason::Stop,
            raw_stop_reason: None,
            error: None,
            timestamp: Utc::now().timestamp_millis(),
        }
    }
}

impl ResponseAssembly {
    pub fn fail(&mut self, error: String) {
        self.stop_reason = StopReason::Error;
        self.error = Some(error);
    }

    pub fn abort(&mut self) {
        self.stop_reason = StopReason::Aborted;
        self.error = Some("Request aborted".into());
    }

    pub fn push(&mut self, event: ProviderEvent) -> Option<Value> {
        match event {
            ProviderEvent::Start { response_id } => self.response_id = response_id,
            ProviderEvent::TextDelta { index, delta } => {
                if let ContentBlock::Text { text, .. } = self
                    .blocks
                    .entry(index)
                    .or_insert_with(|| ContentBlock::text(""))
                {
                    text.push_str(&delta);
                }
                return Some(json!({"type":"assistant_delta","index":index,"delta":delta}));
            }
            ProviderEvent::TextDone {
                index,
                text,
                text_signature,
            } => {
                let block = self
                    .blocks
                    .entry(index)
                    .or_insert_with(|| ContentBlock::text(""));
                if let ContentBlock::Text {
                    text: value,
                    text_signature: signature,
                } = block
                {
                    if let Some(text) = text {
                        *value = text;
                    }
                    *signature = text_signature;
                }
            }
            ProviderEvent::ThinkingDelta { index, delta } => {
                if let ContentBlock::Thinking { thinking, .. } =
                    self.blocks.entry(index).or_insert_with(empty_thinking)
                {
                    thinking.push_str(&delta);
                }
                return Some(json!({"type":"thinking_delta","index":index,"delta":delta}));
            }
            ProviderEvent::ThinkingDone {
                index,
                encrypted_content,
                ..
            } => {
                if let ContentBlock::Thinking {
                    thinking_signature, ..
                } = self.blocks.entry(index).or_insert_with(empty_thinking)
                {
                    *thinking_signature = encrypted_content;
                }
            }
            ProviderEvent::ToolCallStart { index, id, name } => {
                self.calls.insert(
                    index,
                    PendingToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                    },
                );
                self.tool_block(index);
                return Some(json!({"type":"tool_call_start","index":index,"id":id,"name":name}));
            }
            ProviderEvent::ToolCallDelta {
                index,
                arguments_delta,
            } => {
                if let Some(call) = self.calls.get_mut(&index) {
                    call.arguments.push_str(&arguments_delta);
                }
                self.tool_block(index);
                return Some(
                    json!({"type":"tool_call_delta","index":index,"delta":arguments_delta}),
                );
            }
            ProviderEvent::ToolCallDone {
                index,
                id,
                name,
                arguments,
            } => {
                self.calls.insert(
                    index,
                    PendingToolCall {
                        id,
                        name,
                        arguments,
                    },
                );
                self.tool_block(index);
            }
            ProviderEvent::Usage { usage } => self.usage = usage,
            ProviderEvent::Done { reason, raw_reason } => {
                self.stop_reason = reason;
                self.raw_stop_reason = raw_reason;
            }
        }
        None
    }

    fn tool_block(&mut self, index: u64) {
        if let Some(call) = self.calls.get(&index) {
            self.blocks.insert(
                index,
                ContentBlock::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: OrderedJsonValue::from(
                        serde_json::from_str::<Value>(&call.arguments)
                            .unwrap_or_else(|_| json!({})),
                    ),
                    thought_signature: None,
                    namespace: None,
                },
            );
        }
    }

    pub fn message(&self, provider: &str, model: &str, cost: &ModelCost) -> AgentMessage {
        AgentMessage::Assistant {
            content: self.blocks.values().cloned().collect(),
            api: if provider == "openai-codex" {
                "openai-codex-responses"
            } else {
                "openai-completions"
            }
            .into(),
            provider: provider.into(),
            model: model.into(),
            response_model: None,
            response_id: self.response_id.clone(),
            diagnostics: None,
            usage: normalized_usage(&self.usage, cost),
            stop_reason: match self.stop_reason {
                StopReason::Stop | StopReason::Unknown => crate::agent::StopReason::Stop,
                StopReason::Length => crate::agent::StopReason::Length,
                StopReason::ToolUse => crate::agent::StopReason::ToolUse,
                StopReason::Aborted => crate::agent::StopReason::Aborted,
                StopReason::Error | StopReason::ContentFilter => crate::agent::StopReason::Error,
            },
            deferred: None,
            error_message: self.error.clone(),
            raw_stop_reason: self.raw_stop_reason.clone(),
            end_turn: None,
            timestamp: self.timestamp,
        }
    }
}

fn empty_thinking() -> ContentBlock {
    ContentBlock::Thinking {
        thinking: String::new(),
        thinking_signature: None,
        redacted: None,
    }
}

pub fn normalized_usage(usage: &NormalizedUsage, model_cost: &ModelCost) -> Usage {
    let mut normalized = Usage {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_tokens,
        cache_write: usage.cache_write_tokens,
        cache_write_1h: None,
        reasoning: (usage.reasoning_tokens > 0).then_some(usage.reasoning_tokens),
        total_tokens: usage.total_tokens,
        ..Usage::default()
    };
    calculate_cost(&mut normalized, model_cost);
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_items_keep_each_signature_and_partial_errors() {
        let mut response = ResponseAssembly::default();
        response.push(ProviderEvent::ThinkingDelta {
            index: 0,
            delta: "first thought".into(),
        });
        response.push(ProviderEvent::ThinkingDone {
            index: 0,
            id: None,
            encrypted_content: Some("first-signature".into()),
        });
        response.push(ProviderEvent::TextDone {
            index: 1,
            text: Some("commentary".into()),
            text_signature: Some("commentary-id".into()),
        });
        response.push(ProviderEvent::ThinkingDelta {
            index: 2,
            delta: "second thought".into(),
        });
        response.push(ProviderEvent::ThinkingDone {
            index: 2,
            id: None,
            encrypted_content: Some("second-signature".into()),
        });
        response.push(ProviderEvent::TextDelta {
            index: 3,
            delta: "unfinished answer".into(),
        });
        response.abort();
        let value = serde_json::to_value(response.message(
            "openai-codex",
            "fixture",
            &ModelCost::default(),
        ))
        .unwrap();
        assert_eq!(value["content"].as_array().unwrap().len(), 4);
        assert_eq!(value["content"][0]["thinkingSignature"], "first-signature");
        assert_eq!(value["content"][1]["text"], "commentary");
        assert_eq!(value["content"][1]["textSignature"], "commentary-id");
        assert_eq!(value["content"][2]["thinkingSignature"], "second-signature");
        assert_eq!(value["content"][3]["text"], "unfinished answer");
        assert_eq!(value["stopReason"], "aborted");
    }
}
