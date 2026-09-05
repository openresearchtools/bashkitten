//! Native port of Pi's OpenAI Codex OAuth (see PI_UPSTREAM.md).
//! Outbound traffic here is exclusively user-initiated OpenAI authentication.
use crate::providers::{ProviderAuthStore, ProviderCredential, account_id_from_jwt};
use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::Query,
    http::{StatusCode, header},
    response::{Html, IntoResponse},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::TryRngCore;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc},
    task::JoinHandle,
    time::Instant,
};
use url::Url;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_BASE: &str = "https://auth.openai.com";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const LIFETIME: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStatus {
    pub phase: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    pub callback_available: bool,
}

struct Attempt {
    id: String,
    owner: String,
    status: LoginStatus,
    manual: Option<mpsc::Sender<String>>,
    task: Option<JoinHandle<()>>,
}

#[derive(Clone, Default)]
pub struct LoginManager(Arc<Mutex<Option<Attempt>>>);

struct BrowserFlow {
    verifier: String,
    state: String,
    url: String,
}

fn browser_flow() -> Result<BrowserFlow> {
    let mut verifier_bytes = [0_u8; 32];
    let mut state_bytes = [0_u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut verifier_bytes)
        .map_err(|_| anyhow::anyhow!("OS random source unavailable"))?;
    rand::rngs::OsRng
        .try_fill_bytes(&mut state_bytes)
        .map_err(|_| anyhow::anyhow!("OS random source unavailable"))?;
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = hex::encode(state_bytes);
    let mut url = Url::parse(&format!("{AUTH_BASE}/oauth/authorize"))?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "pi"),
    ]);
    Ok(BrowserFlow {
        verifier,
        state,
        url: url.into(),
    })
}

// Pi accepts a redirect URL, query, code#state, or a bare manually pasted code.
fn authorization_code(input: &str, expected_state: &str) -> Result<String> {
    let input = input.trim();
    let (code, state) = if let Ok(url) = Url::parse(input) {
        let pairs: HashMap<_, _> = url.query_pairs().into_owned().collect();
        (pairs.get("code").cloned(), pairs.get("state").cloned())
    } else if let Some((code, state)) = input.split_once('#') {
        (
            Some(code.to_owned()),
            Some(state.split('#').next().unwrap_or("").to_owned()),
        )
    } else if input.contains("code=") {
        let pairs: HashMap<_, _> = url::form_urlencoded::parse(input.as_bytes())
            .into_owned()
            .collect();
        (pairs.get("code").cloned(), pairs.get("state").cloned())
    } else {
        (Some(input.to_owned()), None)
    };
    if let Some(state) = state.filter(|state| !state.is_empty()) {
        if !bool::from(state.as_bytes().ct_eq(expected_state.as_bytes())) {
            bail!("State mismatch");
        }
    }
    code.filter(|code| !code.is_empty())
        .context("Missing authorization code")
}

struct AbortOnDrop(JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn callback_server(state: String, tx: mpsc::Sender<String>) -> Option<AbortOnDrop> {
    let listener = TcpListener::bind(("127.0.0.1", 1455)).await.ok()?;
    let app = Router::new().route("/auth/callback", get(move |Query(params): Query<HashMap<String, String>>| {
        let expected = state.clone();
        let tx = tx.clone();
        async move {
            let state = params.get("state").map(String::as_str).unwrap_or("");
            let code = params.get("code").filter(|code| !code.is_empty());
            let (status, message) = if !bool::from(state.as_bytes().ct_eq(expected.as_bytes())) {
                (StatusCode::BAD_REQUEST, "State mismatch.")
            } else if let Some(code) = code {
                if tx.try_send(format!("{code}#{state}")).is_ok() {
                    (StatusCode::OK, "OpenAI authorization received. Return to BashKitten Settings to finish. You can close this window.")
                } else { (StatusCode::CONFLICT, "Login is no longer waiting for a code.") }
            } else { (StatusCode::BAD_REQUEST, "Missing authorization code.") };
            (status, [(header::CACHE_CONTROL, "no-store"), (header::REFERRER_POLICY, "no-referrer"), (header::CONTENT_SECURITY_POLICY, "default-src 'none'; frame-ancestors 'none'")], Html(format!("<!doctype html><meta charset=utf-8><title>BashKitten login</title><p>{message}</p>"))).into_response()
        }
    }));
    Some(AbortOnDrop(tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    })))
}

impl LoginManager {
    pub async fn status(&self, owner: &str) -> LoginStatus {
        self.0
            .lock()
            .await
            .as_ref()
            .filter(|attempt| attempt.owner == owner)
            .map(|attempt| attempt.status.clone())
            .unwrap_or_else(|| LoginStatus {
                phase: "idle".into(),
                ..Default::default()
            })
    }

    pub async fn start(
        &self,
        owner: String,
        method: &str,
        store: ProviderAuthStore,
    ) -> Result<LoginStatus> {
        if !matches!(method, "browser" | "device_code") {
            bail!("Unknown OpenAI Codex login method");
        }
        let mut guard = self.0.lock().await;
        if guard.as_ref().is_some_and(|attempt| {
            attempt
                .task
                .as_ref()
                .is_some_and(|task| !task.is_finished())
        }) {
            bail!("A login is already in progress. Complete or cancel it first.");
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::channel(1);
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let flow = if method == "browser" {
            Some(browser_flow()?)
        } else {
            None
        };
        let listener = if let Some(flow) = &flow {
            callback_server(flow.state.clone(), tx.clone()).await
        } else {
            None
        };
        let status = LoginStatus {
            phase: if flow.is_some() {
                "waiting"
            } else {
                "starting"
            }
            .into(),
            message: if flow.is_some() {
                "A browser window should open. Complete login to finish."
            } else {
                "Requesting device code…"
            }
            .into(),
            url: flow.as_ref().map(|flow| flow.url.clone()),
            user_code: None,
            callback_available: listener.is_some(),
        };
        let manager = self.clone();
        let task_id = id.clone();
        let task = tokio::spawn(async move {
            // The guard owns the temporary listener even if cancellation aborts this task.
            let _listener = listener;
            let result = if let Some(flow) = flow {
                browser_login(&client, flow, rx).await
            } else {
                device_login(&client, &manager, &task_id).await
            };
            manager.finish(&task_id, store, result).await;
        });
        *guard = Some(Attempt {
            id,
            owner,
            status: status.clone(),
            manual: Some(tx),
            task: Some(task),
        });
        Ok(status)
    }

    async fn finish(&self, id: &str, store: ProviderAuthStore, result: Result<ProviderCredential>) {
        let mut guard = self.0.lock().await;
        let Some(attempt) = guard.as_mut().filter(|attempt| attempt.id == id) else {
            return;
        };
        let result = match result {
            Ok(credential) => store.set_codex(Some(credential)).await,
            Err(error) => Err(error),
        };
        attempt.manual = None;
        attempt.status = match result {
            Ok(()) => LoginStatus {
                phase: "complete".into(),
                message: "OpenAI subscription connected.".into(),
                ..Default::default()
            },
            Err(error) => LoginStatus {
                phase: "error".into(),
                message: error.to_string(),
                ..Default::default()
            },
        };
    }

    pub async fn submit(&self, owner: &str, input: String) -> Result<()> {
        if input.len() > 16_384 {
            bail!("Authorization input is too long");
        }
        let guard = self.0.lock().await;
        let attempt = guard
            .as_ref()
            .filter(|attempt| attempt.owner == owner)
            .context("No login in progress")?;
        if attempt
            .status
            .url
            .as_ref()
            .is_none_or(|url| !url.contains("/oauth/authorize"))
        {
            bail!("Browser login is not waiting for a code");
        }
        attempt
            .manual
            .as_ref()
            .context("Login is no longer waiting for a code")?
            .try_send(input)
            .map_err(|_| anyhow::anyhow!("Authorization code already submitted"))?;
        Ok(())
    }

    pub async fn cancel(&self, owner: &str) -> Result<()> {
        let mut guard = self.0.lock().await;
        if let Some(attempt) = guard.as_mut() {
            if attempt.owner != owner {
                bail!("Login belongs to another browser session");
            }
            if let Some(task) = attempt.task.take() {
                task.abort();
                let _ = task.await;
            }
            attempt.manual = None;
            attempt.status = LoginStatus {
                phase: "cancelled".into(),
                message: "Login cancelled".into(),
                ..Default::default()
            };
        }
        Ok(())
    }

    pub async fn logout(&self, store: ProviderAuthStore) -> Result<()> {
        let mut guard = self.0.lock().await;
        if let Some(mut attempt) = guard.take() {
            if let Some(task) = attempt.task.take() {
                task.abort();
                let _ = task.await;
            }
        }
        store.set_codex(None).await
    }
}

async fn browser_login(
    client: &Client,
    flow: BrowserFlow,
    mut rx: mpsc::Receiver<String>,
) -> Result<ProviderCredential> {
    let input = tokio::time::timeout(LIFETIME, rx.recv())
        .await
        .context("Browser login timed out")?
        .context("Login cancelled")?;
    let code = authorization_code(&input, &flow.state)?;
    exchange(
        client,
        &format!("{AUTH_BASE}/oauth/token"),
        &code,
        &flow.verifier,
        REDIRECT_URI,
    )
    .await
}

async fn exchange(
    client: &Client,
    endpoint: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<ProviderCredential> {
    let response = client
        .post(endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await
        .context("OpenAI Codex token exchange request failed")?;
    if !response.status().is_success() {
        bail!("OpenAI Codex token exchange failed ({})", response.status());
    }
    // Never reflect a token response/error body into logs or the browser.
    let json: Value = response
        .json()
        .await
        .context("Invalid OpenAI Codex token exchange response")?;
    credential_from_response(&json)
}

fn credential_from_response(json: &Value) -> Result<ProviderCredential> {
    let access = json["access_token"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("Token response missing access token")?;
    let refresh = json["refresh_token"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("Token response missing refresh token")?;
    let expires = json["expires_in"]
        .as_f64()
        .filter(|v| v.is_finite())
        .context("Token response missing expiry")?;
    let account_id =
        account_id_from_jwt(access).context("Failed to extract accountId from token")?;
    Ok(ProviderCredential::OAuth {
        access: access.into(),
        refresh: refresh.into(),
        expires: chrono::Utc::now()
            .timestamp_millis()
            .saturating_add((expires * 1000.0) as i64),
        extra: [("accountId".into(), Value::String(account_id))]
            .into_iter()
            .collect(),
    })
}

#[derive(Deserialize)]
struct DeviceToken {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Debug, PartialEq)]
enum DevicePoll {
    Pending,
    SlowDown,
    Complete,
}

fn device_poll_status(status: StatusCode, body: &Value) -> Result<DevicePoll> {
    if status.is_success() {
        return Ok(DevicePoll::Complete);
    }
    if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
        return Ok(DevicePoll::Pending);
    }
    let code = body["error"]["code"]
        .as_str()
        .or_else(|| body["error"].as_str());
    match code {
        Some("deviceauth_authorization_pending") => Ok(DevicePoll::Pending),
        Some("slow_down") => Ok(DevicePoll::SlowDown),
        _ => bail!("OpenAI Codex device auth failed with status {status}"),
    }
}

async fn device_login(
    client: &Client,
    manager: &LoginManager,
    id: &str,
) -> Result<ProviderCredential> {
    let response = client
        .post(format!("{AUTH_BASE}/api/accounts/deviceauth/usercode"))
        .json(&json!({"client_id": CLIENT_ID}))
        .send()
        .await
        .context("Device code request failed")?;
    if response.status() == StatusCode::NOT_FOUND {
        bail!(
            "OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL."
        );
    }
    if !response.status().is_success() {
        bail!(
            "OpenAI Codex device code request failed with status {}",
            response.status()
        );
    }
    let body: Value = response
        .json()
        .await
        .context("Invalid OpenAI Codex device code response")?;
    let device_id = body["device_auth_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("Device response missing device ID")?;
    let code = body["user_code"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("Device response missing user code")?;
    let interval = body["interval"]
        .as_f64()
        .or_else(|| body["interval"].as_str()?.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .context("Invalid device polling interval")?;
    let mut interval = Duration::from_millis(((interval * 1000.0).floor() as u64).max(1000));
    {
        let mut guard = manager.0.lock().await;
        let attempt = guard
            .as_mut()
            .filter(|attempt| attempt.id == id)
            .context("Login cancelled")?;
        attempt.manual = None;
        attempt.status = LoginStatus {
            phase: "waiting".into(),
            message: "Open the verification page and enter this code.".into(),
            url: Some(format!("{AUTH_BASE}/codex/device")),
            user_code: Some(code.into()),
            callback_available: false,
        };
    }
    let deadline = Instant::now() + LIFETIME;
    let mut slowed = false;
    while Instant::now() < deadline {
        // Pi polls immediately, then observes the server interval (minimum 1s).
        let response = client
            .post(format!("{AUTH_BASE}/api/accounts/deviceauth/token"))
            .json(&json!({"device_auth_id":device_id,"user_code":code}))
            .send()
            .await
            .context("Device authorization polling failed")?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        match device_poll_status(status, &body)? {
            DevicePoll::Complete => {
                let token: DeviceToken = serde_json::from_value(body)
                    .context("Invalid OpenAI Codex device auth token response")?;
                if token.authorization_code.is_empty() || token.code_verifier.is_empty() {
                    bail!("Invalid OpenAI Codex device auth token response");
                }
                return exchange(
                    client,
                    &format!("{AUTH_BASE}/oauth/token"),
                    &token.authorization_code,
                    &token.code_verifier,
                    DEVICE_REDIRECT_URI,
                )
                .await;
            }
            DevicePoll::SlowDown => {
                slowed = true;
                interval = interval.saturating_add(Duration::from_secs(5));
            }
            DevicePoll::Pending => {}
        }
        tokio::time::sleep(interval.min(deadline.saturating_duration_since(Instant::now()))).await;
    }
    if slowed {
        bail!(
            "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again."
        );
    }
    bail!("Device flow timed out")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn token_response() -> Value {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(
                &json!({"https://api.openai.com/auth":{"chatgpt_account_id":"test-account"}}),
            )
            .unwrap(),
        );
        json!({"access_token":format!("e30.{payload}.signature"),"refresh_token":"test-refresh","expires_in":3600})
    }

    #[test]
    fn pi_browser_url_pkce_and_manual_input() {
        let flow = browser_flow().unwrap();
        let url = Url::parse(&flow.url).unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.origin().ascii_serialization(), AUTH_BASE);
        assert_eq!(flow.verifier.len(), 43);
        assert_eq!(flow.state.len(), 32);
        assert_eq!(
            params["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(flow.verifier.as_bytes()))
        );
        assert_eq!(params["client_id"], CLIENT_ID);
        assert_eq!(params["redirect_uri"], REDIRECT_URI);
        assert_eq!(params["scope"], "openid profile email offline_access");
        assert_eq!(params["originator"], "pi");
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["codex_cli_simplified_flow"], "true");
        assert_eq!(params["id_token_add_organizations"], "true");
        assert!(!flow.url.contains(&flow.verifier));
        assert_ne!(flow.state, browser_flow().unwrap().state);
        for input in [
            "abc".to_owned(),
            format!("abc#{}", flow.state),
            format!("code=abc&state={}", flow.state),
            format!("{REDIRECT_URI}?code=abc&state={}", flow.state),
        ] {
            assert_eq!(authorization_code(&input, &flow.state).unwrap(), "abc");
        }
        assert_eq!(
            authorization_code("abc#wrong", &flow.state)
                .unwrap_err()
                .to_string(),
            "State mismatch"
        );
        assert!(authorization_code("", &flow.state).is_err());
        assert!(authorization_code(REDIRECT_URI, &flow.state).is_err());
    }

    #[test]
    fn pi_device_pending_and_slow_down_rules() {
        assert_eq!(
            device_poll_status(StatusCode::FORBIDDEN, &Value::Null).unwrap(),
            DevicePoll::Pending
        );
        assert_eq!(
            device_poll_status(StatusCode::NOT_FOUND, &Value::Null).unwrap(),
            DevicePoll::Pending
        );
        assert_eq!(
            device_poll_status(
                StatusCode::BAD_REQUEST,
                &json!({"error":{"code":"deviceauth_authorization_pending"}})
            )
            .unwrap(),
            DevicePoll::Pending
        );
        assert_eq!(
            device_poll_status(StatusCode::TOO_MANY_REQUESTS, &json!({"error":"slow_down"}))
                .unwrap(),
            DevicePoll::SlowDown
        );
        assert!(
            device_poll_status(StatusCode::BAD_REQUEST, &json!({"error":"invalid_grant"})).is_err()
        );
    }

    #[tokio::test]
    async fn token_exchange_matches_pi_and_stores_only_private_credentials() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let app = Router::new().route("/token", axum::routing::post(|axum::extract::Form(body): axum::extract::Form<HashMap<String,String>>| async move {
            assert_eq!(body.len(), 5);
            assert_eq!(body["grant_type"], "authorization_code");
            assert_eq!(body["client_id"], CLIENT_ID);
            assert_eq!(body["code"], "test-code");
            assert_eq!(body["code_verifier"], "test-verifier");
            assert_eq!(body["redirect_uri"], REDIRECT_URI);
            axum::Json(token_response())
        }));
        let _server = AbortOnDrop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let credential = exchange(
            &Client::new(),
            &endpoint,
            "test-code",
            "test-verifier",
            REDIRECT_URI,
        )
        .await
        .unwrap();
        let value = serde_json::to_value(&credential).unwrap();
        assert_eq!(value["type"], "oauth");
        assert_eq!(value["accountId"], "test-account");
        assert_eq!(value["refresh"], "test-refresh");
        assert!(value["expires"].as_i64().unwrap() > chrono::Utc::now().timestamp_millis());
        let temp = tempfile::tempdir().unwrap();
        let store = ProviderAuthStore::new(temp.path().join("auth.json"));
        store.set_codex(Some(credential)).await.unwrap();
        assert_eq!(
            store.path().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(store.credential("openai-codex").unwrap().is_some());
        store.set_codex(None).await.unwrap();
        assert!(store.credential("openai-codex").unwrap().is_none());
        assert!(
            !std::fs::read_to_string(store.path())
                .unwrap()
                .contains("test-refresh")
        );
    }

    #[tokio::test]
    async fn cancelled_login_cannot_restore_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let store = ProviderAuthStore::new(temp.path().join("auth.json"));
        store
            .set_codex(Some(credential_from_response(&token_response()).unwrap()))
            .await
            .unwrap();
        let manager = LoginManager::default();
        // No real OAuth request is made until a browser submits a valid code.
        let status = manager
            .start("owner".into(), "browser", store.clone())
            .await
            .unwrap();
        assert_eq!(status.phase, "waiting");
        if status.callback_available {
            let response = Client::new()
                .get("http://127.0.0.1:1455/auth/callback?code=unused&state=wrong")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(manager.status("owner").await.phase, "waiting");
        }
        assert_eq!(manager.status("different-owner").await.phase, "idle");
        assert!(
            manager
                .submit("different-owner", "secret".into())
                .await
                .is_err()
        );
        assert!(manager.cancel("different-owner").await.is_err());
        manager.cancel("owner").await.unwrap();
        assert!(store.credential("openai-codex").unwrap().is_some()); // cancelling preserves an old login
        assert!(manager.submit("owner", "secret".into()).await.is_err());
        manager.logout(store.clone()).await.unwrap();
        assert!(store.credential("openai-codex").unwrap().is_none());
    }

    #[tokio::test]
    async fn logout_waits_out_inflight_refresh_and_keeps_other_providers() {
        use fs2::FileExt;
        let temp = tempfile::tempdir().unwrap();
        let store = ProviderAuthStore::new(temp.path().join("auth.json"));
        store
            .set_codex(Some(credential_from_response(&token_response()).unwrap()))
            .await
            .unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp.path().join("auth.json.lock"))
            .unwrap();
        lock.lock_exclusive().unwrap();
        let deleting = {
            let store = store.clone();
            tokio::spawn(async move { store.set_codex(None).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!deleting.is_finished());
        let mut credentials = store.load().unwrap();
        credentials.insert(
            "other".into(),
            serde_json::from_value(json!({"type":"api_key","key":"preserved"})).unwrap(),
        );
        crate::config::atomic_private_json(store.path(), &credentials).unwrap();
        FileExt::unlock(&lock).unwrap();
        deleting.await.unwrap().unwrap();
        assert!(store.credential("openai-codex").unwrap().is_none());
        assert!(store.credential("other").unwrap().is_some());
    }

    #[test]
    fn incomplete_token_responses_never_echo_secrets() {
        let value = json!({"access_token":"secret-access","refresh_token":"secret-refresh"});
        let error = credential_from_response(&value).unwrap_err().to_string();
        assert!(!error.contains("secret-"));
    }
}
