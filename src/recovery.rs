//! Pinned Pi recovery policy: ai/src/utils/{retry,overflow}.ts and
//! coding-agent/src/core/agent-session.ts::_checkCompaction.
use crate::agent::{
    self, AgentMessage, CompactionSettings, SessionEntry, SessionEntryKind, StopReason,
};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RetryPolicy {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
}
impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2000,
        }
    }
}
impl RetryPolicy {
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        self.base_delay_ms
            .saturating_mul(2_u64.saturating_pow(attempt.saturating_sub(1)))
    }
}

fn pattern(value: &str) -> Regex {
    RegexBuilder::new(value)
        .case_insensitive(true)
        .build()
        .expect("pinned Pi regex")
}

pub fn is_retryable(message: &AgentMessage) -> bool {
    let AgentMessage::Assistant {
        stop_reason: StopReason::Error,
        error_message: Some(error),
        ..
    } = message
    else {
        return false;
    };
    static EXCLUDED: OnceLock<Regex> = OnceLock::new();
    static RETRYABLE: OnceLock<Regex> = OnceLock::new();
    if EXCLUDED.get_or_init(|| pattern("GoUsageLimitError|FreeUsageLimitError|Monthly usage limit reached|available balance|insufficient_quota|out of budget|quota exceeded|billing")).is_match(error) { return false; }
    RETRYABLE.get_or_init(|| pattern("overloaded|rate.?limit|too many requests|429|500|502|503|504|524|service.?unavailable|server.?error|internal.?error|provider.?returned.?error|exceeded request buffer limit while retrying upstream|network.?error|connection.?error|connection.?refused|connection.?lost|other side closed|fetch failed|getaddrinfo|ENOTFOUND|EAI_AGAIN|upstream.?connect|reset before headers|socket hang up|socket connection was closed|timed? out|timeout|terminated|websocket.?closed|websocket.?error|ended without|stream ended before message_stop|stream ended before a terminal response event|http2 request did not get a response|retry delay|you can retry your request|try your request again|please retry your request|ResourceExhausted")).is_match(error)
}

pub fn is_overflow(message: &AgentMessage, context_window: u64) -> bool {
    let AgentMessage::Assistant {
        stop_reason,
        error_message,
        usage,
        ..
    } = message
    else {
        return false;
    };
    if *stop_reason == StopReason::Error
        && let Some(error) = error_message
    {
        static EXCLUDED: OnceLock<Regex> = OnceLock::new();
        static OVERFLOW: OnceLock<Regex> = OnceLock::new();
        let excluded = EXCLUDED.get_or_init(|| {
            pattern("^(Throttling error|Service unavailable):|rate limit|too many requests")
        });
        let overflow = OVERFLOW.get_or_init(|| pattern(r"prompt is too long|request_too_large|input is too long for requested model|exceeds the context window|exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))|input token count.*exceeds the maximum|maximum prompt length is \d+|reduce the length of the messages|maximum context length is \d+ tokens|exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?|input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)|exceeds the limit of \d+|exceeds the available context size|greater than the context length|context window exceeds limit|exceeded model token limit|too large for model with \d+ maximum context length|prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?|model_context_window_exceeded|prompt too long; exceeded (?:max )?context length|range of input length should be|context[_ ]length[_ ]exceeded|too many tokens|token limit exceeded|^4(?:00|13)\s*(?:status code)?\s*\(no body\)"));
        if !excluded.is_match(error) && overflow.is_match(error) {
            return true;
        }
    }
    let input = usage.input.saturating_add(usage.cache_read);
    context_window > 0
        && ((*stop_reason == StopReason::Stop && input > context_window)
            || (*stop_reason == StopReason::Length
                && usage.output == 0
                && input as f64 >= context_window as f64 * 0.99))
}

pub fn recoverable_length(message: &AgentMessage, desired_max_output: u64) -> bool {
    matches!(message, AgentMessage::Assistant { stop_reason: StopReason::Length, usage, .. }
        if desired_max_output > 0 && usage.output < desired_max_output)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoCompaction {
    pub reason: &'static str,
    pub will_retry: bool,
}

pub fn auto_compaction(
    message: &AgentMessage,
    entries: &[SessionEntry],
    messages: &[AgentMessage],
    model: &crate::models::ModelInfo,
    settings: CompactionSettings,
    skip_aborted: bool,
) -> Option<AutoCompaction> {
    let context_window = model.context_window;
    let max_tokens = model.max_tokens;
    let AgentMessage::Assistant {
        provider: source_provider,
        model: source_model,
        timestamp,
        stop_reason,
        usage,
        ..
    } = message
    else {
        return None;
    };
    if !settings.enabled || (skip_aborted && *stop_reason == StopReason::Aborted) {
        return None;
    }
    let compacted_at = entries
        .iter()
        .rev()
        .find(|entry| matches!(entry.kind, SessionEntryKind::Compaction { .. }))
        .and_then(|entry| chrono::DateTime::parse_from_rfc3339(&entry.timestamp).ok())
        .map(|value| value.timestamp_millis());
    if compacted_at.is_some_and(|at| *timestamp <= at) {
        return None;
    }
    if source_provider == &model.provider
        && source_model == &model.id
        && (is_overflow(message, context_window) || recoverable_length(message, max_tokens))
    {
        return Some(AutoCompaction {
            reason: "overflow",
            will_retry: *stop_reason != StopReason::Stop,
        });
    }
    let direct = agent::calculate_context_tokens(usage);
    let tokens = if *stop_reason == StopReason::Error || direct == 0 {
        let estimate = agent::estimate_context_tokens(messages);
        if let Some(index) = estimate.last_usage_index
            && let AgentMessage::Assistant { timestamp, .. } = &messages[index]
            && compacted_at.is_some_and(|at| *timestamp <= at)
        {
            return None;
        }
        estimate.tokens
    } else {
        direct
    };
    agent::should_compact(tokens, context_window, settings).then_some(AutoCompaction {
        reason: "threshold",
        will_retry: false,
    })
}
