//! Pinned Pi extensions/llama/huggingface.ts. Called only by explicit model
//! search/details actions; opening settings does not contact Hugging Face.
use crate::tools::CancellationToken;
use anyhow::{Result, bail};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

pub async fn find_token() -> Option<String> {
    if let Ok(token) = std::env::var("HF_TOKEN")
        && !token.trim_matches(crate::ecmascript::whitespace).is_empty()
    {
        return Some(token.trim_matches(crate::ecmascript::whitespace).into());
    }
    let mut paths = Vec::new();
    if let Some(p) = std::env::var_os("HF_TOKEN_PATH").filter(|s| !s.is_empty()) {
        paths.push(PathBuf::from(p));
    }
    if let Some(p) = std::env::var_os("HF_HOME").filter(|s| !s.is_empty()) {
        paths.push(PathBuf::from(p).join("token"));
    }
    if let Some(p) = std::env::var_os("XDG_CACHE_HOME").filter(|s| !s.is_empty()) {
        paths.push(PathBuf::from(p).join("huggingface/token"));
    }
    if let Some(p) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(p).join(".cache/huggingface/token"));
    }
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        if seen.insert(path.clone())
            && let Ok(token) = tokio::fs::read_to_string(path).await
            && !token.trim_matches(crate::ecmascript::whitespace).is_empty()
        {
            return Some(token.trim_matches(crate::ecmascript::whitespace).into());
        }
    }
    None
}

pub fn search_results(payload: Value) -> Result<Value> {
    let Some(results) = payload.as_array() else {
        bail!("Hugging Face returned invalid search results");
    };
    Ok(Value::Array(
        results
            .iter()
            .filter_map(|value| {
                value["id"].as_str().map(
                    |id| json!({"id":id,"downloads":value["downloads"].as_f64().unwrap_or(0.0)}),
                )
            })
            .collect(),
    ))
}

pub fn model_details(id: &str, payload: Value) -> Result<Value> {
    if !payload.is_object() && !payload.is_array() {
        bail!("Hugging Face returned invalid model details");
    }
    static QUANT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:^|[-_.])((?:UD-)?(?:IQ\d(?:_[A-Z0-9]+)+|Q\d(?:_[A-Z0-9]+)+|BF16|F16|F32|MXFP\d(?:_[A-Z0-9]+)*))$").unwrap()
    });
    static SHARD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-\d{5}-of-\d{5}$").unwrap());
    let mut sizes: BTreeMap<String, (f64, bool)> = BTreeMap::new();
    for file in payload["siblings"].as_array().into_iter().flatten() {
        let Some(path) = file["rfilename"]
            .as_str()
            .filter(|p| p.to_lowercase().ends_with(".gguf"))
        else {
            continue;
        };
        let filename = path.rsplit('/').next().unwrap();
        if filename.to_lowercase().starts_with("mmproj") {
            continue;
        }
        let stem = SHARD.replace(&filename[..filename.len() - 5], "");
        let Some(capture) = QUANT.captures(&stem) else {
            continue;
        };
        let entry = sizes
            .entry(capture[1].to_uppercase())
            .or_insert((0.0, true));
        if let Some(size) = file["size"].as_f64() {
            entry.0 += size;
        } else {
            entry.1 = false;
        }
    }
    let mut quantizations: Vec<_> = sizes
        .into_iter()
        .map(|(name, (size, complete))| {
            let mut value = json!({"name":name});
            if complete {
                value["size"] = json!(size);
            }
            value
        })
        .collect();
    let collator = icu_collator::Collator::try_new(Default::default(), Default::default())
        .expect("compiled ICU data");
    quantizations.sort_by(|l, r| {
        if l["name"] == "Q4_K_M" {
            return std::cmp::Ordering::Less;
        }
        if r["name"] == "Q4_K_M" {
            return std::cmp::Ordering::Greater;
        }
        l["size"]
            .as_f64()
            .unwrap_or(9_007_199_254_740_991.0)
            .total_cmp(&r["size"].as_f64().unwrap_or(9_007_199_254_740_991.0))
            .then_with(|| {
                collator.compare(l["name"].as_str().unwrap(), r["name"].as_str().unwrap())
            })
    });
    Ok(
        json!({"id":payload["id"].as_str().unwrap_or(id),"gated":if matches!(payload["gated"].as_str(),Some("auto"|"manual")){payload["gated"].clone()}else{json!(false)},"quantizations":quantizations}),
    )
}

// Pi uses Number(header), not parseFloat: radix strings and exact Infinity
// spellings are accepted; NaN and zero fall through to the RateLimit header.
fn retry_after_number(value: &str) -> Option<f64> {
    let value = value.trim_matches(crate::ecmascript::whitespace);
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
        if let Some(digits) = value.strip_prefix(prefix) {
            return crate::ecmascript::radix_number(digits, radix);
        }
    }
    if value.is_empty() {
        return Some(0.0);
    }
    static DECIMAL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^[+-]?(?:Infinity|(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?)$")
            .unwrap()
    });
    DECIMAL
        .is_match(value)
        .then(|| value.parse().ok())
        .flatten()
}

pub fn http_error(status: u16, headers: &reqwest::header::HeaderMap, payload: &Value) -> String {
    if status == 429 {
        let delay = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(retry_after_number)
            .filter(|v| *v != 0.0)
            .or_else(|| {
                headers
                    .get("ratelimit")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| Regex::new(r"(?:^|;)t=(\d+)").unwrap().captures(v))
                    .and_then(|c| c[1].parse().ok())
            });
        return delay
            .filter(|n| *n != 0.0)
            .map(|n| {
                format!(
                    "Hugging Face rate limit reached; retry in {}s",
                    crate::ecmascript::number_string(n)
                )
            })
            .unwrap_or_else(|| "Hugging Face rate limit reached".into());
    }
    payload["error"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("Hugging Face returned HTTP {status}"))
}

pub struct Client {
    token: Option<String>,
    base_url: String,
    http: reqwest::Client,
}
impl Client {
    pub fn new(token: Option<String>) -> Self {
        Self {
            token,
            base_url: "https://huggingface.co".into(),
            http: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("build HF HTTP client"),
        }
    }
    async fn request(&self, path: &str, cancel: &CancellationToken) -> Result<Value> {
        // Allowed network category: user explicitly requested HF search/details.
        let mut request = self.http.get(format!("{}{path}", self.base_url));
        if let Some(token) = self.token.as_ref().filter(|token| !token.is_empty()) {
            request = request.bearer_auth(token);
        }
        let operation = async {
            let response = request
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("fetch failed"))?;
            let status = response.status();
            let headers = response.headers().clone();
            let payload = response.json().await.unwrap_or(Value::Null);
            if !status.is_success() {
                let mut error = http_error(status.as_u16(), &headers, &payload);
                if let Some(token) = self.token.as_ref().filter(|t| !t.is_empty()) {
                    error = error.replace(token, "[redacted]");
                }
                bail!("{error}");
            }
            Ok(payload)
        };
        tokio::select! {biased;_=cancel.cancelled()=>bail!("This operation was aborted"),result=tokio::time::timeout(Duration::from_secs(15),operation)=>result.map_err(|_|anyhow::anyhow!("The operation was aborted due to timeout"))?}
    }
    pub async fn search(&self, query: &str, cancel: &CancellationToken) -> Result<Value> {
        let params = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("search", query),
                ("filter", "gguf"),
                ("sort", "downloads"),
                ("direction", "-1"),
                ("limit", "20"),
            ])
            .finish();
        search_results(
            self.request(&format!("/api/models?{params}"), cancel)
                .await?,
        )
    }
    pub async fn details(&self, id: &str, cancel: &CancellationToken) -> Result<Value> {
        // encodeURIComponent, preserving only its exact ASCII unescaped set.
        let encoded = id
            .split('/')
            .map(|s| {
                s.bytes()
                    .map(|b| {
                        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
                            (b as char).to_string()
                        } else {
                            format!("%{b:02X}")
                        }
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("/");
        model_details(
            id,
            self.request(&format!("/api/models/{encoded}?blobs=true"), cancel)
                .await?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Response, routing::any};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn explicit_http_actions_preserve_paths_gated_details_and_error_redaction() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let app = Router::new().fallback(any(move |request: axum::extract::Request| {
            let recorded = recorded.clone();
            async move {
                recorded.lock().unwrap().push((request.uri().to_string(), request.headers().get("authorization").cloned()));
                let (status, payload) = if request.uri().path() == "/api/models/a%20b/model!" {
                    (200, json!({"id":"a b/model!","gated":"manual","siblings":[{"rfilename":"model.Q4_K_M.gguf","size":2048}]}).to_string())
                } else if request.uri().query().is_some_and(|query| query.contains("search=bad")) {
                    (200, "malformed JSON".into())
                } else if request.uri().query().is_some_and(|query| query.contains("search=private")) {
                    (401, json!({"error":"Access rejected for fixture-token"}).to_string())
                } else {
                    (200, json!([{"id":"org/model","downloads":123}]).to_string())
                };
                Response::builder().status(status).body(Body::from(payload)).unwrap()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = Client {
            token: Some("fixture-token".into()),
            base_url: base_url.clone(),
            http: reqwest::Client::new(),
        };
        let cancel = CancellationToken::default();
        assert_eq!(
            client.search("tiny model/gguf", &cancel).await.unwrap(),
            json!([{"id":"org/model","downloads":123.0}])
        );
        let details = client.details("a b/model!", &cancel).await.unwrap();
        assert_eq!(details["gated"], "manual");
        assert_eq!(
            details["quantizations"],
            json!([{"name":"Q4_K_M","size":2048.0}])
        );
        assert_eq!(
            client.search("bad", &cancel).await.unwrap_err().to_string(),
            "Hugging Face returned invalid search results"
        );
        assert_eq!(
            client
                .search("private", &cancel)
                .await
                .unwrap_err()
                .to_string(),
            "Access rejected for [redacted]"
        );
        cancel.cancel();
        let before = calls.lock().unwrap().len();
        assert_eq!(
            client
                .search("cancelled", &cancel)
                .await
                .unwrap_err()
                .to_string(),
            "This operation was aborted"
        );
        assert_eq!(calls.lock().unwrap().len(), before);
        let public = Client {
            token: Some(String::new()),
            base_url,
            http: reqwest::Client::new(),
        };
        public
            .search("public", &CancellationToken::default())
            .await
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[0].0,
            "/api/models?search=tiny+model%2Fgguf&filter=gguf&sort=downloads&direction=-1&limit=20"
        );
        assert_eq!(calls[1].0, "/api/models/a%20b/model!?blobs=true");
        assert_eq!(calls[0].1.as_ref().unwrap(), "Bearer fixture-token");
        assert!(calls.last().unwrap().1.is_none());
        server.abort();
    }
}
