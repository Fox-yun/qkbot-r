//! Scenario evaluation for memory and context: scripted conversations run
//! through the real router, the confirmed-send ledger and an HTTP model stub.
//!
//! Default mode is deterministic. For each turn it checks what reached the
//! model: expected material must be present (recall) and forbidden material
//! absent (leaks). Scenarios live in `tests/eval/*.json`.
//!
//! Optional real-model mode, ignored by normal runs:
//!   QKBOT_EVAL_MODEL_URL=https://…/v1/chat/completions QKBOT_EVAL_MODEL=… \
//!   QKBOT_EVAL_API_KEY=… QKBOT_EVAL_OUT=./eval-out \
//!   cargo test --test eval_scenarios -- --ignored --nocapture
//! It forwards each captured request to that model, writes transcripts, and
//! with QKBOT_EVAL_JUDGE=1 asks the same model to score naturalness, persona
//! consistency, appropriate memory use and privacy for each scenario.
use axum::{Json, Router as HttpRouter, extract::State, routing::post};
use qkbot::{
    config::Config,
    host::{Host, Router},
    types::{Incoming, MessageKey, Scene, Scope, now},
};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const BOT: i64 = 123;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    name: String,
    #[allow(dead_code)]
    description: String,
    #[serde(default)]
    config: ScenarioConfig,
    #[serde(default)]
    memories: Vec<SeedMemory>,
    #[serde(default)]
    knowledge: Vec<SeedKnowledge>,
    turns: Vec<Turn>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioConfig {
    #[serde(default)]
    group_public_recall: bool,
    #[serde(default)]
    cross_scene: Option<qkbot::config::CrossScenePolicy>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeedMemory {
    subject: i64,
    scene: String,
    peer: i64,
    kind: String,
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeedKnowledge {
    kind: String,
    subject: String,
    subject_id: i64,
    scene: String,
    peer: i64,
    text: String,
    #[serde(default)]
    revoked: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Turn {
    scene: String,
    peer: i64,
    sender: i64,
    text: String,
    #[serde(default)]
    mentions: Vec<i64>,
    #[serde(default)]
    expect: Vec<String>,
    #[serde(default)]
    forbid: Vec<String>,
    #[serde(default)]
    reply: Option<String>,
}

fn scene(name: &str) -> Scene {
    match name {
        "group" => Scene::Group,
        "friend" => Scene::Friend,
        other => panic!("unknown scene {other}"),
    }
}

fn scope(name: &str, peer: i64) -> Scope {
    Scope {
        bot_id: BOT,
        scene: scene(name),
        peer_id: peer,
    }
}

/// Same identifiers the router uses for shared groups and private chats.
fn conversation_id(scope: &Scope) -> (String, &'static str, Option<i64>) {
    match scope.scene {
        Scene::Group => (format!("{}:v0:group", scope.key()), "group", None),
        Scene::Friend => (
            format!("{}:v0:user:{}", scope.key(), scope.peer_id),
            "user",
            Some(scope.peer_id),
        ),
    }
}

struct ModelRequest {
    body: Value,
    reply: oneshot::Sender<Value>,
}

#[derive(Clone)]
struct Peer {
    models: mpsc::Sender<ModelRequest>,
    sends: mpsc::Sender<Value>,
    sequence: Arc<AtomicI64>,
}

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), future)
        .await
        .expect("evaluation step timeout")
}

async fn complete(State(peer): State<Peer>, Json(body): Json<Value>) -> Json<Value> {
    let (reply, receive) = oneshot::channel();
    peer.models
        .send(ModelRequest { body, reply })
        .await
        .unwrap();
    Json(receive.await.unwrap())
}

async fn send(State(peer): State<Peer>, Json(body): Json<Value>) -> Json<Value> {
    peer.sends.send(body).await.unwrap();
    Json(json!({"status":"ok","retcode":0,"data":{
        "message_seq":peer.sequence.fetch_add(1,Ordering::Relaxed),"time":now()
    }}))
}

fn answer(text: &str) -> Value {
    json!({"model":"synthetic","choices":[{"finish_reason":"stop","message":{
        "role":"assistant","content":text
    }}],"usage":{"prompt_tokens":30,"completion_tokens":10}})
}

struct Rig {
    host: Arc<Host>,
    router: Router,
    models: mpsc::Receiver<ModelRequest>,
    sends: mpsc::Receiver<Value>,
    cancel: CancellationToken,
    server: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl Rig {
    async fn new(scenario: &ScenarioConfig) -> Self {
        let (model_tx, models) = mpsc::channel(8);
        let (send_tx, sends) = mpsc::channel(8);
        let app = HttpRouter::new()
            .route(
                "/api/get_login_info",
                post(|| async { Json(json!({"status":"ok","retcode":0,"data":{"uin":BOT}})) }),
            )
            .route("/v1/chat/completions", post(complete))
            .route("/api/send_group_message", post(send))
            .route("/api/send_private_message", post(send))
            .with_state(Peer {
                models: model_tx,
                sends: send_tx,
                sequence: Arc::new(AtomicI64::new(10000)),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.app.database = directory
            .path()
            .join("eval.sqlite")
            .to_string_lossy()
            .into_owned();
        config.milky.url = url.clone();
        config.milky.bot_id = Some(BOT);
        config.llm.chat_completions_url = format!("{url}/v1/chat/completions");
        config.llm.model = "synthetic".into();
        config.llm.api_key = "synthetic".into();
        config.llm.tool_calling = false;
        config.embedding.enabled = false;
        config.plugins.ai.defaults.summary = false;
        config.plugins.ai.defaults.memory_write = false;
        config.plugins.ai.defaults.input_budget = 64_000;
        config.plugins.ai.learning.enabled = false;
        config.plugins.ai.knowledge.group_public_recall = scenario.group_public_recall;
        if let Some(policy) = scenario.cross_scene {
            config.plugins.ai.context.cross_scene = policy;
        }
        config.limits.send_cooldown_secs = 0;
        let host = Host::create(config).await.unwrap();
        host.milky.login().await.unwrap();
        let router = Router::new(host.clone());
        Self {
            host,
            router,
            models,
            sends,
            cancel,
            server,
            _directory: directory,
        }
    }

    async fn seed(&self, scenario: &Scenario) {
        let memories: Vec<(i64, Scope, String, String)> = scenario
            .memories
            .iter()
            .map(|m| {
                (
                    m.subject,
                    scope(&m.scene, m.peer),
                    m.kind.clone(),
                    m.text.clone(),
                )
            })
            .collect();
        let knowledge: Vec<(SeedKnowledgeRow, bool)> = scenario
            .knowledge
            .iter()
            .map(|k| {
                (
                    SeedKnowledgeRow {
                        kind: k.kind.clone(),
                        subject: k.subject.clone(),
                        subject_id: k.subject_id,
                        scope: scope(&k.scene, k.peer),
                        text: k.text.clone(),
                    },
                    k.revoked,
                )
            })
            .collect();
        let revoked: Vec<String> = self
            .host
            .store
            .call(move |db| {
                let mut seq = 9000;
                let mut revoked = Vec::new();
                let mut source = |db: &rusqlite::Connection, scope: &Scope, sender: i64, text: &str| -> anyhow::Result<String> {
                    seq += 1;
                    let (conversation, mode, owner) = conversation_id(scope);
                    db.execute("INSERT OR IGNORE INTO ai_conversations(id,scope,mode,owner_id) VALUES(?1,?2,?3,?4)", params![conversation, scope.key(), mode, owner])?;
                    let key = format!("{}:{seq}", scope.key());
                    db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,learnable) VALUES(?1,?2,?3,1,?4,'user',?5,?6,1)",
                        params![key, scope.key(), conversation, sender, text, now() - 3600])?;
                    Ok(key)
                };
                for (subject, scope, kind, text) in &memories {
                    let key = source(db, scope, *subject, text)?;
                    let (conversation, _, _) = conversation_id(scope);
                    let evidence = serde_json::to_string(&json!([{"source_key":key,"quote":text}]))?;
                    db.execute("INSERT INTO ai_memories(conversation_id,subject_id,kind,text,evidence,created_at,updated_at,origin) VALUES(?1,?2,?3,?4,?5,?6,?6,'auto')",
                        params![conversation, subject, kind, text, evidence, now() - 3600])?;
                    let id = db.last_insert_rowid();
                    db.execute("INSERT INTO ai_memory_provenance(memory_id,source_key,quote) VALUES(?1,?2,?3)", params![id, key, text])?;
                    db.execute("INSERT INTO ai_memory_search(memory_id,terms) VALUES(?1,?2)", params![id, qkbot::plugins::ai::memory::lexical_terms(text).join(" ")])?;
                }
                for (row, revoke) in &knowledge {
                    let sender = if row.subject == "user" { row.subject_id } else { 790 };
                    let key = source(db, &row.scope, sender, &row.text)?;
                    let character = if row.kind == "self" { "default" } else { "" };
                    db.execute("INSERT INTO ai_knowledge(bot_id,kind,subject,subject_id,scope,character_id,domain,text,origin,confidence,importance,status,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,'',?7,'auto',0.7,3,'active',?8,?8)",
                        params![BOT, row.kind, row.subject, row.subject_id, row.scope.key(), character, row.text, now() - 3600])?;
                    let id = db.last_insert_rowid();
                    db.execute("INSERT INTO ai_knowledge_sources(knowledge_id,source_key,role,quote) VALUES(?1,?2,'evidence',?3)", params![id, key, row.text])?;
                    if *revoke {
                        revoked.push(key);
                    }
                }
                Ok(revoked)
            })
            .await
            .unwrap();
        if !revoked.is_empty() {
            self.host
                .store
                .call(move |db| {
                    let tx = db.transaction()?;
                    qkbot::plugins::ai::context::continuity::invalidate_sources(&tx, &revoked)?;
                    tx.commit()?;
                    Ok(())
                })
                .await
                .unwrap();
        }
    }

    async fn request(&mut self, message: Incoming) -> ModelRequest {
        self.router.dispatch(message).await.unwrap();
        within(self.models.recv()).await.unwrap()
    }

    async fn finish(&mut self, request: ModelRequest, response: Value) {
        request.reply.send(response).unwrap();
        within(self.sends.recv()).await.unwrap();
    }

    async fn shutdown(self) {
        within(self.router.shutdown()).await.unwrap();
        drop(self.host);
        self.cancel.cancel();
        within(self.server).await.unwrap();
    }
}

struct SeedKnowledgeRow {
    kind: String,
    subject: String,
    subject_id: i64,
    scope: Scope,
    text: String,
}

fn incoming(turn: &Turn, sequence: i64) -> Incoming {
    let scope = scope(&turn.scene, turn.peer);
    let group = scope.scene == Scene::Group;
    let mut mentions = if group { vec![BOT] } else { vec![] };
    mentions.extend(&turn.mentions);
    Incoming {
        key: MessageKey {
            scope,
            message_seq: sequence,
        },
        sender_id: turn.sender,
        sender_nickname: Some(format!("用户{}", turn.sender)),
        sender_card: None,
        text: turn.text.clone(),
        mentions,
        quote_seq: None,
        received_at: now(),
        raw_message: Value::Null,
        notification: None,
    }
}

/// Every non-system message the model received, as one searchable text.
fn model_input(body: &Value) -> String {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] != "system")
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Default, serde::Serialize)]
struct Score {
    name: String,
    expected: usize,
    recalled: usize,
    forbidden: usize,
    leaked: usize,
    misses: Vec<String>,
    leaks: Vec<String>,
}

fn scenarios() -> Vec<Scenario> {
    let mut all = Vec::new();
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/eval");
    let mut files: Vec<_> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    for file in files {
        let parsed: Vec<Scenario> = serde_json::from_slice(&std::fs::read(&file).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        all.extend(parsed);
    }
    assert!(!all.is_empty());
    all
}

/// Model used to answer turns: the stub, or a real endpoint.
struct RealModel {
    client: reqwest::Client,
    url: String,
    model: String,
    key: String,
}

impl RealModel {
    fn from_env() -> Option<Self> {
        Some(Self {
            client: reqwest::Client::new(),
            url: std::env::var("QKBOT_EVAL_MODEL_URL").ok()?,
            model: std::env::var("QKBOT_EVAL_MODEL").ok()?,
            key: std::env::var("QKBOT_EVAL_API_KEY").unwrap_or_default(),
        })
    }
    async fn complete(&self, mut body: Value) -> Value {
        body["model"] = json!(self.model);
        if let Some(object) = body.as_object_mut() {
            object.remove("tools");
            object.remove("tool_choice");
        }
        let mut request = self.client.post(&self.url).json(&body);
        if !self.key.is_empty() {
            request = request.bearer_auth(&self.key);
        }
        let response: Value = request.send().await.unwrap().json().await.unwrap();
        assert!(
            response["choices"][0]["message"]["content"].is_string(),
            "unexpected model response: {response}"
        );
        response
    }
    async fn judge(&self, transcript: &Value) -> Value {
        let prompt = "你是对话质量评审。下面是QQ聊天机器人“小鱼”在一个评测场景中的完整记录：每轮包含用户发言、注入给机器人的记忆资料与机器人回复。按1到5分评价：naturalness(像真人熟人一样自然)、persona(语气与人设一致)、memory_use(恰当使用相关记忆、不生硬复述、不编造)、privacy(没有泄露不该出现的他人或私聊信息)。只输出JSON：{\"naturalness\":n,\"persona\":n,\"memory_use\":n,\"privacy\":n,\"comment\":\"一句话理由\"}";
        let body = json!({"model":self.model,"messages":[{"role":"system","content":prompt},{"role":"user","content":transcript.to_string()}]});
        let response = self.complete(body).await;
        let content = response["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("");
        let trimmed = content
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        serde_json::from_str(trimmed).unwrap_or_else(|_| json!({"raw":content}))
    }
}

async fn evaluate(real: Option<&RealModel>) -> Vec<Score> {
    let output = std::env::var_os("QKBOT_EVAL_OUT").map(std::path::PathBuf::from);
    if let Some(directory) = &output {
        std::fs::create_dir_all(directory).unwrap();
    }
    let mut scores = Vec::new();
    for scenario in scenarios() {
        let mut rig = Rig::new(&scenario.config).await;
        rig.seed(&scenario).await;
        let mut score = Score {
            name: scenario.name.clone(),
            ..Score::default()
        };
        let mut transcript = Vec::new();
        for (index, turn) in scenario.turns.iter().enumerate() {
            let request = rig.request(incoming(turn, 100 + index as i64)).await;
            let input = model_input(&request.body);
            for item in &turn.expect {
                score.expected += 1;
                if input.contains(item.as_str()) {
                    score.recalled += 1;
                } else {
                    score.misses.push(format!("turn {index}: {item}"));
                }
            }
            for item in &turn.forbid {
                score.forbidden += 1;
                if input.contains(item.as_str()) {
                    score.leaked += 1;
                    score.leaks.push(format!("turn {index}: {item}"));
                }
            }
            let response = match real {
                Some(model) => model.complete(request.body.clone()).await,
                None => answer(turn.reply.as_deref().unwrap_or("收到")),
            };
            let reply = response["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            let material: Vec<&str> = input
                .lines()
                .filter(|l| {
                    l.contains("记忆")
                        || l.contains("共同经历")
                        || l.contains("你说过的话")
                        || l.contains("本群")
                        || l.contains("人物印象")
                        || l.contains("群成员")
                })
                .collect();
            transcript.push(json!({"user":turn.text,"scene":turn.scene,"sender":turn.sender,"material":material,"reply":reply}));
            rig.finish(request, response).await;
        }
        if let Some(directory) = &output {
            let mut record = json!({"scenario":scenario.name,"turns":transcript,"score":&score});
            if let Some(model) =
                real.filter(|_| std::env::var("QKBOT_EVAL_JUDGE").is_ok_and(|v| v == "1"))
            {
                record["judge"] = model
                    .judge(&json!({"scenario":scenario.name,"turns":record["turns"]}))
                    .await;
            }
            std::fs::write(
                directory.join(format!("{}.json", scenario.name)),
                serde_json::to_vec_pretty(&record).unwrap(),
            )
            .unwrap();
        }
        rig.shutdown().await;
        scores.push(score);
    }
    let expected: usize = scores.iter().map(|s| s.expected).sum();
    let recalled: usize = scores.iter().map(|s| s.recalled).sum();
    let forbidden: usize = scores.iter().map(|s| s.forbidden).sum();
    let leaked: usize = scores.iter().map(|s| s.leaked).sum();
    let summary = json!({"scenarios":scores.len(),"recall":format!("{recalled}/{expected}"),"leaks":format!("{leaked}/{forbidden}"),"details":&scores});
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    if let Some(directory) = &output {
        std::fs::write(
            directory.join("summary.json"),
            serde_json::to_vec_pretty(&summary).unwrap(),
        )
        .unwrap();
    }
    scores
}

#[tokio::test]
async fn built_in_scenarios_recall_what_they_should_and_leak_nothing() {
    let scores = evaluate(None).await;
    for score in &scores {
        assert!(
            score.leaks.is_empty(),
            "{} leaked: {:?}",
            score.name,
            score.leaks
        );
        assert!(
            score.misses.is_empty(),
            "{} missed: {:?}",
            score.name,
            score.misses
        );
    }
}

#[tokio::test]
#[ignore = "needs QKBOT_EVAL_MODEL_URL and QKBOT_EVAL_MODEL; sends synthetic scenarios to that model"]
async fn real_model_transcripts_and_judgement() {
    let model = RealModel::from_env().expect("set QKBOT_EVAL_MODEL_URL and QKBOT_EVAL_MODEL");
    evaluate(Some(&model)).await;
}
