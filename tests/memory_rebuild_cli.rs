//! Real CLI backfill against temporary SQLite and a synthetic loopback model.
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use qkbot::{config::Config, host::Host, types::now};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const CONVERSATION: &str = "1:group:2:v0:group";
const SOURCE: &str = "我今天完成了合成部署项目的验收。";
const MANUAL: &str = "管理员补充的旧偏好，必须原样保留。";

#[derive(Default)]
struct Peer {
    models: Mutex<Vec<Value>>,
    logins: AtomicUsize,
    unexpected: AtomicUsize,
}

async fn login(State(peer): State<Arc<Peer>>) -> Json<Value> {
    peer.logins.fetch_add(1, Ordering::SeqCst);
    Json(json!({"status":"ok","retcode":0,"data":{"uin":1}}))
}

async fn model(State(peer): State<Arc<Peer>>, Json(body): Json<Value>) -> Json<Value> {
    let input: Value = serde_json::from_str(
        body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let source = &input["sources"][0];
    let update = json!({"summary":"","changes":[{
        "op":"upsert","id":null,"subject_id":source["subject_id"],
        "kind":"event","text":"合成部署项目已完成验收。",
        "evidence":[{"source_key":source["key"],"quote":SOURCE}],"ttl_days":null
    }]});
    peer.models.lock().unwrap().push(body);
    Json(json!({"model":"fixture-model-reply","choices":[{
        "finish_reason":"stop","message":{"role":"assistant","content":update.to_string()}
    }],"usage":{"prompt_tokens":11,"completion_tokens":7}}))
}

async fn unexpected(State(peer): State<Arc<Peer>>) -> StatusCode {
    peer.unexpected.fetch_add(1, Ordering::SeqCst);
    StatusCode::NOT_FOUND
}

async fn cli(config: &Path, arguments: &[&str]) -> Vec<Value> {
    let config = config.to_owned();
    let arguments: Vec<_> = arguments.iter().map(|arg| arg.to_string()).collect();
    let output = tokio::task::spawn_blocking(move || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_qkbot"));
        command
            .arg("--config")
            .arg(&config)
            .args(arguments)
            .current_dir(config.parent().unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in [
            "QKBOT_API_KEY",
            "QKBOT_MILKY_TOKEN",
            "QKBOT_ADMIN_TOKEN",
            "QKBOT_LLBOT_TOKEN",
            "QKBOT_NONEBOT_TOKEN",
        ] {
            command.env_remove(name);
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "rebuild CLI did not exit: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn protected_snapshot(db: &Connection) -> Value {
    let live: String = db.query_row(
        "SELECT json_array(epoch,settings,summary,compacted_until,memory_processed_until) FROM ai_conversations WHERE id=?1",
        [CONVERSATION], |row| row.get(0),
    ).unwrap();
    let manual: String = db.query_row(
        "SELECT json_array(id,conversation_id,subject_id,kind,text,evidence,created_at,updated_at,expires_at,origin,active,locked,version,edited_by) FROM ai_memories WHERE origin='manual'",
        [], |row| row.get(0),
    ).unwrap();
    let mut statement = db.prepare(
        "SELECT json_array(id,scope,origin_plugin,plugin_epoch,conversation_id,context_epoch,body,state,detail,message_seq,created_at,updated_at,access_revision) FROM outbox ORDER BY id"
    ).unwrap();
    let outbox = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let mut statement = db
        .prepare("SELECT json_array(id,epoch,enabled) FROM plugin_states ORDER BY id")
        .unwrap();
    let plugins = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    json!({"live":live,"manual":manual,"outbox":outbox,"plugins":plugins})
}

#[tokio::test]
async fn rebuild_cli_resumes_without_replaying_or_starting_the_bot_runtime() {
    let temporary = tempfile::tempdir().unwrap();
    let database = temporary.path().join("history.sqlite3");
    let config_path = temporary.path().join("config.toml");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let peer = Arc::new(Peer::default());
    let app = Router::new()
        .route("/api/get_login_info", post(login))
        .route("/model", post(model))
        .fallback(unexpected)
        .with_state(peer.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut config = Config::default();
    config.app.database = database.to_string_lossy().into_owned();
    config.plugins.native.enabled.clear();
    config.milky.url = endpoint.clone();
    config.milky.request_timeout_secs = 3;
    config.llm.chat_completions_url = format!("{endpoint}/model");
    config.llm.model = "fixture-request-model".into();
    config.llm.request_timeout_secs = 3;
    config.plugins.ai.memory_output_budget = 2345;
    let host = Host::create(config.clone()).await.unwrap();
    host.store.call(|db| {
        db.execute("INSERT INTO ai_conversations(id,scope,mode,summary,compacted_until,memory_processed_until) VALUES(?1,'1:group:2','group','正常摘要必须保持',12,34)", [CONVERSATION])?;
        db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,learnable) VALUES('1:group:2:9','1:group:2',?1,1,7,'user',?2,?3,1,1)", params![CONVERSATION,SOURCE,now()-60])?;
        db.execute("INSERT INTO ai_memories(conversation_id,subject_id,kind,text,evidence,created_at,updated_at,origin,active,locked,version,edited_by) VALUES(?1,7,'preference',?2,'[]',1000,1001,'manual',1,1,7,'fixture-admin')",params![CONVERSATION,MANUAL])?;
        for state in ["prepared","submitted"] {
            db.execute("INSERT INTO outbox(id,scope,origin_plugin,plugin_epoch,body,state,detail,created_at,updated_at) VALUES(?1,'1:group:2','ai',1,'unchanged synthetic outbox',?1,'keep detail',100,101)",[state])?;
        }
        Ok(())
    }).await.unwrap();
    host.store.stop().await.unwrap();
    drop(host);
    config.app.database = "history.sqlite3".into();
    let config_text = toml::to_string(&config).unwrap();
    std::fs::write(&config_path, &config_text).unwrap();
    let before = protected_snapshot(&Connection::open(&database).unwrap());

    let args = ["--rebuild-memory", "test", "--rebuild-max-batches", "1"];
    let first = cli(&config_path, &args).await;
    let checkpoint = first
        .iter()
        .find(|event| event["event"] == "memory_rebuild_checkpoint")
        .unwrap();
    assert_eq!(
        checkpoint["status"],
        json!({"run_id":"test","jobs":1,"completed_jobs":1,"batches":1,"sources":1,"inserted":1,"duplicates":0,"expired":0})
    );
    assert_eq!(peer.models.lock().unwrap().len(), 1);
    let second = cli(&config_path, &args).await;
    assert!(
        second
            .iter()
            .any(|event| event["event"] == "memory_rebuild_complete"
                && event["status"] == checkpoint["status"])
    );
    assert_eq!(peer.models.lock().unwrap().len(), 1);
    let logins = peer.logins.load(Ordering::SeqCst);
    let status = cli(&config_path, &["--memory-rebuild-status", "test"]).await;
    assert_eq!(status.len(), 1);
    assert_eq!(status[0]["event"], "memory_rebuild_status");
    assert_eq!(status[0]["status"], checkpoint["status"]);
    assert_eq!(peer.logins.load(Ordering::SeqCst), logins);
    assert!(logins > 0);
    assert_eq!(peer.unexpected.load(Ordering::SeqCst), 0);

    let requests = peer.models.lock().unwrap();
    let request = &requests[0];
    assert_eq!(request["max_tokens"], 2345);
    assert_eq!(request["model"], "fixture-request-model");
    let system = request["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("人物背景：") && system.contains("聊天观察："));
    assert!(system.contains("existing_memories") && system.contains("只读"));
    let input: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(input["existing_memories_read_only"], true);
    assert_eq!(input["existing_memories"].as_array().unwrap().len(), 1);
    assert_eq!(input["existing_memories"][0]["text"], MANUAL);
    assert_eq!(input["existing_memories"][0]["origin"], "manual");
    drop(requests);

    let db = Connection::open(&database).unwrap();
    assert_eq!(protected_snapshot(&db), before);
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), config_text);
    let learned: (i64, String, String, String) = db
        .query_row(
            "SELECT subject_id,kind,text,evidence FROM ai_memories WHERE origin='auto'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (learned.0, learned.1.as_str(), learned.2.as_str()),
        (7, "event", "合成部署项目已完成验收。")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&learned.3).unwrap(),
        json!([{"source_key":"1:group:2:9","quote":SOURCE}])
    );
    let usage: (i64,i64,i64) = db.query_row("SELECT requests,input_tokens,output_tokens FROM usage_daily WHERE purpose='memory_rebuild' AND model='fixture-model-reply'",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!(usage, (1, 11, 7));
    assert_eq!(db.query_row("SELECT COUNT(*) FROM model_admissions WHERE purpose='background' AND conversation_id=?1",[CONVERSATION],|row|row.get::<_,i64>(0)).unwrap(),1);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM ai_memories", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    drop(db);
    server.abort();
}
