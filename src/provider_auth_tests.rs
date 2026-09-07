//! Real loopback refresh requests with disposable auth files and synthetic JWTs.
use super::*;
use axum::{Router, body::Bytes, response::IntoResponse, routing::post};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Notify, Semaphore};

fn token(account: &str) -> String {
    format!(
        "e30.{}.fixture",
        STANDARD.encode(
            serde_json::to_vec(&json!({OPENAI_CODEX_ACCOUNT_CLAIM:{"chatgpt_account_id":account}}))
                .unwrap()
        )
    )
}
async fn expired_store(path: &Path) -> ProviderAuthStore {
    let store = ProviderAuthStore::new(path.join("auth.json"));
    store
        .set_codex(Some(ProviderCredential::OAuth {
            access: token("old-account"),
            refresh: "old-refresh".into(),
            expires: 0,
            extra: [("accountId".into(), json!("old-account"))].into(),
        }))
        .await
        .unwrap();
    store
}
async fn server(
    body: Value,
) -> (
    String,
    Arc<AtomicUsize>,
    Arc<Notify>,
    Arc<Semaphore>,
    tokio::task::JoinHandle<()>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let (count, notify, gate) = (calls.clone(), entered.clone(), release.clone());
    let app = Router::new().route(
        "/token",
        post(move |request: Bytes| {
            let (count, notify, gate, body) =
                (count.clone(), notify.clone(), gate.clone(), body.clone());
            async move {
                let form: HashMap<String, String> =
                    url::form_urlencoded::parse(&request).into_owned().collect();
                assert_eq!(form["grant_type"], "refresh_token");
                assert_eq!(form["refresh_token"], "old-refresh");
                assert_eq!(form["client_id"], OPENAI_CODEX_CLIENT_ID);
                count.fetch_add(1, Ordering::SeqCst);
                notify.notify_one();
                let _permit = gate.acquire().await.unwrap();
                axum::Json(body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/token", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, calls, entered, release, task)
}
#[tokio::test]
async fn concurrent_refresh_rotates_once_and_validates_before_publication() {
    let tmp = tempfile::tempdir().unwrap();
    let store = expired_store(tmp.path()).await;
    let (url,calls,entered,release,server)=server(json!({"access_token":token("new-account"),"refresh_token":"new-refresh","expires_in":3600})).await;
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = store.clone();
        let url = url.clone();
        requests.spawn(async move {
            store
                .codex_access_at(&Client::builder().no_proxy().build().unwrap(), &url)
                .await
        });
    }
    entered.notified().await;
    release.add_permits(1);
    while let Some(result) = requests.join_next().await {
        assert_eq!(result.unwrap().unwrap().account_id, "new-account");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let stored =
        serde_json::to_value(store.credential(OPENAI_CODEX_PROVIDER_ID).unwrap().unwrap()).unwrap();
    assert_eq!(stored["accountId"], "new-account");
    assert_eq!(stored["refresh"], "new-refresh");
    server.abort();

    let tmp = tempfile::tempdir().unwrap();
    let store = expired_store(tmp.path()).await;
    let before = fs::read(store.path()).unwrap();
    let (url, _, _, release, server) = self::server(
        json!({"access_token":"invalid","refresh_token":"bad-refresh","expires_in":3600}),
    )
    .await;
    release.add_permits(1);
    let error = store
        .codex_access_at(&Client::new(), &url)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "OAuth refresh failed for openai-codex: Failed to extract accountId from token"
    );
    assert_eq!(fs::read(store.path()).unwrap(), before);
    server.abort();
}
#[tokio::test]
async fn logout_waits_for_real_refresh_and_cancellation_releases_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let store = expired_store(tmp.path()).await;
    let (url,_,entered,release,server)=server(json!({"access_token":token("new-account"),"refresh_token":"new-refresh","expires_in":3600})).await;
    let active_store = store.clone();
    let refresh =
        tokio::spawn(async move { active_store.codex_access_at(&Client::new(), &url).await });
    entered.notified().await;
    let logout_store = store.clone();
    let logout = tokio::spawn(async move { logout_store.set_codex(None).await });
    tokio::task::yield_now().await;
    assert!(!logout.is_finished());
    release.add_permits(1);
    refresh.await.unwrap().unwrap();
    logout.await.unwrap().unwrap();
    assert!(
        store
            .credential(OPENAI_CODEX_PROVIDER_ID)
            .unwrap()
            .is_none()
    );
    server.abort();

    let store = expired_store(tmp.path()).await;
    let (url, _, entered, _, server) = self::server(json!({})).await;
    let active_store = store.clone();
    let refresh =
        tokio::spawn(async move { active_store.codex_access_at(&Client::new(), &url).await });
    entered.notified().await;
    refresh.abort();
    assert!(refresh.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), store.set_codex(None))
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .credential(OPENAI_CODEX_PROVIDER_ID)
            .unwrap()
            .is_none()
    );
    server.abort();
}
#[tokio::test]
async fn pinned_refresh_responses_and_errors() {
    let fixture: Value =
        serde_json::from_str(include_str!("../tests/fixtures/pi-oauth-refresh.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let spec = case["spec"].clone();
        let expected = case["expected"].clone();
        let captured = Arc::new(tokio::sync::Mutex::new(String::new()));
        let target = captured.clone();
        let app = Router::new().route(
            "/token",
            post(move |bytes: Bytes| {
                let spec = spec.clone();
                let target = target.clone();
                async move {
                    *target.lock().await = String::from_utf8(bytes.to_vec()).unwrap();
                    let status = axum::http::StatusCode::from_u16(
                        spec["status"].as_u64().unwrap_or(200) as u16,
                    )
                    .unwrap();
                    (
                        status,
                        spec["raw"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| spec["body"].to_string()),
                    )
                        .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let before = chrono::Utc::now().timestamp_millis();
        let result = refresh_codex_token(&Client::new(), &url, "old-refresh").await;
        let after = chrono::Utc::now().timestamp_millis();
        assert_eq!(
            *captured.lock().await,
            case["request"]["body"].as_str().unwrap()
        );
        if let Some(error) = expected["error"].as_str() {
            assert_eq!(
                result.unwrap_err().to_string(),
                crate::provider_http::safe_error_body(
                    error,
                    &case["spec"]["body"]
                        .as_object()
                        .into_iter()
                        .flat_map(
                            |body| ["access_token", "refresh_token"].into_iter().filter_map(
                                |key| body.get(key).and_then(Value::as_str).map(str::to_owned)
                            )
                        )
                        .collect::<Vec<_>>()
                ),
                "{}",
                case["spec"]["name"]
            );
        } else {
            let value = serde_json::to_value(result.unwrap()).unwrap();
            for key in ["type", "access", "refresh", "accountId"] {
                assert_eq!(value[key], expected["credential"][key]);
            }
            let offset = expected["credential"]["expires"].as_i64().unwrap()
                - fixture["now"].as_i64().unwrap();
            assert!(value["expires"].as_i64().unwrap() >= before + offset);
            assert!(value["expires"].as_i64().unwrap() <= after + offset);
        }
        server.abort();
    }
}

#[test]
fn token_fields_are_redacted_in_truncated_and_embedded_errors() {
    for raw in [
        r#"{"access_token":"secret""#,
        r#"{"refresh_token":"secret"#,
        r#"Token response missing fields: {"access_token":"secret","expires_in":3600}"#,
    ] {
        let redacted = crate::provider_http::safe_error_body(raw, &[]);
        assert!(!redacted.contains("secret"), "{redacted}");
        assert!(redacted.contains("<redacted>"));
    }
}
