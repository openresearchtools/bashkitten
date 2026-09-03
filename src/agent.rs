//! Pure session-worker state and Pi-compatible compaction/usage logic.
//!
//! Behavioral reference: Pi v0.84.4 at commit
//! `b79e4cc834970cca69daebffab7df1da7d1e52c4`.
//!
//! The compaction prompt text and the algorithms ported here are derived from Pi,
//! Copyright (c) 2025 Mario Zechner, used under the MIT License:
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::error::Error;
use std::fmt;

pub const DEFAULT_RESERVE_TOKENS: u64 = 16_384;
pub const DEFAULT_KEEP_RECENT_TOKENS: u64 = 20_000;
pub const ESTIMATED_IMAGE_CHARS: u64 = 4_800;
pub const TOOL_RESULT_MAX_CHARS: usize = 2_000;
pub const MAX_SEGMENT_NUMBER: u32 = 999_999;

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = r#"You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary."#;

pub const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

pub const UPDATE_SUMMARIZATION_INSTRUCTIONS: &str = r#"Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

pub const UPDATE_SUMMARIZATION_PROMPT: &str = r#"The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

pub const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = r#"This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.

Summarize the prefix to provide context for the retained suffix:

## Original Request
[What did the user ask for in this turn?]

## Early Progress
- [Key decisions and work done in the prefix]

## Context for Suffix
- [Information needed to understand the retained recent work]

Be concise. Focus on what's needed to understand the kept suffix."#;

pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    pub total_tokens: u64,
    pub cost: UsageCost,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostRates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostTier {
    #[serde(flatten)]
    pub rates: CostRates,
    pub input_tokens_above: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    #[serde(flatten)]
    pub rates: CostRates,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<CostTier>,
}

/// Pi's context accounting prefers a provider's native total when it is non-zero.
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens != 0 {
        usage.total_tokens
    } else {
        usage
            .input
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write)
    }
}

/// Apply Pi's request-wide pricing tier and per-million-token arithmetic.
pub fn calculate_cost(usage: &mut Usage, model_cost: &ModelCost) -> UsageCost {
    let input_tokens = usage
        .input
        .saturating_add(usage.cache_read)
        .saturating_add(usage.cache_write);
    let mut rates = model_cost.rates;
    let mut matched_threshold: Option<u64> = None;

    for tier in &model_cost.tiers {
        if input_tokens > tier.input_tokens_above
            && matched_threshold.is_none_or(|threshold| tier.input_tokens_above > threshold)
        {
            rates = tier.rates;
            matched_threshold = Some(tier.input_tokens_above);
        }
    }

    let long_write = usage.cache_write_1h.unwrap_or(0);
    let short_write = usage.cache_write.saturating_sub(long_write);
    usage.cost.input = rates.input / 1_000_000.0 * usage.input as f64;
    usage.cost.output = rates.output / 1_000_000.0 * usage.output as f64;
    usage.cost.cache_read = rates.cache_read / 1_000_000.0 * usage.cache_read as f64;
    usage.cost.cache_write = (rates.cache_write * short_write as f64
        + rates.input * 2.0 * long_write as f64)
        / 1_000_000.0;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    usage.cost.clone()
}

pub fn combine_usage(first: &Usage, second: &Usage) -> Usage {
    let mut combined = Usage {
        input: first.input.saturating_add(second.input),
        output: first.output.saturating_add(second.output),
        cache_read: first.cache_read.saturating_add(second.cache_read),
        cache_write: first.cache_write.saturating_add(second.cache_write),
        cache_write_1h: (first.cache_write_1h.is_some() || second.cache_write_1h.is_some()).then(
            || {
                first
                    .cache_write_1h
                    .unwrap_or(0)
                    .saturating_add(second.cache_write_1h.unwrap_or(0))
            },
        ),
        reasoning: (first.reasoning.is_some() || second.reasoning.is_some()).then(|| {
            first
                .reasoning
                .unwrap_or(0)
                .saturating_add(second.reasoning.unwrap_or(0))
        }),
        total_tokens: first.total_tokens.saturating_add(second.total_tokens),
        cost: UsageCost::default(),
    };
    combined.cost.input = first.cost.input + second.cost.input;
    combined.cost.output = first.cost.output + second.cost.output;
    combined.cost.cache_read = first.cost.cache_read + second.cost.cache_read;
    combined.cost.cache_write = first.cost.cache_write + second.cost.cache_write;
    combined.cost.total = first.cost.total + second.cost.total;
    combined
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAiPromptTokenDetails {
    pub cached_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAiCompletionTokenDetails {
    pub reasoning_tokens: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAiCompletionsUsage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub prompt_cache_hit_tokens: Option<u64>,
    pub prompt_tokens_details: Option<OpenAiPromptTokenDetails>,
    pub completion_tokens_details: Option<OpenAiCompletionTokenDetails>,
}

/// Normalize the usage object emitted by Pi's `openai-completions` provider.
pub fn normalize_openai_completions_usage(
    raw: &OpenAiCompletionsUsage,
    model_cost: &ModelCost,
) -> Usage {
    let prompt_tokens = raw.prompt_tokens.unwrap_or(0);
    let cache_read = raw
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
        .or(raw.prompt_cache_hit_tokens)
        .or(raw.cached_tokens)
        .unwrap_or(0);
    let cache_write = raw
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cache_write_tokens)
        .unwrap_or(0);
    let output = raw.completion_tokens.unwrap_or(0);
    let input = prompt_tokens.saturating_sub(cache_read.saturating_add(cache_write));
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: Some(
            raw.completion_tokens_details
                .as_ref()
                .and_then(|details| details.reasoning_tokens)
                .unwrap_or(0),
        ),
        total_tokens: input
            .saturating_add(output)
            .saturating_add(cache_read)
            .saturating_add(cache_write),
        cost: UsageCost::default(),
    };
    calculate_cost(&mut usage, model_cost);
    usage
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAiResponsesInputDetails {
    pub cached_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAiResponsesOutputDetails {
    pub reasoning_tokens: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAiResponsesUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub input_tokens_details: Option<OpenAiResponsesInputDetails>,
    pub output_tokens_details: Option<OpenAiResponsesOutputDetails>,
}

/// Normalize the terminal usage object emitted by Pi's Responses implementation.
pub fn normalize_openai_responses_usage(
    raw: &OpenAiResponsesUsage,
    model_cost: &ModelCost,
) -> Usage {
    let cache_read = raw
        .input_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
        .unwrap_or(0);
    let cache_write = raw
        .input_tokens_details
        .as_ref()
        .and_then(|details| details.cache_write_tokens)
        .unwrap_or(0);
    let mut usage = Usage {
        input: raw
            .input_tokens
            .unwrap_or(0)
            .saturating_sub(cache_read.saturating_add(cache_write)),
        output: raw.output_tokens.unwrap_or(0),
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: Some(
            raw.output_tokens_details
                .as_ref()
                .and_then(|details| details.reasoning_tokens)
                .unwrap_or(0),
        ),
        total_tokens: raw.total_tokens.unwrap_or(0),
        cost: UsageCost::default(),
    };
    calculate_cost(&mut usage, model_cost);
    usage
}

/// JSON value that preserves object insertion order without requiring
/// `serde_json`'s optional `preserve_order` feature. Pi's compaction transcript
/// uses JavaScript `Object.entries`, so top-level and nested argument order are
/// model-visible behavior.
#[derive(Clone, Debug, PartialEq)]
pub enum OrderedJsonValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<OrderedJsonValue>),
    Object(Vec<(String, OrderedJsonValue)>),
}

impl OrderedJsonValue {
    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(entries) => entries
                .iter()
                .find_map(|(candidate, value)| (candidate == key).then_some(value)),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }
}

impl From<Value> for OrderedJsonValue {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(value),
            Value::Number(value) => Self::Number(value),
            Value::String(value) => Self::String(value),
            Value::Array(values) => Self::Array(values.into_iter().map(Self::from).collect()),
            Value::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Self::from(value)))
                    .collect(),
            ),
        }
    }
}

impl Serialize for OrderedJsonValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => value.serialize(serializer),
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(value)?;
                }
                sequence.end()
            }
            Self::Object(entries) => {
                let mut map = serializer.serialize_map(Some(entries.len()))?;
                for (key, value) in entries {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for OrderedJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OrderedJsonVisitor;

        impl<'de> Visitor<'de> for OrderedJsonVisitor {
            type Value = OrderedJsonValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Null)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Null)
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(OrderedJsonValue::Number)
                    .ok_or_else(|| E::custom("non-finite number is not valid JSON"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(OrderedJsonValue::String(value))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(OrderedJsonValue::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some((key, value)) = map.next_entry()? {
                    entries.push((key, value));
                }
                Ok(OrderedJsonValue::Object(entries))
            }
        }

        deserializer.deserialize_any(OrderedJsonVisitor)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(rename = "textSignature", skip_serializing_if = "Option::is_none")]
        text_signature: Option<String>,
    },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(rename = "thinkingSignature", skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
    #[serde(rename = "toolCall")]
    ToolCall {
        id: String,
        name: String,
        arguments: OrderedJsonValue,
        #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            text_signature: None,
        }
    }

    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl MessageContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Blocks(vec![ContentBlock::text(text)])
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum StopReason {
    #[serde(rename = "pending")]
    Pending,
    #[default]
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "toolUse")]
    ToolUse,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "aborted")]
    Aborted,
    #[serde(rename = "deferred")]
    Deferred,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryKind {
    Steer,
    Queue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role")]
pub enum AgentMessage {
    #[serde(rename = "user")]
    User {
        content: MessageContent,
        timestamp: i64,
        #[serde(rename = "sourceSession", skip_serializing_if = "Option::is_none")]
        source_session: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        delivery: Option<DeliveryKind>,
    },
    #[serde(rename = "assistant")]
    Assistant {
        content: Vec<ContentBlock>,
        #[serde(default)]
        api: String,
        #[serde(default)]
        provider: String,
        #[serde(default)]
        model: String,
        #[serde(rename = "responseModel", skip_serializing_if = "Option::is_none")]
        response_model: Option<String>,
        #[serde(rename = "responseId", skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        diagnostics: Option<Value>,
        usage: Usage,
        #[serde(rename = "stopReason")]
        stop_reason: StopReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        deferred: Option<Value>,
        #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
        #[serde(rename = "rawStopReason", skip_serializing_if = "Option::is_none")]
        raw_stop_reason: Option<String>,
        #[serde(rename = "endTurn", skip_serializing_if = "Option::is_none")]
        end_turn: Option<bool>,
        timestamp: i64,
    },
    #[serde(rename = "toolResult")]
    ToolResult {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        content: MessageContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(rename = "addedToolNames", skip_serializing_if = "Option::is_none")]
        added_tool_names: Option<Vec<String>>,
        #[serde(rename = "isError")]
        is_error: bool,
        timestamp: i64,
    },
    #[serde(rename = "bashExecution")]
    BashExecution {
        command: String,
        output: String,
        #[serde(rename = "exitCode")]
        exit_code: Option<i32>,
        cancelled: bool,
        truncated: bool,
        #[serde(rename = "fullOutputPath", skip_serializing_if = "Option::is_none")]
        full_output_path: Option<String>,
        #[serde(rename = "excludeFromContext", skip_serializing_if = "Option::is_none")]
        exclude_from_context: Option<bool>,
        timestamp: i64,
    },
    #[serde(rename = "custom")]
    Custom {
        #[serde(rename = "customType")]
        custom_type: String,
        content: MessageContent,
        display: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        timestamp: i64,
    },
    #[serde(rename = "branchSummary")]
    BranchSummary {
        summary: String,
        #[serde(rename = "fromId")]
        from_id: String,
        timestamp: i64,
    },
    #[serde(rename = "compactionSummary")]
    CompactionSummary {
        summary: String,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
        timestamp: i64,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    #[serde(default)]
    pub read_files: Vec<String>,
    #[serde(default)]
    pub modified_files: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: Option<String>,
    pub timestamp: String,
    #[serde(flatten)]
    pub kind: SessionEntryKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntryKind {
    Message {
        message: AgentMessage,
    },
    ThinkingLevelChange {
        #[serde(rename = "thinkingLevel")]
        thinking_level: String,
    },
    ModelChange {
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    Compaction {
        summary: String,
        #[serde(rename = "firstKeptEntryId")]
        first_kept_entry_id: String,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(rename = "fromHook", skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
    },
    BranchSummary {
        #[serde(rename = "fromId")]
        from_id: String,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(rename = "fromHook", skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
    },
    Custom {
        #[serde(rename = "customType")]
        custom_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    CustomMessage {
        #[serde(rename = "customType")]
        custom_type: String,
        content: MessageContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        display: bool,
    },
    Label {
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    SessionInfo {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl SessionEntry {
    pub fn context_messages(&self) -> Vec<AgentMessage> {
        match &self.kind {
            SessionEntryKind::Message { message } => vec![message.clone()],
            SessionEntryKind::CustomMessage {
                custom_type,
                content,
                details,
                display,
            } => vec![AgentMessage::Custom {
                custom_type: custom_type.clone(),
                content: content.clone(),
                display: *display,
                details: details.clone(),
                timestamp: parse_timestamp_millis(&self.timestamp),
            }],
            SessionEntryKind::BranchSummary {
                from_id, summary, ..
            } if !summary.is_empty() => vec![AgentMessage::BranchSummary {
                summary: summary.clone(),
                from_id: from_id.clone(),
                timestamp: parse_timestamp_millis(&self.timestamp),
            }],
            SessionEntryKind::Compaction {
                summary,
                tokens_before,
                ..
            } => vec![AgentMessage::CompactionSummary {
                summary: summary.clone(),
                tokens_before: *tokens_before,
                timestamp: parse_timestamp_millis(&self.timestamp),
            }],
            _ => Vec::new(),
        }
    }
}

fn parse_timestamp_millis(timestamp: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|value| value.timestamp_millis())
        .unwrap_or(0)
}

pub fn build_session_path<'a>(
    entries: &'a [SessionEntry],
    leaf_id: Option<&str>,
) -> Vec<&'a SessionEntry> {
    let by_id: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.id.as_str(), index))
        .collect();
    let mut current = match leaf_id {
        Some(id) => by_id
            .get(id)
            .copied()
            .or_else(|| entries.len().checked_sub(1)),
        None => entries.len().checked_sub(1),
    };
    let mut path = Vec::new();

    while let Some(index) = current {
        let entry = &entries[index];
        path.push(entry);
        current = entry
            .parent_id
            .as_deref()
            .and_then(|parent| by_id.get(parent).copied());
    }
    path.reverse();
    path
}

pub fn build_context_entries<'a>(
    entries: &'a [SessionEntry],
    leaf_id: Option<&str>,
) -> Vec<&'a SessionEntry> {
    let path = build_session_path(entries, leaf_id);
    let Some((compaction_index, compaction)) = path
        .iter()
        .enumerate()
        .rev()
        .find(|(_, entry)| matches!(entry.kind, SessionEntryKind::Compaction { .. }))
    else {
        return path;
    };
    let SessionEntryKind::Compaction {
        first_kept_entry_id,
        ..
    } = &compaction.kind
    else {
        unreachable!();
    };

    let mut context = vec![*compaction];
    let mut found_first_kept = false;
    for entry in &path[..compaction_index] {
        if entry.id == *first_kept_entry_id {
            found_first_kept = true;
        }
        if found_first_kept {
            context.push(*entry);
        }
    }
    context.extend(path[compaction_index + 1..].iter().copied());
    context
}

pub fn build_session_context(entries: &[SessionEntry], leaf_id: Option<&str>) -> Vec<AgentMessage> {
    build_context_entries(entries, leaf_id)
        .into_iter()
        .flat_map(SessionEntry::context_messages)
        .collect()
}

fn utf16_len(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}

fn content_chars(content: &MessageContent) -> u64 {
    match content {
        MessageContent::Text(text) => utf16_len(text),
        MessageContent::Blocks(blocks) => blocks.iter().fold(0_u64, |total, block| {
            total.saturating_add(match block {
                ContentBlock::Text { text, .. } => utf16_len(text),
                ContentBlock::Image { .. } => ESTIMATED_IMAGE_CHARS,
                _ => 0,
            })
        }),
    }
}

fn json_stringify_len(value: &OrderedJsonValue) -> u64 {
    serde_json::to_string(value)
        .map(|json| utf16_len(&json))
        .unwrap_or(0)
}

pub fn estimate_tokens(message: &AgentMessage) -> u64 {
    let chars = match message {
        AgentMessage::User { content, .. }
        | AgentMessage::ToolResult { content, .. }
        | AgentMessage::Custom { content, .. } => content_chars(content),
        AgentMessage::Assistant { content, .. } => content.iter().fold(0_u64, |total, block| {
            total.saturating_add(match block {
                ContentBlock::Text { text, .. } => utf16_len(text),
                ContentBlock::Thinking { thinking, .. } => utf16_len(thinking),
                ContentBlock::ToolCall {
                    name, arguments, ..
                } => utf16_len(name).saturating_add(json_stringify_len(arguments)),
                ContentBlock::Image { .. } => 0,
            })
        }),
        AgentMessage::BashExecution {
            command, output, ..
        } => utf16_len(command).saturating_add(utf16_len(output)),
        AgentMessage::BranchSummary { summary, .. }
        | AgentMessage::CompactionSummary { summary, .. } => utf16_len(summary),
    };
    chars.saturating_add(3) / 4
}

fn valid_assistant_usage(message: &AgentMessage) -> Option<&Usage> {
    match message {
        AgentMessage::Assistant {
            usage, stop_reason, ..
        } if !matches!(stop_reason, StopReason::Aborted | StopReason::Error)
            && calculate_context_tokens(usage) > 0 =>
        {
            Some(usage)
        }
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextUsageEstimate {
    pub tokens: u64,
    pub usage_tokens: u64,
    pub trailing_tokens: u64,
    pub last_usage_index: Option<usize>,
}

pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextUsageEstimate {
    let usage_info = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| valid_assistant_usage(message).map(|usage| (index, usage)));

    let Some((index, usage)) = usage_info else {
        let estimated = messages.iter().map(estimate_tokens).sum();
        return ContextUsageEstimate {
            tokens: estimated,
            usage_tokens: 0,
            trailing_tokens: estimated,
            last_usage_index: None,
        };
    };

    let usage_tokens = calculate_context_tokens(usage);
    let trailing_tokens = messages[index + 1..].iter().map(estimate_tokens).sum();
    ContextUsageEstimate {
        tokens: usage_tokens.saturating_add(trailing_tokens),
        usage_tokens,
        trailing_tokens,
        last_usage_index: Some(index),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
        }
    }
}

pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: CompactionSettings,
) -> bool {
    settings.enabled
        && (settings.reserve_tokens > context_window
            || context_tokens > context_window - settings.reserve_tokens)
}

fn is_cut_point_message(message: &AgentMessage) -> bool {
    !matches!(message, AgentMessage::ToolResult { .. })
}

fn is_turn_start_message(message: &AgentMessage) -> bool {
    matches!(
        message,
        AgentMessage::User { .. }
            | AgentMessage::BashExecution { .. }
            | AgentMessage::Custom { .. }
            | AgentMessage::BranchSummary { .. }
            | AgentMessage::CompactionSummary { .. }
    )
}

fn is_turn_start_entry(entry: &SessionEntry) -> bool {
    !matches!(entry.kind, SessionEntryKind::Compaction { .. })
        && entry.context_messages().iter().any(is_turn_start_message)
}

fn valid_cut_points(entries: &[SessionEntry], start_index: usize, end_index: usize) -> Vec<usize> {
    (start_index..end_index)
        .filter(|index| {
            let entry = &entries[*index];
            !matches!(entry.kind, SessionEntryKind::Compaction { .. })
                && entry.context_messages().iter().any(is_cut_point_message)
        })
        .collect()
}

pub fn find_turn_start_index(
    entries: &[SessionEntry],
    entry_index: usize,
    start_index: usize,
) -> Option<usize> {
    (start_index..=entry_index)
        .rev()
        .find(|index| is_turn_start_entry(&entries[*index]))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CutPointResult {
    pub first_kept_entry_index: usize,
    pub turn_start_index: Option<usize>,
    pub is_split_turn: bool,
}

pub fn find_cut_point(
    entries: &[SessionEntry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u64,
) -> CutPointResult {
    assert!(start_index <= end_index && end_index <= entries.len());
    let cut_points = valid_cut_points(entries, start_index, end_index);
    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: None,
            is_split_turn: false,
        };
    }

    let mut accumulated_tokens = 0_u64;
    let mut cut_index = cut_points[0];
    for index in (start_index..end_index).rev() {
        let message_tokens: u64 = entries[index]
            .context_messages()
            .iter()
            .map(estimate_tokens)
            .sum();
        if message_tokens == 0 {
            continue;
        }
        accumulated_tokens = accumulated_tokens.saturating_add(message_tokens);
        if accumulated_tokens >= keep_recent_tokens {
            if let Some(found) = cut_points.iter().find(|cut| **cut >= index) {
                cut_index = *found;
            }
            break;
        }
    }

    while cut_index > start_index {
        let previous = &entries[cut_index - 1];
        if matches!(previous.kind, SessionEntryKind::Compaction { .. })
            || !previous.context_messages().is_empty()
        {
            break;
        }
        cut_index -= 1;
    }

    let starts_turn = is_turn_start_entry(&entries[cut_index]);
    let turn_start_index = (!starts_turn)
        .then(|| find_turn_start_index(entries, cut_index, start_index))
        .flatten();
    CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !starts_turn && turn_start_index.is_some(),
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileOperations {
    pub read: BTreeSet<String>,
    pub written: BTreeSet<String>,
    pub edited: BTreeSet<String>,
}

pub fn extract_file_operations(message: &AgentMessage, file_ops: &mut FileOperations) {
    let AgentMessage::Assistant { content, .. } = message else {
        return;
    };
    for block in content {
        let ContentBlock::ToolCall {
            name, arguments, ..
        } = block
        else {
            continue;
        };
        let Some(path) = arguments.get("path").and_then(OrderedJsonValue::as_str) else {
            continue;
        };
        match name.as_str() {
            "read" => {
                file_ops.read.insert(path.to_owned());
            }
            "write" => {
                file_ops.written.insert(path.to_owned());
            }
            "edit" => {
                file_ops.edited.insert(path.to_owned());
            }
            _ => {}
        }
    }
}

pub fn compute_file_lists(file_ops: &FileOperations) -> CompactionDetails {
    let modified: BTreeSet<String> = file_ops.edited.union(&file_ops.written).cloned().collect();
    CompactionDetails {
        read_files: file_ops.read.difference(&modified).cloned().collect(),
        modified_files: modified.into_iter().collect(),
    }
}

pub fn format_file_operations(details: &CompactionDetails) -> String {
    let mut sections = Vec::new();
    if !details.read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            details.read_files.join("\n")
        ));
    }
    if !details.modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            details.modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompactionPreparation {
    pub first_kept_entry_id: String,
    pub messages_to_summarize: Vec<AgentMessage>,
    pub turn_prefix_messages: Vec<AgentMessage>,
    pub is_split_turn: bool,
    pub tokens_before: u64,
    pub previous_summary: Option<String>,
    pub file_operations: FileOperations,
    pub settings: CompactionSettings,
}

pub fn prepare_compaction(
    path_entries: &[SessionEntry],
    settings: CompactionSettings,
) -> Option<CompactionPreparation> {
    if path_entries
        .last()
        .is_some_and(|entry| matches!(entry.kind, SessionEntryKind::Compaction { .. }))
    {
        return None;
    }

    let previous_compaction_index = path_entries
        .iter()
        .rposition(|entry| matches!(entry.kind, SessionEntryKind::Compaction { .. }));
    let mut previous_summary = None;
    let mut boundary_start = 0;
    let mut file_operations = FileOperations::default();

    if let Some(index) = previous_compaction_index
        && let SessionEntryKind::Compaction {
            summary,
            first_kept_entry_id,
            details,
            from_hook,
            ..
        } = &path_entries[index].kind
    {
        previous_summary = Some(summary.clone());
        boundary_start = path_entries
            .iter()
            .position(|entry| entry.id == *first_kept_entry_id)
            .unwrap_or(index + 1);
        if !from_hook.unwrap_or(false)
            && let Some(details) = details
            && let Ok(details) = serde_json::from_value::<CompactionDetails>(details.clone())
        {
            file_operations.read.extend(details.read_files);
            file_operations.edited.extend(details.modified_files);
        }
    }

    let boundary_end = path_entries.len();
    let tokens_before = estimate_context_tokens(&build_session_context(path_entries, None)).tokens;
    let cut_point = find_cut_point(
        path_entries,
        boundary_start,
        boundary_end,
        settings.keep_recent_tokens,
    );
    let first_kept_entry = path_entries.get(cut_point.first_kept_entry_index)?;
    let history_end = if cut_point.is_split_turn {
        cut_point.turn_start_index?
    } else {
        cut_point.first_kept_entry_index
    };

    let messages_to_summarize: Vec<_> = path_entries[boundary_start..history_end]
        .iter()
        .filter_map(message_for_compaction)
        .collect();
    let turn_prefix_messages: Vec<_> = if cut_point.is_split_turn {
        path_entries[history_end..cut_point.first_kept_entry_index]
            .iter()
            .filter_map(message_for_compaction)
            .collect()
    } else {
        Vec::new()
    };
    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }

    for message in messages_to_summarize.iter().chain(&turn_prefix_messages) {
        extract_file_operations(message, &mut file_operations);
    }

    Some(CompactionPreparation {
        first_kept_entry_id: first_kept_entry.id.clone(),
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary,
        file_operations,
        settings,
    })
}

fn message_for_compaction(entry: &SessionEntry) -> Option<AgentMessage> {
    if matches!(entry.kind, SessionEntryKind::Compaction { .. }) {
        None
    } else {
        entry.context_messages().into_iter().next()
    }
}

fn content_text(content: &MessageContent, separator: &str) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(separator),
    }
}

pub fn bash_execution_to_text(message: &AgentMessage) -> Option<String> {
    let AgentMessage::BashExecution {
        command,
        output,
        exit_code,
        cancelled,
        truncated,
        full_output_path,
        ..
    } = message
    else {
        return None;
    };
    let mut text = format!("Ran `{command}`\n");
    if output.is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str(&format!("```\n{output}\n```"));
    }
    if *cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if exit_code.is_some_and(|code| code != 0) {
        text.push_str(&format!(
            "\n\nCommand exited with code {}",
            exit_code.unwrap()
        ));
    }
    if *truncated && let Some(path) = full_output_path {
        text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
    }
    Some(text)
}

fn truncate_for_summary(text: &str, max_utf16_units: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max_utf16_units {
        return text.to_owned();
    }
    let truncated = units.len() - max_utf16_units;
    let prefix = String::from_utf16_lossy(&units[..max_utf16_units]);
    format!("{prefix}\n\n[... {truncated} more characters truncated]")
}

fn json_argument_pairs(arguments: &OrderedJsonValue) -> String {
    let OrderedJsonValue::Object(object) = arguments else {
        return String::new();
    };
    object
        .iter()
        .map(|(key, value)| {
            format!(
                "{key}={}",
                serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn serialize_conversation(messages: &[AgentMessage]) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match message {
            AgentMessage::User { content, .. } => {
                let content = content_text(content, "");
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            AgentMessage::Assistant { content, .. } => {
                let mut thinking = Vec::new();
                let mut text = Vec::new();
                let mut has_text = false;
                let mut tool_calls = Vec::new();
                for block in content {
                    match block {
                        ContentBlock::Thinking {
                            thinking: value, ..
                        } => thinking.push(value.as_str()),
                        ContentBlock::Text { text: value, .. } => {
                            has_text = true;
                            text.push(value.as_str());
                        }
                        ContentBlock::ToolCall {
                            name, arguments, ..
                        } => tool_calls.push(format!("{name}({})", json_argument_pairs(arguments))),
                        ContentBlock::Image { .. } => {}
                    }
                }
                if !thinking.is_empty() {
                    parts.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
                }
                if has_text {
                    parts.push(format!("[Assistant]: {}", text.join("\n")));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            AgentMessage::ToolResult { content, .. } => {
                let content = content_text(content, "");
                if !content.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&content, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
            AgentMessage::BashExecution {
                exclude_from_context,
                ..
            } => {
                if !exclude_from_context.unwrap_or(false)
                    && let Some(text) = bash_execution_to_text(message)
                {
                    parts.push(format!("[User]: {text}"));
                }
            }
            AgentMessage::Custom { content, .. } => {
                let content = content_text(content, "");
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            AgentMessage::BranchSummary { summary, .. } => parts.push(format!(
                "[User]: {BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}"
            )),
            AgentMessage::CompactionSummary { summary, .. } => parts.push(format!(
                "[User]: {COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"
            )),
        }
    }
    parts.join("\n\n")
}

pub fn build_summarization_prompt(
    messages: &[AgentMessage],
    previous_summary: Option<&str>,
    custom_instructions: Option<&str>,
) -> String {
    let previous_summary = previous_summary.filter(|summary| !summary.is_empty());
    let mut base = if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT.to_owned()
    } else {
        SUMMARIZATION_PROMPT.to_owned()
    };
    if let Some(instructions) = custom_instructions.filter(|value| !value.is_empty()) {
        base.push_str("\n\nAdditional focus: ");
        base.push_str(instructions);
    }

    let mut prompt = format!(
        "<conversation>\n{}\n</conversation>\n\n",
        serialize_conversation(messages)
    );
    if let Some(previous) = previous_summary {
        prompt.push_str(&format!(
            "<previous-summary>\n{previous}\n</previous-summary>\n\n"
        ));
    }
    prompt.push_str(&base);
    prompt
}

pub fn build_turn_prefix_prompt(messages: &[AgentMessage]) -> String {
    format!(
        "<conversation>\n{}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}",
        serialize_conversation(messages)
    )
}

pub fn summary_max_tokens(reserve_tokens: u64, model_max_tokens: u64) -> u64 {
    let budget = reserve_tokens.saturating_mul(4) / 5;
    if model_max_tokens > 0 {
        budget.min(model_max_tokens)
    } else {
        budget
    }
}

pub fn turn_prefix_max_tokens(reserve_tokens: u64, model_max_tokens: u64) -> u64 {
    let budget = reserve_tokens / 2;
    if model_max_tokens > 0 {
        budget.min(model_max_tokens)
    } else {
        budget
    }
}

pub fn merge_split_turn_summary(history: Option<&str>, turn_prefix: &str) -> String {
    format!(
        "{}\n\n---\n\n**Turn Context (split turn):**\n\n{turn_prefix}",
        history.unwrap_or("No prior history.")
    )
}

pub fn summarization_failure(
    stop_reason: StopReason,
    error_message: Option<&str>,
    label: &str,
) -> Option<String> {
    match stop_reason {
        StopReason::Error => Some(format!(
            "{label} failed: {}",
            error_message.unwrap_or("Unknown error")
        )),
        StopReason::Length => Some(format!(
            "{label} failed: generation hit the token cap and the summary is incomplete"
        )),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingMessageQueue<T> {
    mode: QueueMode,
    messages: VecDeque<T>,
}

impl<T> PendingMessageQueue<T> {
    pub fn new(mode: QueueMode) -> Self {
        Self {
            mode,
            messages: VecDeque::new(),
        }
    }

    pub fn mode(&self) -> QueueMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: QueueMode) {
        self.mode = mode;
    }

    pub fn enqueue(&mut self, message: T) {
        self.messages.push_back(message);
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    pub fn drain(&mut self) -> Vec<T> {
        match self.mode {
            QueueMode::All => self.messages.drain(..).collect(),
            QueueMode::OneAtATime => self.messages.pop_front().into_iter().collect(),
        }
    }
}

impl<T> Default for PendingMessageQueue<T> {
    fn default() -> Self {
        Self::new(QueueMode::OneAtATime)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrainedQueue {
    Steering,
    FollowUp,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueueDrain<T> {
    pub queue: DrainedQueue,
    pub messages: Vec<T>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentQueues<T> {
    pub steering: PendingMessageQueue<T>,
    pub follow_up: PendingMessageQueue<T>,
}

impl<T> Default for AgentQueues<T> {
    fn default() -> Self {
        Self {
            steering: PendingMessageQueue::default(),
            follow_up: PendingMessageQueue::default(),
        }
    }
}

impl<T> AgentQueues<T> {
    pub fn has_messages(&self) -> bool {
        !self.steering.is_empty() || !self.follow_up.is_empty()
    }

    pub fn clear(&mut self) {
        self.steering.clear();
        self.follow_up.clear();
    }

    /// Poll at a safe model boundary. Steering always wins; follow-up is eligible
    /// only when the loop would otherwise stop (including an idle-session wake).
    pub fn drain_at_boundary(&mut self, would_otherwise_stop: bool) -> Option<QueueDrain<T>> {
        let steering = self.steering.drain();
        if !steering.is_empty() {
            return Some(QueueDrain {
                queue: DrainedQueue::Steering,
                messages: steering,
            });
        }
        if would_otherwise_stop {
            let follow_up = self.follow_up.drain();
            if !follow_up.is_empty() {
                return Some(QueueDrain {
                    queue: DrainedQueue::FollowUp,
                    messages: follow_up,
                });
            }
        }
        None
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct SegmentNumber(u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SegmentNumberError;

impl fmt::Display for SegmentNumberError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("session segment number must be in 1..=999999")
    }
}

impl Error for SegmentNumberError {}

impl SegmentNumber {
    pub fn new(number: u32) -> Result<Self, SegmentNumberError> {
        if (1..=MAX_SEGMENT_NUMBER).contains(&number) {
            Ok(Self(number))
        } else {
            Err(SegmentNumberError)
        }
    }

    pub fn get(self) -> u32 {
        self.0
    }

    pub fn file_name(self) -> String {
        format!("{:06}.jsonl", self.0)
    }

    pub fn next(self) -> Result<Self, SegmentNumberError> {
        Self::new(self.0.checked_add(1).ok_or(SegmentNumberError)?)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompletedTurnCommit {
    pub segment: SegmentNumber,
    pub entries: Vec<SessionEntry>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SegmentRotationPlan {
    /// Segment that must first receive and flush all settled pre-compaction entries.
    pub closing_segment: SegmentNumber,
    /// Exactly the next monotonically numbered segment.
    pub opening_segment: SegmentNumber,
    /// Entries not yet persisted in the closing segment.
    pub pre_compaction_entries: Vec<SessionEntry>,
    /// The Pi-compatible compaction record for logical history/accounting.
    pub compaction_entry: SessionEntry,
    /// Active context to encode into the newly opened segment.
    pub compacted_context: Vec<AgentMessage>,
}

impl SegmentRotationPlan {
    pub fn new(
        closing_segment: SegmentNumber,
        pre_compaction_entries: Vec<SessionEntry>,
        compaction_entry: SessionEntry,
        compacted_context: Vec<AgentMessage>,
    ) -> Result<Self, SegmentNumberError> {
        Ok(Self {
            closing_segment,
            opening_segment: closing_segment.next()?,
            pre_compaction_entries,
            compaction_entry,
            compacted_context,
        })
    }
}

/// Storage boundary implemented by the session worker, not by provider code.
///
/// `rotate_after_compaction` must flush `pre_compaction_entries`, close the old
/// segment, atomically create exactly `opening_segment` with mode 0600, encode the
/// compaction checkpoint plus `compacted_context`, and leave older segments
/// immutable. The hook reports success only after file data is flushed.
pub trait SegmentPersistence {
    type Error;

    fn flush_completed_turn(&mut self, commit: &CompletedTurnCommit) -> Result<(), Self::Error>;
    fn rotate_after_compaction(&mut self, plan: &SegmentRotationPlan) -> Result<(), Self::Error>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
}

impl UsageTotals {
    pub fn add(&mut self, usage: &Usage) {
        self.input = self.input.saturating_add(usage.input);
        self.output = self.output.saturating_add(usage.output);
        self.cache_read = self.cache_read.saturating_add(usage.cache_read);
        self.cache_write = self.cache_write.saturating_add(usage.cache_write);
        self.cost += usage.cost.total;
    }

    pub fn total_tokens(self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }
}

pub fn session_usage_totals(entries: &[SessionEntry]) -> UsageTotals {
    let mut totals = UsageTotals::default();
    for entry in entries {
        match &entry.kind {
            SessionEntryKind::Message {
                message: AgentMessage::Assistant { usage, .. },
            }
            | SessionEntryKind::Message {
                message:
                    AgentMessage::ToolResult {
                        usage: Some(usage), ..
                    },
            }
            | SessionEntryKind::Compaction {
                usage: Some(usage), ..
            }
            | SessionEntryKind::BranchSummary {
                usage: Some(usage), ..
            } => totals.add(usage),
            _ => {}
        }
    }
    totals
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageCostBreakdownEntry {
    pub key: String,
    pub cost: f64,
    pub tokens: u64,
}

pub fn usage_cost_breakdown(entries: &[SessionEntry]) -> Vec<UsageCostBreakdownEntry> {
    let mut by_key: BTreeMap<String, UsageTotals> = BTreeMap::new();
    for entry in entries {
        let item = match &entry.kind {
            SessionEntryKind::Message {
                message:
                    AgentMessage::Assistant {
                        provider,
                        model,
                        response_model,
                        usage,
                        ..
                    },
            } => Some((
                format!(
                    "{provider}/{}",
                    response_model.as_deref().unwrap_or(model.as_str())
                ),
                usage,
            )),
            SessionEntryKind::Message {
                message:
                    AgentMessage::ToolResult {
                        usage: Some(usage), ..
                    },
            }
            | SessionEntryKind::Compaction {
                usage: Some(usage), ..
            }
            | SessionEntryKind::BranchSummary {
                usage: Some(usage), ..
            } => Some(("Tools/summaries".to_owned(), usage)),
            _ => None,
        };
        if let Some((key, usage)) = item {
            by_key.entry(key).or_default().add(usage);
        }
    }
    let mut result: Vec<_> = by_key
        .into_iter()
        .map(|(key, totals)| UsageCostBreakdownEntry {
            key,
            cost: totals.cost,
            tokens: totals.total_tokens(),
        })
        .filter(|entry| entry.cost > 0.0 || entry.tokens > 0)
        .collect();
    result.sort_by(|left, right| right.cost.total_cmp(&left.cost));
    result
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CurrentContextUsage {
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}

pub fn current_context_usage(
    branch_entries: &[SessionEntry],
    active_messages: &[AgentMessage],
    context_window: u64,
) -> Option<CurrentContextUsage> {
    if context_window == 0 {
        return None;
    }
    if let Some(compaction_index) = branch_entries
        .iter()
        .rposition(|entry| matches!(entry.kind, SessionEntryKind::Compaction { .. }))
    {
        let has_post_compaction_usage =
            branch_entries[compaction_index + 1..]
                .iter()
                .rev()
                .any(|entry| match &entry.kind {
                    SessionEntryKind::Message { message } => {
                        valid_assistant_usage(message).is_some()
                    }
                    _ => false,
                });
        if !has_post_compaction_usage {
            return Some(CurrentContextUsage {
                tokens: None,
                context_window,
                percent: None,
            });
        }
    }
    let tokens = estimate_context_tokens(active_messages).tokens;
    Some(CurrentContextUsage {
        tokens: Some(tokens),
        context_window,
        percent: Some(tokens as f64 / context_window as f64 * 100.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(total_tokens: u64) -> Usage {
        Usage {
            total_tokens,
            ..Usage::default()
        }
    }

    fn user(id: &str, text: &str, parent: Option<&str>) -> SessionEntry {
        SessionEntry {
            id: id.to_owned(),
            parent_id: parent.map(str::to_owned),
            timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
            kind: SessionEntryKind::Message {
                message: AgentMessage::User {
                    content: MessageContent::text(text),
                    timestamp: 0,
                    source_session: None,
                    delivery: None,
                },
            },
        }
    }

    fn assistant(id: &str, text: &str, parent: Option<&str>, total_tokens: u64) -> SessionEntry {
        SessionEntry {
            id: id.to_owned(),
            parent_id: parent.map(str::to_owned),
            timestamp: "2026-01-01T00:00:01.000Z".to_owned(),
            kind: SessionEntryKind::Message {
                message: AgentMessage::Assistant {
                    content: vec![ContentBlock::text(text)],
                    api: "openai-responses".to_owned(),
                    provider: "openai-codex".to_owned(),
                    model: "gpt".to_owned(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    usage: usage(total_tokens),
                    stop_reason: StopReason::Stop,
                    deferred: None,
                    error_message: None,
                    raw_stop_reason: None,
                    end_turn: None,
                    timestamp: 0,
                },
            },
        }
    }

    #[test]
    fn compaction_threshold_is_strict() {
        let settings = CompactionSettings::default();
        let threshold = 100_000 - DEFAULT_RESERVE_TOKENS;
        assert!(!should_compact(threshold, 100_000, settings));
        assert!(should_compact(threshold + 1, 100_000, settings));
        assert!(!should_compact(
            u64::MAX,
            100_000,
            CompactionSettings {
                enabled: false,
                ..settings
            }
        ));
        assert!(should_compact(
            0,
            1_000,
            CompactionSettings {
                reserve_tokens: 2_000,
                ..settings
            }
        ));
    }

    #[test]
    fn context_estimate_uses_last_valid_usage_and_trailing_estimates() {
        let messages = vec![
            AgentMessage::User {
                content: MessageContent::text("ignored by usage anchor"),
                timestamp: 0,
                source_session: None,
                delivery: None,
            },
            assistant("a", "answer", None, 100)
                .context_messages()
                .pop()
                .unwrap(),
            AgentMessage::User {
                content: MessageContent::text("12345678"),
                timestamp: 0,
                source_session: None,
                delivery: None,
            },
        ];
        assert_eq!(
            estimate_context_tokens(&messages),
            ContextUsageEstimate {
                tokens: 102,
                usage_tokens: 100,
                trailing_tokens: 2,
                last_usage_index: Some(1),
            }
        );
    }

    #[test]
    fn estimation_matches_pi_utf16_and_image_rules() {
        let emoji = AgentMessage::User {
            content: MessageContent::Text("😀😀".to_owned()),
            timestamp: 0,
            source_session: None,
            delivery: None,
        };
        assert_eq!(estimate_tokens(&emoji), 1);
        let image = AgentMessage::User {
            content: MessageContent::Blocks(vec![ContentBlock::image("ignored", "image/png")]),
            timestamp: 0,
            source_session: None,
            delivery: None,
        };
        assert_eq!(estimate_tokens(&image), 1_200);
    }

    #[test]
    fn cut_point_can_split_turn_but_never_starts_at_tool_result() {
        let tool_result = SessionEntry {
            id: "tool".to_owned(),
            parent_id: Some("assistant".to_owned()),
            timestamp: "2026-01-01T00:00:02.000Z".to_owned(),
            kind: SessionEntryKind::Message {
                message: AgentMessage::ToolResult {
                    tool_call_id: "call".to_owned(),
                    tool_name: "read".to_owned(),
                    content: MessageContent::text("1234"),
                    details: None,
                    usage: None,
                    added_tool_names: None,
                    is_error: false,
                    timestamp: 0,
                },
            },
        };
        let entries = vec![
            user("user", "1234", None),
            assistant("assistant", "12345678", Some("user"), 0),
            tool_result,
        ];
        let cut = find_cut_point(&entries, 0, entries.len(), 2);
        assert_eq!(cut.first_kept_entry_index, 1);
        assert_eq!(cut.turn_start_index, Some(0));
        assert!(cut.is_split_turn);
    }

    #[test]
    fn preparation_uses_pi_cut_and_usage_anchor() {
        let entries = vec![
            user("old-user", "old request", None),
            assistant("old-assistant", "old answer", Some("old-user"), 100),
            user("kept-user", "1234", Some("old-assistant")),
        ];
        let prepared = prepare_compaction(
            &entries,
            CompactionSettings {
                keep_recent_tokens: 1,
                ..CompactionSettings::default()
            },
        )
        .unwrap();
        assert_eq!(prepared.first_kept_entry_id, "kept-user");
        assert_eq!(prepared.messages_to_summarize.len(), 2);
        assert!(prepared.turn_prefix_messages.is_empty());
        assert!(!prepared.is_split_turn);
        assert_eq!(prepared.tokens_before, 101);
    }

    #[test]
    fn queues_are_fifo_one_or_all_and_steering_wins() {
        let mut queues = AgentQueues::default();
        queues.follow_up.enqueue("follow");
        queues.steering.enqueue("steer-1");
        queues.steering.enqueue("steer-2");
        let first = queues.drain_at_boundary(true).unwrap();
        assert_eq!(first.queue, DrainedQueue::Steering);
        assert_eq!(first.messages, vec!["steer-1"]);
        let second = queues.drain_at_boundary(true).unwrap();
        assert_eq!(second.messages, vec!["steer-2"]);
        let third = queues.drain_at_boundary(true).unwrap();
        assert_eq!(third.queue, DrainedQueue::FollowUp);
        assert_eq!(third.messages, vec!["follow"]);

        queues.steering.set_mode(QueueMode::All);
        queues.steering.enqueue("a");
        queues.steering.enqueue("b");
        assert_eq!(
            queues.drain_at_boundary(false).unwrap().messages,
            vec!["a", "b"]
        );
    }

    #[test]
    fn conversation_serialization_matches_pi_labels_and_tool_format() {
        let messages = vec![
            AgentMessage::User {
                content: MessageContent::text("hello"),
                timestamp: 0,
                source_session: None,
                delivery: None,
            },
            AgentMessage::Assistant {
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "plan".to_owned(),
                        thinking_signature: None,
                        redacted: None,
                    },
                    ContentBlock::text("done"),
                    ContentBlock::ToolCall {
                        id: "1".to_owned(),
                        name: "read".to_owned(),
                        arguments: OrderedJsonValue::Object(vec![
                            (
                                "path".to_owned(),
                                OrderedJsonValue::String("/tmp/a".to_owned()),
                            ),
                            ("line".to_owned(), OrderedJsonValue::Number(3.into())),
                        ]),
                        thought_signature: None,
                        namespace: None,
                    },
                ],
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: 0,
            },
        ];
        assert_eq!(
            serialize_conversation(&messages),
            "[User]: hello\n\n[Assistant thinking]: plan\n\n[Assistant]: done\n\n[Assistant tool calls]: read(path=\"/tmp/a\", line=3)"
        );
    }

    #[test]
    fn summary_prompt_wrapper_and_budgets_are_exact() {
        let messages = vec![AgentMessage::User {
            content: MessageContent::text("hello"),
            timestamp: 0,
            source_session: None,
            delivery: None,
        }];
        let prompt = build_summarization_prompt(&messages, Some("old"), Some("paths"));
        assert!(prompt.starts_with("<conversation>\n[User]: hello\n</conversation>\n\n"));
        assert!(prompt.contains("<previous-summary>\nold\n</previous-summary>\n\n"));
        assert!(prompt.ends_with("Additional focus: paths"));
        assert_eq!(summary_max_tokens(16_384, 128_000), 13_107);
        assert_eq!(turn_prefix_max_tokens(16_384, 128_000), 8_192);
    }

    #[test]
    fn completion_usage_normalization_and_tier_selection_match_pi() {
        let costs = ModelCost {
            rates: CostRates {
                input: 1.0,
                output: 2.0,
                cache_read: 0.5,
                cache_write: 1.5,
            },
            tiers: vec![CostTier {
                rates: CostRates {
                    input: 10.0,
                    output: 20.0,
                    cache_read: 5.0,
                    cache_write: 15.0,
                },
                input_tokens_above: 100,
            }],
        };
        let raw = OpenAiCompletionsUsage {
            prompt_tokens: Some(120),
            completion_tokens: Some(10),
            prompt_tokens_details: Some(OpenAiPromptTokenDetails {
                cached_tokens: Some(20),
                cache_write_tokens: Some(0),
            }),
            completion_tokens_details: Some(OpenAiCompletionTokenDetails {
                reasoning_tokens: Some(3),
            }),
            ..OpenAiCompletionsUsage::default()
        };
        let normalized = normalize_openai_completions_usage(&raw, &costs);
        assert_eq!(normalized.input, 100);
        assert_eq!(normalized.cache_read, 20);
        assert_eq!(normalized.output, 10);
        assert_eq!(normalized.reasoning, Some(3));
        assert_eq!(normalized.total_tokens, 130);
        assert!((normalized.cost.total - 0.0013).abs() < 1e-12);
    }

    #[test]
    fn context_usage_is_unknown_immediately_after_compaction() {
        let compaction = SessionEntry {
            id: "compact".to_owned(),
            parent_id: None,
            timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
            kind: SessionEntryKind::Compaction {
                summary: "summary".to_owned(),
                first_kept_entry_id: "kept".to_owned(),
                tokens_before: 50_000,
                details: None,
                usage: None,
                from_hook: None,
            },
        };
        assert_eq!(
            current_context_usage(&[compaction], &[], 100_000),
            Some(CurrentContextUsage {
                tokens: None,
                context_window: 100_000,
                percent: None,
            })
        );
    }

    #[test]
    fn segment_rotation_is_exactly_monotonic() {
        let first = SegmentNumber::new(1).unwrap();
        assert_eq!(first.file_name(), "000001.jsonl");
        assert_eq!(first.next().unwrap().file_name(), "000002.jsonl");
        assert!(SegmentNumber::new(0).is_err());
        assert!(
            SegmentNumber::new(MAX_SEGMENT_NUMBER)
                .unwrap()
                .next()
                .is_err()
        );
    }
}
