//! Agent-owned Pi footer values, shared with restoration of finished sessions.
//! Pin: 9841914c71a74d81abe07f751aefd271fd924e63; coding-agent/src/core/
//! usage-totals.ts, agent-session.ts#getContextUsage and components/footer.ts.
use crate::agent::{self, AgentMessage, SessionEntry, SessionEntryKind, UsageTotals};
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub totals: UsageTotals,
    pub context: Option<agent::CurrentContextUsage>,
    pub latest_cache_hit_rate: Option<f64>,
    pub text: String,
}

// JavaScript Number.toFixed rounds the exact binary value to the nearest
// decimal, choosing the larger integer on a tie. Rust formatting uses even ties.
// Usage labels use only nonnegative finite values and at most three decimals.
pub fn fixed(value: f64, digits: u32) -> String {
    assert!(value.is_finite() && value >= 0.0 && digits <= 3);
    if value >= 1e21 {
        return value.to_string();
    }
    let bits = value.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i32;
    let mantissa = (bits & ((1_u64 << 52) - 1)) | if exponent == 0 { 0 } else { 1_u64 << 52 };
    let shift = if exponent == 0 {
        -1074
    } else {
        exponent - 1023 - 52
    };
    let numerator = u128::from(mantissa) * 10_u128.pow(digits);
    let rounded = if shift >= 0 {
        numerator << shift
    } else if -shift >= 128 {
        0
    } else {
        let shift = (-shift) as u32;
        let quotient = numerator >> shift;
        let remainder = numerator & ((1_u128 << shift) - 1);
        quotient + u128::from(remainder >= 1_u128 << (shift - 1))
    };
    let mut result = rounded.to_string();
    if digits > 0 {
        let width = digits as usize + 1;
        if result.len() < width {
            result = format!("{}{}", "0".repeat(width - result.len()), result);
        }
        result.insert(result.len() - digits as usize, '.');
    }
    result
}

pub fn format_tokens(count: u64) -> String {
    if count < 1000 {
        count.to_string()
    } else if count < 10000 {
        format!("{}k", fixed(count as f64 / 1000.0, 1))
    } else if count < 1000000 {
        format!("{}k", (count + 500) / 1000)
    } else if count < 10000000 {
        format!("{}M", fixed(count as f64 / 1000000.0, 1))
    } else {
        format!("{}M", (count + 500000) / 1000000)
    }
}

pub fn snapshot(
    entries: &[SessionEntry],
    messages: &[AgentMessage],
    before: UsageTotals,
    context_window: u64,
    subscription: bool,
    auto: bool,
) -> Snapshot {
    snapshot_with_cache(
        entries,
        messages,
        before,
        None,
        context_window,
        subscription,
        auto,
    )
}

/// None means no assistant entry; Some(None) means the latest assistant has no
/// prompt usage. The latter must clear an older nonzero cache-hit percentage.
pub fn latest_cache_hit_rate(entries: &[SessionEntry]) -> Option<Option<f64>> {
    entries.iter().rev().find_map(|entry| match &entry.kind {
        SessionEntryKind::Message {
            message: AgentMessage::Assistant { usage, .. },
        } => {
            let prompt = usage.input + usage.cache_read + usage.cache_write;
            Some((prompt > 0).then(|| usage.cache_read as f64 / prompt as f64 * 100.0))
        }
        _ => None,
    })
}

pub fn snapshot_with_cache(
    entries: &[SessionEntry],
    messages: &[AgentMessage],
    before: UsageTotals,
    cache_hit_rate_before: Option<f64>,
    context_window: u64,
    subscription: bool,
    auto: bool,
) -> Snapshot {
    let mut totals = before;
    agent::accumulate_session_usage(&mut totals, entries);
    let context = agent::current_context_usage(entries, messages, context_window);
    let latest_cache_hit_rate = latest_cache_hit_rate(entries).unwrap_or(cache_hit_rate_before);
    let mut parts = Vec::new();
    for (prefix, count) in [
        ("↑", totals.input),
        ("↓", totals.output),
        ("R", totals.cache_read),
        ("W", totals.cache_write),
    ] {
        if count > 0 {
            parts.push(format!("{prefix}{}", format_tokens(count)));
        }
    }
    if (totals.cache_read > 0 || totals.cache_write > 0)
        && let Some(rate) = latest_cache_hit_rate
    {
        parts.push(format!("CH{}%", fixed(rate, 1)));
    }
    if totals.cost > 0.0 || subscription {
        parts.push(format!(
            "${}{}",
            fixed(totals.cost, 3),
            if subscription { " (sub)" } else { "" }
        ));
    }
    let percent = match context {
        Some(context) => context
            .percent
            .map(|percent| format!("{}%", fixed(percent, 1)))
            .unwrap_or_else(|| "?".into()),
        None => "0.0%".into(),
    };
    parts.push(format!(
        "{percent}/{}{}",
        format_tokens(context_window),
        if auto { " (auto)" } else { "" }
    ));
    Snapshot {
        totals,
        context,
        latest_cache_hit_rate,
        text: parts.join(" "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decimal_rounding_uses_javascript_binary_number_rules() {
        for (value, digits, expected) in [
            (0.0625, 3, "0.063"),
            (1.25, 1, "1.3"),
            (2.55, 1, "2.5"),
            (0.0005, 3, "0.001"),
            (0.0, 3, "0.000"),
        ] {
            assert_eq!(fixed(value, digits), expected);
        }
    }
}
