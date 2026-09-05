//! Pinned OpenAI SDK APIError plus Pi error-body.ts/provider-retry.ts.
//! Credential redaction is the explicit BashKitten secret-storage requirement.
use crate::providers::ProviderRequest;
use anyhow::{Result, bail};
use reqwest::{Client, Response, header::HeaderMap};
use serde_json::{Value, json};
use std::time::Duration;

pub(crate) fn safe_error_body(body: &str, secrets: &[String]) -> String {
    fn clean(v: &mut Value) {
        match v {
            Value::Object(map) => {
                for (key, value) in map {
                    if matches!(
                        key.to_ascii_lowercase().as_str(),
                        "access_token"
                            | "refresh_token"
                            | "id_token"
                            | "api_key"
                            | "authorization"
                            | "client_secret"
                    ) {
                        *value = json!("<redacted>");
                    } else {
                        clean(value);
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    clean(value);
                }
            }
            _ => {}
        }
    }
    let mut text = body.to_owned();
    // Keep original non-secret JSON bytes: the SDK distinguishes malformed/raw
    // bodies from parsed objects and preserves whitespace in raw text.
    if let Ok(mut value) = serde_json::from_str::<Value>(body) {
        let before = value.clone();
        clean(&mut value);
        if value != before {
            text = value.to_string();
        }
    }
    for secret in secrets.iter().filter(|v| !v.is_empty()) {
        text = text.replace(secret, "<redacted>");
    }
    text
}
pub fn error_message(status: u16, body: &str, secrets: &[String]) -> String {
    let body = safe_error_body(body, secrets);
    let parsed = serde_json::from_str::<Value>(&body).ok();
    let error = parsed.as_ref().and_then(|v| v.get("error"));
    let truthy = |v: &Value| !v.is_null() && v != false && v != "" && v != 0;
    let message = if let Some(error) = error.filter(|v| truthy(v)) {
        if let Some(message) = error.get("message").filter(|v| truthy(v)) {
            message
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| message.to_string())
        } else {
            error.to_string()
        }
    } else if parsed.is_none() {
        body.clone()
    } else {
        String::new()
    };
    let mut message = if message.is_empty() {
        format!("{status} status code (no body)")
    } else {
        format!("{status} {message}")
    };
    if let Some(error) = error.filter(|v| v.as_object().is_some_and(|v| !v.is_empty())) {
        let full = error.to_string();
        let length = full.encode_utf16().count();
        let capped = if length > 4000 {
            let units: Vec<_> = full.encode_utf16().take(4000).collect();
            format!(
                "{}... [truncated {} chars]",
                String::from_utf16_lossy(&units),
                length - 4000
            )
        } else {
            full
        };
        if !message.contains(&capped) {
            message = format!("{status}: {capped}");
        }
        if let Some(raw) = error.pointer("/metadata/raw").filter(|v| truthy(v)) {
            let raw = raw
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| raw.to_string());
            if !message.contains(&raw) {
                message.push('\n');
                message.push_str(&raw);
            }
        }
    }
    message
}
fn retryable(status: Option<u16>, headers: &HeaderMap) -> bool {
    match headers.get("x-should-retry").and_then(|v| v.to_str().ok()) {
        Some("true") => true,
        Some("false") => false,
        _ => status.is_none_or(|v| matches!(v, 408 | 409 | 429) || v >= 500),
    }
}
fn parse_float(value: &str) -> Option<f64> {
    static PATTERN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^[+-]?(?:Infinity|(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?)")
            .unwrap()
    });
    PATTERN
        .find(value.trim_start())
        .and_then(|m| m.as_str().parse().ok())
}
fn retry_delay(headers: &HeaderMap, index: u32, max: Option<u64>, error: &str) -> Result<Duration> {
    let explicit = headers
        .get("retry-after-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_float)
        .or_else(|| {
            let value = headers.get("retry-after")?.to_str().ok()?;
            Some(parse_float(value).map(|v| v * 1000.0).unwrap_or_else(|| {
                chrono::DateTime::parse_from_rfc2822(value)
                    .map(|date| {
                        (date.timestamp_millis() - chrono::Utc::now().timestamp_millis()) as f64
                    })
                    .unwrap_or(f64::NAN)
            }))
        });
    let milliseconds = if let Some(value) = explicit {
        let maximum = max.unwrap_or(60000);
        if maximum > 0 && value > maximum as f64 {
            bail!(
                "Server requested {}s retry delay (max: {}s). {error}",
                (value / 1000.0).ceil(),
                (maximum as f64 / 1000.0).ceil()
            );
        }
        value
    } else {
        (0.5 * 2f64.powf(index as f64)).min(8.0) * 1000.0 * (1.0 - rand::random::<f64>() * 0.25)
    };
    // Node setTimeout maps invalid, negative and overflowing delays to 1 ms.
    let milliseconds =
        if milliseconds.is_finite() && milliseconds >= 1.0 && milliseconds <= i32::MAX as f64 {
            milliseconds as u64
        } else {
            1
        };
    Ok(Duration::from_millis(milliseconds))
}

pub async fn send_compatible(
    client: &Client,
    url: String,
    headers: HeaderMap,
    body: Value,
    request: &ProviderRequest,
    secrets: &[String],
) -> Result<Response> {
    let maximum = request.max_retries.unwrap_or(0);
    let timeout = Duration::from_millis(request.timeout_ms.unwrap_or(600000));
    for attempt in 0..=maximum {
        // Allowed network: explicit inference at the configured compatible
        // provider (or its explicitly selected llama.cpp loopback endpoint).
        // Pi's SDK timeout covers fetch/response headers, not the SSE lifetime.
        let sent = tokio::time::timeout(
            timeout,
            client
                .post(url.clone())
                .headers(headers.clone())
                .json(&body)
                .send(),
        )
        .await;
        let (status, response_headers, error) = match sent {
            Ok(Ok(response)) if response.status().is_success() => return Ok(response),
            Ok(Ok(response)) => {
                let status = response.status().as_u16();
                let headers = response.headers().clone();
                let text = response.text().await.unwrap_or_default();
                (Some(status), headers, error_message(status, &text, secrets))
            }
            Ok(Err(error)) => (
                None,
                HeaderMap::new(),
                if error.is_timeout() {
                    "Request timed out."
                } else {
                    "Connection error."
                }
                .into(),
            ),
            Err(_) => (None, HeaderMap::new(), "Request timed out.".into()),
        };
        if attempt == maximum || !retryable(status, &response_headers) {
            bail!("{error}");
        }
        tokio::time::sleep(retry_delay(
            &response_headers,
            attempt,
            request.max_retry_delay_ms,
            &error,
        )?)
        .await;
        // Dropping this future cancels both the in-flight request and backoff;
        // the worker's cancellation select is authoritative for abort wording.
    }
    unreachable!("inclusive attempts always returns")
}
