//! Learning-note administration uses authenticated local storage, never a model.
use axum::{Router, extract::Request, http::StatusCode as AxumStatus};
use qkbot::{
    admin::{self, AdminState, auth::AuthService, runtime::Runtime, settings::ConfigStore},
    config::Config,
};
use reqwest::{Client, Method, Response, StatusCode};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const SETUP: &str = "learning-test-administrator-setup-more-than-32-bytes";
struct Panel {
    _directory: tempfile::TempDir,
    url: String,
    client: Client,
    cookie: String,
    csrf: String,
    model_calls: Arc<AtomicUsize>,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}
impl Panel {
    async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let model_calls = Arc::new(AtomicUsize::new(0));
        let observed = model_calls.clone();
        let provider = Router::new().fallback(move |request: Request| {
            let observed = observed.clone();
            async move {
                if request.uri().path().contains("chat/completions") {
                    observed.fetch_add(1, Ordering::SeqCst);
                }
                AxumStatus::SERVICE_UNAVAILABLE
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_url = format!("http://{}", listener.local_addr().unwrap());
        let stop = cancel.clone();
        let provider_task = tokio::spawn(async move {
            axum::serve(listener, provider)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
                .unwrap();
        });
        let mut config = Config::from_toml(include_str!("../config.example.toml")).unwrap();
        config.app.database = directory
            .path()
            .join("learning.sqlite3")
            .to_string_lossy()
            .into_owned();
        config.plugins.ai.enabled = false;
        config.plugins.ai.learning.enabled = false;
        config.qq_login.enabled = false;
        config.embedding.enabled = false;
        config.nonebot.enabled = false;
        config.milky.url = provider_url.clone();
        config.milky.bot_id = Some(123);
        config.milky.request_timeout_secs = 1;
        config.llm.chat_completions_url = format!("{provider_url}/v1/chat/completions");
        config.web.token = SETUP.into();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, toml::to_string_pretty(&config).unwrap()).unwrap();
        let store = Arc::new(ConfigStore::open(&path).await.unwrap());
        let (effective, revision) = store.effective_with_revision().await.unwrap();
        let (runtime, worker) = Runtime::start(effective, revision, cancel.clone());
        let auth = AuthService::open(&path, SETUP, false).await.unwrap();
        let state = AdminState::new(store, runtime, auth);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = cancel.clone();
        let server = tokio::spawn(async move {
            admin::serve(listener, state, stop).await.unwrap();
        });
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let response = client
            .post(format!("{url}/api/auth/setup"))
            .json(&json!({"code":SETUP,"password":"learning administrator password 123"}))
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
        let auth: Value = response.json().await.unwrap();
        let panel = Self {
            _directory: directory,
            url,
            client,
            cookie,
            csrf: auth["csrf_token"].as_str().unwrap().into(),
            model_calls,
            cancel,
            tasks: vec![worker, server, provider_task],
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let response = panel
                    .request(Method::GET, "/api/learning")
                    .send()
                    .await
                    .unwrap();
                if response.status() == StatusCode::OK {
                    break;
                }
                assert_eq!(
                    response.status(),
                    StatusCode::SERVICE_UNAVAILABLE,
                    "learning unavailable: {}",
                    response.text().await.unwrap()
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        panel
    }
    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .header("x-csrf-token", &self.csrf)
    }
    async fn snapshot(&self) -> Value {
        let response = self
            .request(Method::GET, "/api/learning?bot_id=123")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }
    async fn mutate(&self, id: &str, revision: i64, action: Value) -> Response {
        self.request(Method::POST, "/api/learning/mutate")
            .json(&json!({"request_id":id,"bot_id":"123","revision":revision,"action":action}))
            .send()
            .await
            .unwrap()
    }
    async fn stop(self) {
        assert_eq!(
            self.model_calls.load(Ordering::SeqCst),
            0,
            "admin CRUD must not call a model"
        );
        self.cancel.cancel();
        for task in self.tasks {
            task.await.unwrap();
        }
    }
}

#[tokio::test]
async fn learning_routes_require_session_csrf_and_server_bound_account() {
    let panel = Panel::start().await;
    assert_eq!(
        panel
            .client
            .get(format!("{}/api/learning", panel.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let body = json!({"request_id":"missing-csrf","bot_id":"123","revision":0,"action":{"op":"add","kind":"experience","text":"manual note","scope":"shared","locked":true}});
    assert_eq!(
        panel
            .client
            .post(format!("{}/api/learning/mutate", panel.url))
            .header("cookie", &panel.cookie)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        panel
            .request(Method::GET, "/api/learning?bot_id=456")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let mut wrong = body.clone();
    wrong["bot_id"] = json!("456");
    assert_eq!(
        panel
            .request(Method::POST, "/api/learning/mutate")
            .json(&wrong)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let mut forged = body.clone();
    forged["admin_id"] = json!("attacker");
    assert_eq!(
        panel
            .request(Method::POST, "/api/learning/mutate")
            .json(&forged)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let scoped = json!({"op":"add","kind":"experience","text":"wrong scope","scope":"456:group:10","locked":true});
    assert_eq!(
        panel.mutate("wrong-scope", 0, scoped).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(panel.snapshot().await["notes"], json!([]));
    panel.stop().await;
}

#[tokio::test]
async fn learning_crud_receipts_revisions_history_and_rollback_work_without_model() {
    let panel = Panel::start().await;
    let initial = panel.snapshot().await;
    assert_eq!(initial["bot_id"], "123");
    assert_eq!(initial["learning_enabled"], false);
    assert_eq!(initial["revision"], 0);
    let action = json!({"op":"add","kind":"experience","text":"  人工笔记🐟  ","scope":"shared","locked":true});
    let response = panel.mutate("add-one", 0, action.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let receipt: Value = response.json().await.unwrap();
    let repeated: Value = panel
        .mutate("add-one", 0, action)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(repeated, receipt);
    let recorded: Value = panel
        .request(Method::GET, "/api/learning/operations/add-one?bot_id=123")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(recorded, receipt);
    let first = panel.snapshot().await;
    let note = &first["notes"][0];
    let id = note["id"].as_i64().unwrap();
    let history_id = first["history"][0]["id"].as_i64().unwrap();
    assert_eq!(note["text"], "人工笔记🐟");
    assert_eq!(note["locked"], true);
    assert_eq!(first["history"][0]["rollback_allowed"], true);
    let update = json!({"op":"update","id":id,"version":1,"kind":"term","text":"更新笔记","scope":"shared","locked":false});
    assert_eq!(
        panel.mutate("stale-edit", 0, update.clone()).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        panel.mutate("add-one", 1, update.clone()).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        panel.mutate("edit-one", 1, update).await.status(),
        StatusCode::OK
    );
    assert_eq!(panel.snapshot().await["notes"][0]["locked"], false);
    assert_eq!(
        panel
            .mutate(
                "lock-one",
                2,
                json!({"op":"lock","id":id,"version":2,"locked":true})
            )
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        panel
            .mutate("delete-one", 3, json!({"op":"delete","id":id,"version":3}))
            .await
            .status(),
        StatusCode::OK
    );
    let deleted = panel.snapshot().await;
    assert_eq!(deleted["notes"][0]["deleted"], true);
    assert!(
        deleted["history"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["rollback_allowed"] == true)
    );
    assert_eq!(
        panel
            .mutate(
                "rollback-one",
                4,
                json!({"op":"rollback","id":id,"version":4,"history_id":history_id})
            )
            .await
            .status(),
        StatusCode::OK
    );
    let restored = panel.snapshot().await;
    assert_eq!(restored["revision"], 5);
    assert_eq!(restored["notes"][0]["text"], "人工笔记🐟");
    assert_eq!(restored["notes"][0]["deleted"], false);
    assert_eq!(restored["notes"][0]["locked"], true);
    assert_eq!(restored["history"].as_array().unwrap().len(), 5);
    assert_eq!(panel.mutate("too-long", 5, json!({"op":"add","kind":"experience","text":"🐟".repeat(2001),"scope":"shared","locked":true})).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(panel.snapshot().await["revision"], 5);
    panel.stop().await;
}

#[tokio::test]
async fn personal_routes_bind_account_character_csrf_and_idempotent_revision() {
    let panel = Panel::start().await;
    assert_eq!(
        panel
            .client
            .get(format!("{}/api/personal", panel.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        panel
            .request(Method::GET, "/api/personal?bot_id=456")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let initial = panel
        .request(Method::GET, "/api/personal?bot_id=123&user_id=789")
        .send()
        .await
        .unwrap();
    assert_eq!(initial.status(), StatusCode::OK);
    let initial: Value = initial.json().await.unwrap();
    assert_eq!(initial["profile"]["revision"], 0);
    let mut body = json!({"bot_id":"123","user_id":"789","character_id":"default","revision":0,"request_id":"personal-reset","action":{"op":"reset"}});
    assert_eq!(
        panel
            .client
            .post(format!("{}/api/personal/mutate", panel.url))
            .header("cookie", &panel.cookie)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    for _ in 0..2 {
        let response = panel
            .request(Method::POST, "/api/personal/mutate")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.json::<Value>().await.unwrap()["revision"], 1);
    }
    body["request_id"] = json!("stale");
    assert_eq!(
        panel
            .request(Method::POST, "/api/personal/mutate")
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    body["revision"] = json!(1);
    body["character_id"] = json!("other");
    assert_eq!(
        panel
            .request(Method::POST, "/api/personal/mutate")
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    panel.stop().await;
}
