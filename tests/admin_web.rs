//! Real HTTP administration flow with an offline QQ transport and a temporary DB.
use std::{path::PathBuf, sync::Arc, time::Duration};

use qkbot::{
    admin::{self, AdminState, auth::AuthService, runtime::Runtime, settings::ConfigStore},
    config::Config,
};
use reqwest::{Client, Method, Response, StatusCode};
use rusqlite::Connection;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "integration-admin-token-more-than-thirty-two-bytes";
const FILE_MODEL: &str = "integration-file-model-secret";
const FILE_MILKY: &str = "integration-file-milky-secret";
const FILE_LLBOT: &str = "integration-file-login-secret";

struct Panel {
    _directory: tempfile::TempDir,
    path: PathBuf,
    database: PathBuf,
    url: String,
    client: Client,
    cookie: String,
    csrf: String,
    cancel: CancellationToken,
    worker: Option<JoinHandle<()>>,
    server: Option<JoinHandle<anyhow::Result<()>>>,
}

impl Panel {
    async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let database = directory.path().join("bot.sqlite3");
        let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let offline_url = format!("http://{}", unused.local_addr().unwrap());
        drop(unused);
        let mut config = Config::from_toml(include_str!("../config.example.toml")).unwrap();
        config.app.database = "bot.sqlite3".into();
        config.plugins.ai.enabled = false;
        config.web.token = TOKEN.into();
        config.milky.url = offline_url.clone();
        config.milky.access_token = Some(FILE_MILKY.into());
        config.milky.request_timeout_secs = 1;
        config.llm.chat_completions_url = format!("{offline_url}/v1/chat/completions");
        config.llm.api_key = FILE_MODEL.into();
        config.qq_login.url = offline_url;
        config.qq_login.token = FILE_LLBOT.into();
        std::fs::write(&path, toml::to_string_pretty(&config).unwrap()).unwrap();
        let store = Arc::new(ConfigStore::open(&path).await.unwrap());
        let (config, revision) = store.effective_with_revision().await.unwrap();
        let cancel = CancellationToken::new();
        let (runtime, worker) = Runtime::start(config, revision, cancel.clone());
        let auth = AuthService::open(&path, TOKEN, false).await.unwrap();
        let state = AdminState::new(store, runtime, auth);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = cancel.clone();
        let server = tokio::spawn(async move { admin::serve(listener, state, stop).await });
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let response = client
            .post(format!("{url}/api/auth/setup"))
            .json(&json!({"code":TOKEN,"password":"integration password 123"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let view: Value = response.json().await.unwrap();
        Self {
            _directory: directory,
            path,
            database,
            url,
            client,
            cookie,
            csrf: view["csrf_token"].as_str().unwrap().to_owned(),
            cancel,
            worker: Some(worker),
            server: Some(server),
        }
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .header("x-csrf-token", &self.csrf)
    }

    async fn snapshot(&self) -> Value {
        let response = self
            .request(Method::GET, "/api/config")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    async fn save(&self, snapshot: &Value, secrets: Value) -> Response {
        self.request(Method::PUT, "/api/config")
            .json(&json!({
                "revision": snapshot["revision"],
                "config": snapshot["config"],
                "secrets": secrets,
            }))
            .send()
            .await
            .unwrap()
    }

    async fn phase(&self, expected: &str, revision: Option<&str>) -> Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let response = self
                    .request(Method::GET, "/api/status")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let status: Value = response.json().await.unwrap();
                if status["phase"] == expected
                    && revision.is_none_or(|revision| status["applied_revision"] == revision)
                {
                    return status;
                }
                assert_ne!(status["phase"], "error", "runtime failed: {status}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("admin runtime did not reach expected phase")
    }

    fn stored(&self) -> Config {
        Config::load_raw(&self.path).unwrap()
    }

    fn clock_epoch(&self) -> i64 {
        Connection::open(&self.database)
            .unwrap()
            .query_row(
                "SELECT epoch FROM plugin_states WHERE id='clock'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn assert_database_closed(&self) {
        for suffix in ["-wal", "-shm"] {
            assert!(
                !PathBuf::from(format!("{}{suffix}", self.database.display())).exists(),
                "SQLite connection must close before stop completes"
            );
        }
    }

    async fn shutdown(mut self) {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(10), self.worker.take().unwrap())
            .await
            .expect("bot supervisor did not stop")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), self.server.take().unwrap())
            .await
            .expect("HTTP administration server did not stop")
            .unwrap()
            .unwrap();
        self.assert_database_closed();
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[tokio::test]
async fn sessions_csrf_logout_and_offline_web_memory_transactions() {
    let panel = Panel::start().await;
    panel.phase("waiting_login", None).await;
    for path in ["/api/runtime/stop", "/api/llm/models"] {
        let response = panel
            .client
            .post(format!("{}{path}", panel.url))
            .header("cookie", &panel.cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.json::<Value>().await.unwrap()["code"],
            "csrf_invalid"
        );
    }
    let response = panel
        .request(Method::POST, "/api/auth/setup")
        .json(&json!({"code":TOKEN,"password":"another password 123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    panel
        .request(Method::POST, "/api/runtime/stop")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    panel.phase("stopped", None).await;
    {
        let db = Connection::open(&panel.database).unwrap();
        db.execute(
            "INSERT INTO ai_conversations(id,scope,mode) VALUES('web-c','123:group:456','group')",
            [],
        )
        .unwrap();
    }
    let req = json!({"request_id":uuid::Uuid::new_v4().to_string(),"conversation_id":"web-c","epoch":1,"memory_revision":0,"action":{"op":"create","subject_id":789,"kind":"preference","text":"喜欢白茶"}});
    let create = panel
        .request(Method::POST, "/api/memory/mutate")
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::OK);
    let result: Value = create.json().await.unwrap();
    let again: Value = panel
        .request(Method::POST, "/api/memory/mutate")
        .json(&req)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result, again);
    let mut conflict = req.clone();
    conflict["request_id"] = json!(uuid::Uuid::new_v4().to_string());
    assert_eq!(
        panel
            .request(Method::POST, "/api/memory/mutate")
            .json(&conflict)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let listing: Value = panel
        .request(Method::GET, "/api/memory/records?conversation_id=web-c")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["total"], 1);
    assert_eq!(listing["items"][0]["origin"], "manual");
    assert_eq!(listing["items"][0]["locked"], true);
    panel
        .request(Method::POST, "/api/auth/logout")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        panel
            .request(Method::GET, "/api/memory/conversations")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    panel.shutdown().await;
}

fn assert_secure(response: &Response) {
    let headers = response.headers();
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["x-frame-options"], "DENY");
    assert_eq!(headers["referrer-policy"], "no-referrer");
    let csp = headers["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("default-src 'none'") && csp.contains("frame-ancestors 'none'"));
    assert!(!headers.contains_key("access-control-allow-origin"));
}

fn assert_redacted(snapshot: &Value) {
    let encoded = snapshot.to_string();
    for secret in [
        TOKEN,
        FILE_MODEL,
        FILE_MILKY,
        FILE_LLBOT,
        "replacement-model-secret",
        "replacement-login-secret",
    ] {
        assert!(!encoded.contains(secret));
    }
    assert_eq!(snapshot["config"]["llm"]["api_key"], "");
    assert!(snapshot["config"]["milky"]["access_token"].is_null());
    assert!(snapshot["config"]["web"].get("token").is_none());
    assert_eq!(snapshot["config"]["qq_login"]["token"], "");
}

#[tokio::test]
async fn offline_bot_keeps_panel_available_and_every_api_requires_authentication() {
    let panel = Panel::start().await;
    let status = panel.phase("waiting_login", None).await;
    assert_eq!(status["online"], false);
    assert!(status["bot_id"].is_null());
    for path in ["/", "/app.js", "/style.css"] {
        let response = panel
            .client
            .get(format!("{}{path}", panel.url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_secure(&response);
        let body = response.text().await.unwrap();
        assert!(!body.is_empty() && !body.contains(TOKEN) && !body.contains(FILE_MILKY));
    }
    for (method, path) in [
        (Method::GET, "/api/status"),
        (Method::GET, "/api/config"),
        (Method::PUT, "/api/config"),
        (Method::POST, "/api/llm/models"),
        (Method::POST, "/api/runtime/apply"),
        (Method::POST, "/api/runtime/stop"),
        (Method::GET, "/api/qq/status"),
        (Method::POST, "/api/qq/qrcode"),
        (Method::GET, "/api/nonexistent"),
    ] {
        for credential in [None, Some("incorrect-token"), Some(TOKEN)] {
            let mut request = panel
                .client
                .request(method.clone(), format!("{}{path}", panel.url));
            if let Some(token) = credential {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path}"
            );
            assert_secure(&response);
        }
    }
    let snapshot = panel.snapshot().await;
    assert_redacted(&snapshot);
    for name in ["llm_api_key", "milky_access_token", "llbot_token"] {
        assert_eq!(snapshot["secrets"][name]["file_set"], true);
    }
    for (name, value) in [
        ("origin", "http://other.invalid"),
        ("origin", "null"),
        ("sec-fetch-site", "cross-site"),
    ] {
        let response = panel
            .request(Method::GET, "/api/config")
            .header(name, value)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_secure(&response);
    }
    let response = panel
        .request(Method::GET, "/api/config")
        .header("origin", &panel.url)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_secure(&response);
    panel.shutdown().await;
}

#[tokio::test]
async fn http_config_save_preserves_clears_sets_secrets_and_rejects_external_revision_conflicts() {
    let panel = Panel::start().await;
    panel.phase("waiting_login", None).await;
    let mut snapshot = panel.snapshot().await;
    snapshot["config"]["app"]["timezone"] = json!("UTC");
    let response = panel.save(&snapshot, json!({"llm_api_key":{"action":"keep"},"milky_access_token":{"action":"keep"},"llbot_token":{"action":"keep"}})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let kept: Value = response.json().await.unwrap();
    assert_redacted(&kept);
    let stored = panel.stored();
    assert_eq!(stored.app.timezone, "UTC");
    assert_eq!(stored.llm.api_key, FILE_MODEL);
    assert_eq!(stored.milky.access_token.as_deref(), Some(FILE_MILKY));
    assert_eq!(stored.qq_login.token, FILE_LLBOT);
    assert_eq!(stored.web.token, TOKEN);

    let response = panel.save(&kept, json!({"llm_api_key":{"action":"set","value":"replacement-model-secret"},"milky_access_token":{"action":"clear"},"llbot_token":{"action":"set","value":"replacement-login-secret"}})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let replaced: Value = response.json().await.unwrap();
    assert_redacted(&replaced);
    assert_eq!(replaced["secrets"]["milky_access_token"]["file_set"], false);
    let stored = panel.stored();
    assert_eq!(stored.llm.api_key, "replacement-model-secret");
    assert_eq!(stored.milky.access_token, None);
    assert_eq!(stored.qq_login.token, "replacement-login-secret");

    let response = panel.save(&replaced, json!({"llm_api_key":{"action":"clear"},"llbot_token":{"action":"clear"},"milky_access_token":{"action":"set","value":"replacement-milky-secret"}})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let latest: Value = response.json().await.unwrap();
    assert_redacted(&latest);
    assert!(!latest.to_string().contains("replacement-milky-secret"));
    let stored = panel.stored();
    assert!(stored.llm.api_key.is_empty() && stored.qq_login.token.is_empty());
    assert_eq!(
        stored.milky.access_token.as_deref(),
        Some("replacement-milky-secret")
    );
    assert_eq!(stored.web.token, TOKEN);

    // A valid file may produce much more than 256 KiB of JSON. Saving a full
    // GET snapshot must remain possible without dropping unrelated settings.
    let mut large = panel.snapshot().await;
    for index in 0..40 {
        large["config"]["plugins"]["ai"]["personas"]
            .as_object_mut()
            .unwrap()
            .insert(
                format!("large-persona-{index}"),
                json!(format!("persona-{index:02}:{}", "x".repeat(8 * 1024 - 11))),
            );
    }
    let body_size = serde_json::to_vec(&json!({
        "revision": large["revision"], "config": large["config"], "secrets": {}
    }))
    .unwrap()
    .len();
    assert!(body_size > 256 * 1024 && body_size < 1024 * 1024);
    let response = panel.save(&large, json!({})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let saved_large: Value = response.json().await.unwrap();
    assert_redacted(&saved_large);
    let latest = panel.snapshot().await;
    assert_eq!(latest["config"], large["config"]);
    assert_eq!(latest["revision"], saved_large["revision"]);
    assert!(std::fs::metadata(&panel.path).unwrap().len() < 1024 * 1024);

    let mut external = std::fs::read(&panel.path).unwrap();
    external.extend_from_slice(b"\n# external administrator edit\n");
    std::fs::write(&panel.path, &external).unwrap();
    let response = panel.save(&latest, json!({})).await;
    assert!(response.status().is_client_error());
    assert!(
        response.json::<Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("修改")
    );
    assert_eq!(std::fs::read(&panel.path).unwrap(), external);
    assert_ne!(panel.snapshot().await["revision"], latest["revision"]);
    panel.shutdown().await;
}

#[tokio::test]
async fn malformed_partial_and_oversized_requests_never_replace_configuration() {
    let panel = Panel::start().await;
    panel.phase("waiting_login", None).await;
    let original = std::fs::read(&panel.path).unwrap();
    let snapshot = panel.snapshot().await;
    let mut incomplete = json!({"revision": snapshot["revision"], "config": snapshot["config"]});
    incomplete["config"]["limits"]
        .as_object_mut()
        .unwrap()
        .remove("retention_days");
    let mut unknown = json!({"revision": snapshot["revision"], "config": snapshot["config"]});
    unknown["unexpected"] = json!(true);
    let mut plaintext = json!({"revision": snapshot["revision"], "config": snapshot["config"]});
    plaintext["config"]["llm"]["api_key"] = json!("must-not-be-reflected-secret");
    let empty_set = json!({"revision":snapshot["revision"],"config":snapshot["config"],"secrets":{"llm_api_key":{"action":"set","value":""}}});
    // This is otherwise valid JSON and a complete valid request: rejection
    // must come from the body limit, rather than from malformed JSON syntax.
    let oversized = format!(
        "{}{}",
        json!({"revision": snapshot["revision"], "config": snapshot["config"]}),
        " ".repeat(4 * 1024 * 1024 + 1)
    );
    for body in [
        "{".to_owned(),
        incomplete.to_string(),
        unknown.to_string(),
        plaintext.to_string(),
        empty_set.to_string(),
        oversized,
    ] {
        let response = panel
            .request(Method::PUT, "/api/config")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_client_error());
        assert_secure(&response);
        assert!(
            !response
                .text()
                .await
                .unwrap()
                .contains("must-not-be-reflected-secret")
        );
        assert_eq!(std::fs::read(&panel.path).unwrap(), original);
    }
    let response = panel
        .request(Method::PUT, "/api/config")
        .header("content-type", "text/plain")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_client_error());
    assert_eq!(std::fs::read(&panel.path).unwrap(), original);
    panel.shutdown().await;
}

#[tokio::test]
async fn apply_and_stop_share_database_and_preserve_durable_data() {
    let panel = Panel::start().await;
    let mut snapshot = panel.snapshot().await;
    let old_revision = snapshot["revision"].as_str().unwrap().to_owned();
    panel.phase("waiting_login", Some(&old_revision)).await;
    let original_epoch = panel.clock_epoch();
    {
        let connection = Connection::open(&panel.database).unwrap();
        connection.execute_batch("CREATE TABLE admin_restart_fixture(note TEXT NOT NULL); INSERT INTO admin_restart_fixture VALUES('persistent memory fixture');").unwrap();
        connection.execute("INSERT INTO outbox(id,scope,origin_plugin,plugin_epoch,body,state,created_at,updated_at) VALUES('restart-fixture','test','clock',?1,'test','prepared',1,1)", [original_epoch]).unwrap();
    }
    snapshot["config"]["app"]["timezone"] = json!("UTC");
    let response = panel.save(&snapshot, json!({})).await;
    assert_eq!(response.status(), StatusCode::OK);
    snapshot = response.json().await.unwrap();
    let new_revision = snapshot["revision"].as_str().unwrap().to_owned();
    assert_ne!(new_revision, old_revision);
    // Saving is separate from applying: the old runtime retains its revision.
    panel.phase("waiting_login", Some(&old_revision)).await;
    assert_eq!(panel.clock_epoch(), original_epoch);
    let response = panel
        .request(Method::POST, "/api/runtime/apply")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    panel.phase("waiting_login", Some(&new_revision)).await;
    assert_eq!(panel.clock_epoch(), original_epoch + 2);
    {
        let connection = Connection::open(&panel.database).unwrap();
        let note: String = connection
            .query_row("SELECT note FROM admin_restart_fixture", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(note, "persistent memory fixture");
        let state: String = connection
            .query_row(
                "SELECT state FROM outbox WHERE id='restart-fixture'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            state, "failed",
            "restart recovers a positively unsubmitted outbox item"
        );
    }
    let response = panel
        .request(Method::POST, "/api/runtime/stop")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    panel.phase("stopped", None).await;
    let response = panel
        .request(Method::GET, "/api/memory/conversations")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = panel
        .request(Method::POST, "/api/runtime/apply")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    panel.phase("waiting_login", Some(&new_revision)).await;
    assert_eq!(panel.clock_epoch(), original_epoch + 4);
    let note: String = Connection::open(&panel.database)
        .unwrap()
        .query_row("SELECT note FROM admin_restart_fixture", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(note, "persistent memory fixture");
    panel.shutdown().await;
}
