//! Pinned dedicated Codex SSE HTTP transport, error and retry rules.
use crate::providers::ProviderRequest;
use anyhow::{Result, bail};
use reqwest::{Client, Response, header::HeaderMap};
use serde_json::Value;
use std::time::Duration;

pub fn resolve_url(base: &str) -> String {
    let base = if base.trim().is_empty() {
        crate::providers::OPENAI_CODEX_BASE_URL
    } else {
        base
    };
    let base = base.trim_end_matches('/');
    if base.ends_with("/codex/responses") {
        base.into()
    } else if base.ends_with("/codex") {
        format!("{base}/responses")
    } else {
        format!("{base}/codex/responses")
    }
}
fn truthy(v: &Value) -> bool {
    !v.is_null() && v != false && v != "" && v != 0
}
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(_) => "[object Object]".into(),
        Value::Array(v) => v
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    js_string(v)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        _ => v.to_string(),
    }
}
pub fn error_message(
    status: u16,
    status_text: &str,
    raw: &str,
    now_ms: i64,
    secrets: &[String],
) -> String {
    let raw = crate::provider_http::safe_error_body(raw, secrets);
    let mut message = if !raw.is_empty() {
        raw.clone()
    } else if !status_text.is_empty() {
        status_text.into()
    } else {
        "Request failed".into()
    };
    if let Ok(parsed) = serde_json::from_str::<Value>(&raw)
        && let Some(error) = parsed.get("error").filter(|v| truthy(v))
    {
        let code = error
            .get("code")
            .filter(|v| truthy(v))
            .or_else(|| error.get("type"))
            .map(js_string)
            .unwrap_or_default()
            .to_lowercase();
        if [
            "usage_limit_reached",
            "usage_not_included",
            "rate_limit_exceeded",
        ]
        .iter()
        .any(|v| code.contains(v))
            || status == 429
        {
            let plan = error
                .get("plan_type")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(|v| format!(" ({} plan)", v.to_lowercase()))
                .unwrap_or_default();
            let when = error
                .get("resets_at")
                .and_then(Value::as_f64)
                .filter(|v| *v != 0.0)
                .map(|v| {
                    format!(
                        " Try again in ~{} min.",
                        (((v * 1000.0 - now_ms as f64) / 60000.0 + 0.5).floor()).max(0.0)
                    )
                })
                .unwrap_or_default();
            return format!("You have hit your ChatGPT usage limit{plan}.{when}")
                .trim()
                .into();
        }
        if let Some(value) = error.get("message").filter(|v| truthy(v)) {
            message = js_string(value);
        }
    }
    message
}
fn regex_match(pattern: &str, text: &str) -> bool {
    regex::Regex::new(pattern)
        .expect("pinned regex")
        .is_match(text)
}
pub fn retryable(status: u16, text: &str) -> bool {
    if status == 429
        && regex_match(
            r"(?i)GoUsageLimitError|FreeUsageLimitError|Monthly usage limit reached|available balance|insufficient_quota|out of budget|quota exceeded|billing",
            text,
        )
    {
        return false;
    }
    matches!(status, 429 | 500 | 502 | 503 | 504)
        || regex_match(
            r"(?i)rate.?limit|overloaded|service.?unavailable|upstream.?connect|connection.?refused",
            text,
        )
}
fn number(v: &str) -> Option<f64> {
    let v = v.trim();
    if v.is_empty() {
        Some(0.0)
    } else if let Some(hex) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok().map(|v| v as f64)
    } else {
        v.parse::<f64>().ok().filter(|v| v.is_finite())
    }
}
fn retry_after(headers: &HeaderMap) -> Option<f64> {
    if let Some(v) = headers
        .get("retry-after-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(number)
    {
        return Some(v.max(0.0));
    }
    let v = headers.get("retry-after")?.to_str().ok()?;
    if v.is_empty() {
        return None;
    }
    if let Some(n) = number(v) {
        return Some((n * 1000.0).max(0.0));
    }
    chrono::DateTime::parse_from_rfc2822(v).ok().map(|date| {
        ((date.timestamp_millis() - chrono::Utc::now().timestamp_millis()) as f64).max(0.0)
    })
}
async fn sleep_ms(milliseconds: f64) {
    let millis =
        if milliseconds.is_finite() && milliseconds >= 1.0 && milliseconds <= i32::MAX as f64 {
            milliseconds as u64
        } else {
            1
        };
    tokio::time::sleep(Duration::from_millis(millis)).await;
}
pub async fn send(
    client: &Client,
    url: String,
    mut headers: HeaderMap,
    body: Value,
    request: &ProviderRequest,
    secrets: &[String],
) -> Result<Response> {
    let json = serde_json::to_vec(&body)?;
    let bytes = match zstd::bulk::compress(&json, 3) {
        Ok(compressed) => {
            headers.insert(
                "content-encoding",
                reqwest::header::HeaderValue::from_static("zstd"),
            );
            compressed
        }
        Err(_) => json,
    };
    let maximum = request.max_retries.unwrap_or(0);
    for attempt in 0..=maximum {
        // Allowed network: explicitly selected OpenAI subscription inference.
        let send = client
            .post(&url)
            .headers(headers.clone())
            .body(bytes.clone())
            .send();
        let sent = if let Some(ms) = request.timeout_ms.filter(|v| *v > 0) {
            match tokio::time::timeout(Duration::from_millis(ms), send).await {
                Ok(v) => v.map_err(|_| "fetch failed".to_owned()),
                Err(_) => Err(format!("Codex SSE response headers timed out after {ms}ms")),
            }
        } else {
            send.await.map_err(|_| "fetch failed".to_owned())
        };
        let error = match sent {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => {
                let status = response.status();
                let response_headers = response.headers().clone();
                let raw = response.text().await.unwrap_or_default();
                if attempt < maximum && retryable(status.as_u16(), &raw) {
                    let delay = if let Some(delay) = retry_after(&response_headers) {
                        let max = request.max_retry_delay_ms.unwrap_or(60000);
                        if max > 0 && delay > max as f64 {
                            bail!(
                                "Server requested {}s retry delay (max: {}s)",
                                (delay / 1000.0).ceil(),
                                (max as f64 / 1000.0).ceil()
                            );
                        }
                        delay
                    } else {
                        1000.0 * 2f64.powf(attempt as f64)
                    };
                    sleep_ms(delay).await;
                    continue;
                }
                error_message(
                    status.as_u16(),
                    status.canonical_reason().unwrap_or_default(),
                    &raw,
                    chrono::Utc::now().timestamp_millis(),
                    secrets,
                )
            }
            Err(error) => error,
        };
        // Pi's catch also retries non-retryable HTTP errors unless the final
        // friendly error contains this exact phrase. Preserve that quirk.
        if attempt < maximum && !error.contains("usage limit") {
            sleep_ms(1000.0 * 2f64.powf(attempt as f64)).await;
            continue;
        }
        bail!("{error}");
    }
    bail!("Failed after retries")
}
