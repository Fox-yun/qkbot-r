//! Bulk group personas use fresh membership and authenticated, idempotent HTTP mutations.
//! All accounts, messages, templates and transport responses are synthetic.
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{Json, Router, extract::State, routing::post};
use qkbot::{
    admin::{self, AdminState, auth::AuthService, runtime::Runtime, settings::ConfigStore},
    config::Config,
    store::Store,
    types::now,
};
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use rusqlite::params;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const SETUP: &str = "group-persona-integration-setup-code-long-enough";
const CUSTOM: &str = "测试自定义";
const ENDPOINT: &str = "/api/ai/groups/persona";

struct MilkyState {
    account: AtomicI64,
    groups: Mutex<Vec<i64>>,
    group_reads: AtomicUsize,
    switch_account_on_groups: AtomicBool,
    sends: AtomicUsize,
}

async fn login(State(state): State<Arc<MilkyState>>) -> Json<Value> {
    Json(json!({"status":"ok","retcode":0,"data":{
        "uin":state.account.load(Ordering::SeqCst)
    }}))
}

async fn groups(State(state): State<Arc<MilkyState>>, Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(body["no_cache"], true, "mutations must refresh membership");
    state.group_reads.fetch_add(1, Ordering::SeqCst);
    let groups: Vec<_> = state
        .groups
        .lock()
        .unwrap()
        .iter()
        .map(|id| json!({"group_id":id,"group_name":"模拟群","remark":""}))
        .collect();
    if state.switch_account_on_groups.swap(false, Ordering::SeqCst) {
        state.account.store(303, Ordering::SeqCst);
    }
    Json(json!({"status":"ok","retcode":0,"data":{"groups":groups}}))
}

struct Panel {
    _directory: tempfile::TempDir,
    url: String,
    client: Client,
    cookie: String,
    csrf: String,
    store: Store,
    milky: Arc<MilkyState>,
    cancel: CancellationToken,
    upstream_cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl Panel {
    async fn start() -> Self {
        let milky = Arc::new(MilkyState {
            account: AtomicI64::new(101),
            groups: Mutex::new(vec![202, 203, 204]),
            group_reads: AtomicUsize::new(0),
            switch_account_on_groups: AtomicBool::new(false),
            sends: AtomicUsize::new(0),
        });
        let upstream = Router::new()
            .route("/api/get_login_info", post(login))
            .route("/api/get_group_list", post(groups))
            .route(
                "/api/get_friend_list",
                post(|| async {
                    Json(json!({"status":"ok","retcode":0,"data":{
                        "friends":[{"user_id":202,"nickname":"模拟好友","remark":""}]
                    }}))
                }),
            )
            .route(
                "/api/send_group_message",
                post(|State(state): State<Arc<MilkyState>>| async move {
                    state.sends.fetch_add(1, Ordering::SeqCst);
                    StatusCode::INTERNAL_SERVER_ERROR
                }),
            )
            .with_state(milky.clone());
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url = format!("http://{}", upstream_listener.local_addr().unwrap());
        let upstream_cancel = CancellationToken::new();
        let stop = upstream_cancel.clone();
        let upstream_task = tokio::spawn(async move {
            axum::serve(upstream_listener, upstream)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
                .unwrap();
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = Config::default();
        config.app.database = directory
            .path()
            .join("bot.sqlite3")
            .to_string_lossy()
            .into_owned();
        config.web.token = SETUP.into();
        config.plugins.ai.enabled = false;
        config.plugins.ai.personas.insert(
            CUSTOM.into(),
            "这是模拟服务中已应用的自定义人设，不调用任何模型。".into(),
        );
        config.plugins.native.enabled.clear();
        config.milky.url = upstream_url;
        config.milky.request_timeout_secs = 1;
        let media = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        config.nonebot.listen = media.local_addr().unwrap().to_string();
        config.nonebot.media_dir = directory.path().join("media").to_string_lossy().into();
        drop(media);
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        let settings = Arc::new(ConfigStore::open(&path).await.unwrap());
        let (config, revision) = settings.effective_with_revision().await.unwrap();
        let cancel = CancellationToken::new();
        let (runtime, worker) = Runtime::start(config, revision, cancel.clone());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let status = runtime.status().await;
                if status.applied_revision.is_some() {
                    break;
                }
                assert_ne!(status.phase, "error", "fixture runtime failed");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fixture runtime did not start");
        let store = runtime.data().await.unwrap();
        let auth = AuthService::open(&path, SETUP, false).await.unwrap();
        let state = AdminState::new(settings, runtime, auth);
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
        let setup = client
            .post(format!("{url}/api/auth/setup"))
            .json(&json!({"code":SETUP,"password":"group persona fixture password"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let cookie = setup.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let csrf = setup.json::<Value>().await.unwrap()["csrf_token"]
            .as_str()
            .unwrap()
            .to_owned();
        let panel = Self {
            _directory: directory,
            url,
            client,
            cookie,
            csrf,
            store,
            milky,
            cancel,
            upstream_cancel,
            tasks: vec![server, worker, upstream_task],
        };
        panel.seed().await;
        panel
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .header("x-csrf-token", &self.csrf)
    }

    fn change(&self, id: &str, bot: &str, persona: &str) -> RequestBuilder {
        self.request(Method::POST, ENDPOINT)
            .json(&json!({"request_id":id,"bot_id":bot,"persona":persona}))
    }

    async fn seed(&self) {
        self.store.call(|db| {
            let timestamp = now();
            for (id, scope, mode, owner) in [
                ("current-group", "101:group:202", "group", None),
                ("current-member", "101:group:202", "user", Some(7)),
                ("departed-group", "101:group:203", "group", None),
                ("private-chat", "101:friend:202", "user", Some(7)),
                ("other-account", "303:group:202", "group", None),
            ] {
                db.execute(
                    "INSERT INTO ai_conversations(id,scope,mode,owner_id,settings,summary,compacted_until,memory_processed_until)
                     VALUES(?1,?2,?3,?4,?5,'保留摘要',5,5)",
                    params![id,scope,mode,owner,json!({"persona":"雄小鬼","recent_messages":12}).to_string()],
                )?;
                db.execute(
                    "INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position)
                     VALUES(?1,?2,?3,1,7,'user','保留历史',?4,5)",
                    params![format!("{id}:message"),scope,id,timestamp],
                )?;
                db.execute(
                    "INSERT INTO ai_memories(conversation_id,subject_id,kind,text,evidence,created_at,updated_at,origin,locked)
                     VALUES(?1,7,'fact','保留人工记忆','[]',?2,?2,'manual',1)",
                    params![id,timestamp],
                )?;
            }
            Ok(())
        }).await.unwrap();
    }

    async fn snapshot(&self) -> Value {
        self.store.call(|db| {
            let conversations = db.prepare(
                "SELECT id,scope,settings,epoch,memory_revision,summary,compacted_until,memory_processed_until
                 FROM ai_conversations ORDER BY id"
            )?.query_map([], |row| Ok(json!({
                "id":row.get::<_,String>(0)?,"scope":row.get::<_,String>(1)?,
                "settings":serde_json::from_str::<Value>(&row.get::<_,String>(2)?).unwrap(),
                "epoch":row.get::<_,i64>(3)?,"revision":row.get::<_,i64>(4)?,
                "summary":row.get::<_,String>(5)?,"compacted":row.get::<_,i64>(6)?,
                "processed":row.get::<_,i64>(7)?
            })))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let seeds = db.prepare("SELECT scope,persona FROM ai_group_personas ORDER BY scope")?
                .query_map([], |r|Ok(json!({"scope":r.get::<_,String>(0)?,"persona":r.get::<_,String>(1)?})))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let messages = db.prepare("SELECT conversation_id,epoch,content FROM messages ORDER BY conversation_id")?
                .query_map([], |r|Ok(json!({"id":r.get::<_,String>(0)?,"epoch":r.get::<_,i64>(1)?,"content":r.get::<_,String>(2)?})))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let memories = db.prepare("SELECT id,conversation_id,text,origin,locked FROM ai_memories ORDER BY id")?
                .query_map([], |r|Ok(json!({"id":r.get::<_,i64>(0)?,"conversation":r.get::<_,String>(1)?,
                    "text":r.get::<_,String>(2)?,"origin":r.get::<_,String>(3)?,"locked":r.get::<_,bool>(4)?})))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let receipts:i64=db.query_row("SELECT count(*) FROM ai_memory_operations",[],|r|r.get(0))?;
            Ok(json!({"conversations":conversations,"seeds":seeds,"messages":messages,"memories":memories,"receipts":receipts}))
        }).await.unwrap()
    }

    async fn shutdown(mut self) {
        assert_eq!(self.milky.sends.load(Ordering::SeqCst), 0);
        self.cancel.cancel();
        self.upstream_cancel.cancel();
        for task in self.tasks.drain(..) {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("fixture task did not stop")
                .unwrap();
        }
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.upstream_cancel.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn by_id<'a>(snapshot: &'a Value, id: &str) -> &'a Value {
    snapshot["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap()
}

#[tokio::test]
async fn group_persona_http_auth_fresh_membership_isolation_and_idempotency() {
    let panel = Panel::start().await;
    let before = panel.snapshot().await;
    let body = json!({"request_id":"bulk-one","bot_id":"101","persona":CUSTOM});
    assert_eq!(
        panel
            .client
            .post(format!("{}{ENDPOINT}", panel.url))
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        panel
            .client
            .post(format!("{}{ENDPOINT}", panel.url))
            .header("cookie", &panel.cookie)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(panel.milky.group_reads.load(Ordering::SeqCst), 0);
    assert_eq!(panel.snapshot().await, before);
    assert_eq!(
        panel
            .client
            .get(format!("{}/api/memory/operations/bulk-one", panel.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );

    // The UI can have an old cached membership list. Mutation must query again.
    let listed: Value = panel
        .request(Method::GET, "/api/qq/contacts?force=true")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["groups"].as_array().unwrap().len(), 3);
    *panel.milky.groups.lock().unwrap() = vec![202, 204];
    let result: Value = panel
        .change("bulk-one", "101", CUSTOM)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["affected_groups"], 2);
    assert_eq!(result["affected_conversations"], 2);
    assert_eq!(result["scopes"], json!(["101:group:202", "101:group:204"]));
    assert_eq!(panel.milky.group_reads.load(Ordering::SeqCst), 2);
    let after = panel.snapshot().await;
    for id in ["current-group", "current-member"] {
        let old = by_id(&before, id);
        let new = by_id(&after, id);
        assert_eq!(new["settings"]["persona"], CUSTOM);
        assert_eq!(new["settings"]["recent_messages"], 12);
        assert_eq!(
            new["epoch"].as_i64().unwrap(),
            old["epoch"].as_i64().unwrap() + 1
        );
        assert_eq!(
            new["revision"].as_i64().unwrap(),
            old["revision"].as_i64().unwrap() + 1
        );
        for field in ["scope", "summary", "compacted", "processed"] {
            assert_eq!(new[field], old[field], "{id}: {field}");
        }
    }
    for id in ["departed-group", "private-chat", "other-account"] {
        assert_eq!(by_id(&after, id), by_id(&before, id));
    }
    assert_eq!(after["memories"], before["memories"]);
    assert_eq!(
        after["seeds"],
        json!([
            {"scope":"101:group:202","persona":CUSTOM},
            {"scope":"101:group:204","persona":CUSTOM}
        ])
    );
    for message in after["messages"].as_array().unwrap() {
        assert_eq!(message["content"], "保留历史");
        let updated = matches!(
            message["id"].as_str().unwrap(),
            "current-group" | "current-member"
        );
        assert_eq!(message["epoch"], if updated { 2 } else { 1 });
    }
    assert_eq!(after["receipts"], 1);

    let receipt: Value = panel
        .request(Method::GET, "/api/memory/operations/bulk-one")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(receipt, result);
    // A completed request cannot reapply to new groups or a replacement account.
    panel.milky.account.store(303, Ordering::SeqCst);
    *panel.milky.groups.lock().unwrap() = vec![999];
    let calls = panel.milky.group_reads.load(Ordering::SeqCst);
    let retried: Value = panel
        .change("bulk-one", "101", CUSTOM)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(retried, result);
    assert_eq!(panel.milky.group_reads.load(Ordering::SeqCst), calls);
    assert_eq!(panel.snapshot().await, after);
    assert_eq!(
        panel
            .change("bulk-one", "101", "雄小鬼")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(panel.snapshot().await, after);
    panel.shutdown().await;
}

#[tokio::test]
async fn group_persona_account_mismatch_switch_and_unknown_template_never_write() {
    let panel = Panel::start().await;
    let before = panel.snapshot().await;
    assert_eq!(
        panel
            .change("wrong-account", "303", CUSTOM)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(panel.snapshot().await, before);
    assert_eq!(
        panel
            .change("unknown-template", "101", "不存在的人设")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(panel.snapshot().await, before);

    // Account changes after the list starts, before Milky's post-list identity fence.
    panel
        .milky
        .switch_account_on_groups
        .store(true, Ordering::SeqCst);
    assert_eq!(
        panel
            .change("mid-query-switch", "101", CUSTOM)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(panel.snapshot().await, before);
    for id in ["wrong-account", "unknown-template", "mid-query-switch"] {
        let receipt: Value = panel
            .request(Method::GET, &format!("/api/memory/operations/{id}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(receipt["status"], "not_found");
    }
    panel.shutdown().await;
}
