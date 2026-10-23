//! Real ingress, model HTTP, confirmed Milky sends and cross-scene context.
//! All identities, credentials and services are local synthetic fixtures.
use axum::{Json, Router as HttpRouter, extract::State, routing::post};
use qkbot::{
    config::Config,
    host::{Host, Router},
    types::{Incoming, MessageKey, Scene, Scope, now},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

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
    tokio::time::timeout(Duration::from_secs(8), future)
        .await
        .expect("continuity integration timeout")
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

fn incoming(
    scene: Scene,
    peer: i64,
    sender: i64,
    sequence: i64,
    text: &str,
    direct: bool,
) -> Incoming {
    Incoming {
        key: MessageKey {
            scope: Scope {
                bot_id: 123,
                scene,
                peer_id: peer,
            },
            message_seq: sequence,
        },
        sender_id: sender,
        sender_nickname: Some(format!("用户{sender}")),
        sender_card: None,
        text: text.into(),
        mentions: if direct && scene == Scene::Group {
            vec![123]
        } else {
            Vec::new()
        },
        quote_seq: None,
        received_at: now(),
        raw_message: Value::Null,
        notification: None,
    }
}

async fn wait_visible(host: &Host, key: &str) {
    let key = key.to_owned();
    within(async {
        loop {
            let source=key.clone();
            let visible:bool=host.store.call(move |db| Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM ai_context_sources WHERE source_key=?1 AND context_visible=1)",
                [source], |row| row.get(0))?)).await.unwrap();
            if visible { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await;
}

async fn wait_completed(host: &Host, expected: i64) {
    within(async {
        loop {
            let count: i64 = host
                .store
                .call(|db| {
                    Ok(db.query_row("SELECT count(*) FROM ai_recent_turns", [], |row| row.get(0))?)
                })
                .await
                .unwrap();
            if count >= expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

fn continuity_boxes(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_str())
        .filter(|content| content.contains("anchor_question") && content.contains("anchor_reply"))
        .map(str::to_owned)
        .collect()
}

fn parsed_boxes(body: &Value) -> Vec<Value> {
    continuity_boxes(body)
        .iter()
        .map(|content| serde_json::from_str(content.rsplit_once('\n').unwrap().1).unwrap())
        .collect()
}

fn neighbor_counts(source: &Value) -> (usize, usize) {
    let neighbors = source["neighbors"].as_array().unwrap();
    (
        neighbors
            .iter()
            .filter(|row| row["relation"] == "neighbor_before")
            .count(),
        neighbors
            .iter()
            .filter(|row| row["relation"] == "neighbor_after")
            .count(),
    )
}

fn export_audit_request(stem: &str, request: &Value) {
    let Some(directory) = std::env::var_os("QKBOT_CONTEXT_AUDIT_DIR") else {
        return;
    };
    // Callers use fixed filenames. Only synthetic HTTP request bodies are
    // exported: no credentials, configuration, or production records.
    assert!(matches!(
        stem,
        "full-window-group-b"
            | "full-window-group-c"
            | "other-user-group"
            | "default-budget-group-b"
    ));
    let directory = std::path::PathBuf::from(directory);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join(format!("{stem}.request.json")),
        serde_json::to_vec_pretty(request).unwrap(),
    )
    .unwrap();
    let sources: Vec<_> = parsed_boxes(request)
        .iter()
        .map(|source| {
            let (before, after) = neighbor_counts(source);
            let authors: BTreeSet<_> = source["neighbors"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["sender_id"].as_i64().unwrap())
                .collect();
            json!({"source_scope":source["source_scope"],"actor_id":source["actor_id"],
            "anchor_question_sender":source["anchor_question"]["sender_id"],
            "anchor_reply_sender":source["anchor_reply"]["sender_id"],
            "neighbors_before":before,"neighbors_after":after,"neighbors_total":before+after,
            "neighbor_authors":authors})
        })
        .collect();
    std::fs::write(
        directory.join(format!("{stem}.stats.json")),
        serde_json::to_vec_pretty(&json!({"sources":sources})).unwrap(),
    )
    .unwrap();
}

struct AuditRig {
    host: Arc<Host>,
    router: Router,
    models: mpsc::Receiver<ModelRequest>,
    sends: mpsc::Receiver<Value>,
    cancel: CancellationToken,
    server: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl AuditRig {
    async fn new(configure: impl FnOnce(&mut Config)) -> Self {
        let (model_tx, models) = mpsc::channel(8);
        let (send_tx, sends) = mpsc::channel(8);
        let app = HttpRouter::new()
            .route(
                "/api/get_login_info",
                post(|| async { Json(json!({"status":"ok","retcode":0,"data":{"uin":123}})) }),
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
            .join("audit.sqlite")
            .to_string_lossy()
            .into_owned();
        config.milky.url = url.clone();
        config.milky.bot_id = Some(123);
        config.llm.chat_completions_url = format!("{url}/v1/chat/completions");
        config.llm.model = "synthetic".into();
        config.llm.api_key = "synthetic".into();
        // Local reply capabilities are enabled so their absence from cross-chat
        // documents is meaningful rather than an effect of disabling tools.
        config.llm.tool_calling = true;
        config.plugins.ai.defaults.allowed_tools = vec!["reply_message".into()];
        config.embedding.enabled = false;
        config.plugins.ai.defaults.summary = false;
        config.plugins.ai.defaults.memory_write = false;
        config.plugins.ai.defaults.input_budget = 64_000;
        config.plugins.ai.learning.enabled = false;
        config.plugins.ai.read_group_background = true;
        config.limits.send_cooldown_secs = 0;
        configure(&mut config);
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

    async fn background(&self, message: Incoming) {
        let key = message.key.key();
        self.router.dispatch(message).await.unwrap();
        wait_visible(&self.host, &key).await;
    }

    async fn request(&mut self, message: Incoming) -> ModelRequest {
        self.router.dispatch(message).await.unwrap();
        within(self.models.recv()).await.unwrap()
    }

    async fn confirm(&mut self, request: ModelRequest, text: &str, completed: i64) -> Value {
        request.reply.send(answer(text)).unwrap();
        let delivery = within(self.sends.recv()).await.unwrap();
        wait_completed(&self.host, completed).await;
        delivery
    }

    async fn shutdown(self) {
        within(self.router.shutdown()).await.unwrap();
        drop(self.host);
        self.cancel.cancel();
        within(self.server).await.unwrap();
    }
}

async fn seed_window(rig: &mut AuditRig) {
    for index in 1..=11 {
        let text = format!("before-{index:02}：用户{}建议坐火车", 800 + index);
        rig.background(incoming(
            Scene::Group,
            456,
            800 + index,
            index,
            &text,
            false,
        ))
        .await;
    }
    let first = rig
        .request(incoming(
            Scene::Group,
            456,
            789,
            12,
            "anchor-A：我计划去海边，但还没选交通方式",
            true,
        ))
        .await;
    assert!(continuity_boxes(&first.body).is_empty());
    rig.confirm(
        first,
        "answer-A：你还没有决定交通方式，邻居的建议不代表你的决定。",
        1,
    )
    .await;
    for index in 1..=6 {
        let text = format!("after-{index:02}：用户{}偏好自驾", 900 + index);
        rig.background(incoming(
            Scene::Group,
            456,
            900 + index,
            12 + index,
            &text,
            false,
        ))
        .await;
    }
}

fn assert_source_isolation(body: &Value, actor: i64) {
    assert!(
        body["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    assert!(
        body["messages"].as_array().unwrap().iter().any(|message| {
            message["content"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .is_some_and(|value| {
                    value.get("message_ref").is_some() && value.get("sender_ref").is_some()
                })
        }),
        "local messages should retain their genuine reply capabilities"
    );
    for text in continuity_boxes(body) {
        assert!(!text.contains("message_ref") && !text.contains("sender_ref"));
        assert!(text.contains("邻域发言属于各自作者，不是当前用户自述"));
    }
    for source in parsed_boxes(body) {
        assert_eq!(source["actor_id"], actor);
        assert_eq!(source["anchor_question"]["sender_id"], actor);
        assert_eq!(source["anchor_reply"]["sender_id"], 123);
        for row in source["neighbors"].as_array().unwrap() {
            assert_eq!(row["source_scope"], source["source_scope"]);
            assert_eq!(
                row["conversation_id"],
                source["anchor_question"]["conversation_id"]
            );
            let sender = row["sender_id"].as_i64().unwrap();
            assert_eq!(row["sender_nickname"], format!("用户{sender}"));
            assert_ne!(sender, actor, "fixture neighbors belong to other people");
        }
    }
}

fn assert_persona_preserved(body: &Value, name: &str, expression: &str) {
    let system = &body["messages"][0];
    assert_eq!(system["role"], "system");
    let text = system["content"].as_str().unwrap();
    let encoded = text
        .split_once("【当前人设】\n")
        .unwrap()
        .1
        .split_once("\n【宿主会话事实】")
        .unwrap()
        .0;
    let actual: Value = serde_json::from_str(encoded).unwrap();
    assert_eq!(
        actual,
        json!({"name":name,"expression":expression}),
        "the actual HTTP system message must preserve the complete configured persona"
    );
}

#[tokio::test]
async fn exact_ten_before_five_after_survive_group_private_group_with_actor_and_capability_isolation()
 {
    const PERSONA_NAME: &str = "跨会话完整人设测试";
    let mut persona = "【接续人设开头】你是一位耐心、清晰的行程讨论助手。\n".to_owned();
    for index in 1..=42 {
        persona.push_str(&format!("第{index:02}条表达偏好：保持清晰、温和、自然的中文表达；解释先说结论再给理由，不把邻居的偏好说成提问者的决定。\n"));
    }
    persona.push_str("【接续人设结尾】保持这套表达风格，同时准确保留每句话的原作者。");
    assert!((6000..=8192).contains(&persona.len()));
    let mut rig = AuditRig::new(|config| {
        // Match the proposed production allocation while keeping the default
        // 10/5 neighbor limits and three-turn recall cap unchanged.
        config.plugins.ai.defaults.input_budget = 131_072;
        config.plugins.ai.context.recall_budget = 24_000;
        config.plugins.ai.context.neighbor_budget = 18_000;
        config.plugins.ai.defaults.persona = PERSONA_NAME.into();
        config
            .plugins
            .ai
            .personas
            .insert(PERSONA_NAME.into(), persona.clone());
    })
    .await;
    seed_window(&mut rig).await;
    let other = rig
        .request(incoming(
            Scene::Group,
            999,
            888,
            30,
            "other-actor-question：我选飞机",
            true,
        ))
        .await;
    assert!(continuity_boxes(&other.body).is_empty());
    rig.confirm(other, "other-actor-answer：用户888选择了飞机。", 2)
        .await;

    let group_b = rig
        .request(incoming(
            Scene::Group,
            654,
            789,
            31,
            "换到群B：我选了什么交通方式？请区分我的决定和别人建议。",
            true,
        ))
        .await;
    assert_source_isolation(&group_b.body, 789);
    assert_persona_preserved(&group_b.body, PERSONA_NAME, &persona);
    let sources = parsed_boxes(&group_b.body);
    assert_eq!(sources.len(), 1);
    let source = &sources[0];
    assert_eq!(source["source_scope"], "123:group:456");
    assert_eq!(neighbor_counts(source), (10, 5));
    let neighbors = source["neighbors"].as_array().unwrap();
    let expected: Vec<_> = (2..=11)
        .map(|index| format!("before-{index:02}：用户{}建议坐火车", 800 + index))
        .chain((1..=5).map(|index| format!("after-{index:02}：用户{}偏好自驾", 900 + index)))
        .collect();
    let actual: Vec<_> = neighbors
        .iter()
        .map(|row| row["text"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        actual, expected,
        "retain the nearest ten before and five after in source order"
    );
    let serialized = serde_json::to_string(source).unwrap();
    for excluded in [
        "before-01",
        "after-06",
        "other-actor-question",
        "other-actor-answer",
    ] {
        assert!(
            !serialized.contains(excluded),
            "excluded source appeared: {excluded}"
        );
    }
    export_audit_request("full-window-group-b", &group_b.body);
    let delivered = rig
        .confirm(
            group_b,
            "answer-B：你还没有选择，火车和自驾都是其他成员的意见。",
            3,
        )
        .await;
    assert_eq!(delivered["group_id"], 654);

    let private = rig
        .request(incoming(
            Scene::Friend,
            789,
            789,
            32,
            "私聊继续：这些建议是谁说的？",
            true,
        ))
        .await;
    assert_source_isolation(&private.body, 789);
    assert_persona_preserved(&private.body, PERSONA_NAME, &persona);
    let scopes: BTreeSet<_> = parsed_boxes(&private.body)
        .iter()
        .map(|source| source["source_scope"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        scopes,
        BTreeSet::from(["123:group:456".to_owned(), "123:group:654".to_owned()])
    );
    let delivered = rig
        .confirm(private, "answer-private：邻居各自提出建议，你仍未决定。", 4)
        .await;
    assert_eq!(delivered["user_id"], 789);

    let group_c = rig
        .request(incoming(
            Scene::Group,
            765,
            789,
            33,
            "换到群C：接着私聊，我决定坐火车了吗？不要把邻居偏好当成我的。",
            true,
        ))
        .await;
    assert_source_isolation(&group_c.body, 789);
    assert_persona_preserved(&group_c.body, PERSONA_NAME, &persona);
    let sources = parsed_boxes(&group_c.body);
    let scopes: BTreeSet<_> = sources
        .iter()
        .map(|source| source["source_scope"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        scopes,
        BTreeSet::from([
            "123:group:456".to_owned(),
            "123:group:654".to_owned(),
            "123:friend:789".to_owned()
        ])
    );
    assert_eq!(
        neighbor_counts(
            sources
                .iter()
                .find(|source| source["source_scope"] == "123:group:456")
                .unwrap()
        ),
        (10, 5)
    );
    assert!(
        sources.iter().any(|source| source["anchor_reply"]["text"]
            == "answer-private：邻居各自提出建议，你仍未决定。")
    );
    export_audit_request("full-window-group-c", &group_c.body);
    let delivered = rig.confirm(group_c, "answer-C：还没有。", 5).await;
    assert_eq!(delivered["group_id"], 765);

    let other = rig
        .request(incoming(
            Scene::Group,
            777,
            888,
            34,
            "换群继续：我之前选了哪种交通方式？",
            true,
        ))
        .await;
    assert_source_isolation(&other.body, 888);
    let sources = parsed_boxes(&other.body);
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["source_scope"], "123:group:999");
    assert_eq!(sources[0]["anchor_question"]["sender_id"], 888);
    let text = continuity_boxes(&other.body).join("\n");
    for excluded in [
        "anchor-A",
        "answer-A",
        "answer-B",
        "answer-private",
        "answer-C",
        "before-",
        "after-",
    ] {
        assert!(
            !text.contains(excluded),
            "another actor's context appeared: {excluded}"
        );
    }
    export_audit_request("other-user-group", &other.body);
    rig.confirm(other, "其他用户独立接续完成", 6).await;

    let other_private = rig
        .request(incoming(
            Scene::Friend,
            888,
            888,
            35,
            "现在我私聊继续：之前在两个群里聊到什么？",
            true,
        ))
        .await;
    assert_source_isolation(&other_private.body, 888);
    assert_persona_preserved(&other_private.body, PERSONA_NAME, &persona);
    let sources = parsed_boxes(&other_private.body);
    let scopes: BTreeSet<_> = sources
        .iter()
        .map(|source| source["source_scope"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(sources.len(), 2);
    assert_eq!(
        scopes,
        BTreeSet::from(["123:group:999".to_owned(), "123:group:777".to_owned()])
    );
    let request_text = serde_json::to_string(&other_private.body["messages"]).unwrap();
    for excluded in [
        "123:friend:789",
        "123:group:456",
        "anchor-A",
        "answer-A",
        "answer-B",
        "answer-private",
        "answer-C",
        "before-",
        "after-",
    ] {
        assert!(
            !request_text.contains(excluded),
            "another person's private or group context appeared: {excluded}"
        );
    }
    let delivered = rig
        .confirm(other_private, "用户888私聊独立接续完成", 7)
        .await;
    assert_eq!(delivered["user_id"], 888);
    rig.shutdown().await;
}

#[tokio::test]
async fn default_neighbor_budget_limits_material_instead_of_guaranteeing_fifteen_rows() {
    let mut rig = AuditRig::new(|config| {
        assert_eq!(config.plugins.ai.context.neighbors_before, 10);
        assert_eq!(config.plugins.ai.context.neighbors_after, 5);
        assert_eq!(config.plugins.ai.context.neighbor_budget, 1500);
        assert_eq!(config.plugins.ai.context.recall_budget, 3000);
    })
    .await;
    seed_window(&mut rig).await;
    let next = rig
        .request(incoming(
            Scene::Group,
            654,
            789,
            31,
            "默认预算接着聊：我选择了什么？看到的邻居发言各是谁的？",
            true,
        ))
        .await;
    assert_source_isolation(&next.body, 789);
    let sources = parsed_boxes(&next.body);
    assert_eq!(
        sources.len(),
        1,
        "the complete question/answer pair should still fit"
    );
    let source = &sources[0];
    assert!(
        source["anchor_question"]["text"]
            .as_str()
            .unwrap()
            .contains("anchor-A")
    );
    assert!(
        source["anchor_reply"]["text"]
            .as_str()
            .unwrap()
            .contains("answer-A")
    );
    let (before, after) = neighbor_counts(source);
    assert!(before <= 10 && after <= 5);
    assert!(
        before + after > 0,
        "some short neighboring material should fit"
    );
    assert!(
        before + after < 15,
        "the 10/5 limits are maxima, not a guaranteed 15-row allocation"
    );
    eprintln!(
        "default continuity budget: before={before}, after={after}, total={}",
        before + after
    );
    export_audit_request("default-budget-group-b", &next.body);
    rig.confirm(next, "默认预算接续完成", 2).await;
    rig.shutdown().await;
}

#[tokio::test]
async fn direct_confirmed_turn_carries_bounded_attributed_context_across_group_and_friend() {
    let (model_tx, mut models) = mpsc::channel(8);
    let (send_tx, mut sends) = mpsc::channel(8);
    let app = HttpRouter::new()
        .route(
            "/api/get_login_info",
            post(|| async { Json(json!({"status":"ok","retcode":0,"data":{"uin":123}})) }),
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
    let temp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.app.database = temp
        .path()
        .join("continuity.sqlite")
        .to_string_lossy()
        .into_owned();
    config.milky.url = url.clone();
    config.milky.bot_id = Some(123);
    config.llm.chat_completions_url = format!("{url}/v1/chat/completions");
    config.llm.model = "synthetic".into();
    config.llm.api_key = "synthetic".into();
    config.llm.tool_calling = false;
    config.embedding.enabled = false;
    config.plugins.ai.defaults.allowed_tools.clear();
    config.plugins.ai.defaults.summary = false;
    config.plugins.ai.defaults.memory_write = false;
    config.plugins.ai.defaults.input_budget = 24_000;
    config.plugins.ai.context.recall_budget = 10_000;
    config.plugins.ai.context.neighbor_budget = 4_000;
    config.plugins.ai.learning.enabled = false;
    config.plugins.ai.read_group_background = true;
    config.limits.send_cooldown_secs = 0;
    let host = Host::create(config).await.unwrap();
    host.milky.login().await.unwrap();
    let router = Router::new(host.clone());

    let before = incoming(
        Scene::Group,
        456,
        800,
        1,
        "neighbor-before-unique：讨论海边行程",
        false,
    );
    router.dispatch(before.clone()).await.unwrap();
    wait_visible(&host, &before.key.key()).await;
    router
        .dispatch(incoming(
            Scene::Group,
            456,
            789,
            2,
            "anchor-question-unique：下周去哪",
            true,
        ))
        .await
        .unwrap();
    let first = within(models.recv()).await.unwrap();
    assert!(continuity_boxes(&first.body).is_empty());
    first
        .reply
        .send(answer("anchor-answer-unique：可以去海边"))
        .unwrap();
    within(sends.recv()).await.unwrap();
    wait_completed(&host, 1).await;
    let after = incoming(
        Scene::Group,
        456,
        801,
        3,
        "neighbor-after-unique：记得查看天气",
        false,
    );
    router.dispatch(after.clone()).await.unwrap();
    wait_visible(&host, &after.key.key()).await;

    router
        .dispatch(incoming(Scene::Group, 654, 789, 4, "换个群接着聊", true))
        .await
        .unwrap();
    let second = within(models.recv()).await.unwrap();
    let boxes = continuity_boxes(&second.body);
    assert_eq!(
        boxes.len(),
        1,
        "cross-group source box must reach the actual model request"
    );
    let source = &boxes[0];
    for marker in [
        "anchor-question-unique",
        "anchor-answer-unique",
        "neighbor-before-unique",
        "neighbor-after-unique",
        "123:group:456",
    ] {
        assert!(
            source.contains(marker),
            "missing attributed source: {marker}"
        );
    }
    assert!(source.contains("800") && source.contains("801"));
    assert!(!source.contains("message_ref") && !source.contains("sender_ref"));
    second
        .reply
        .send(answer("second-group-answer-unique：继续规划海边行程"))
        .unwrap();
    within(sends.recv()).await.unwrap();
    wait_completed(&host, 2).await;

    router
        .dispatch(incoming(Scene::Friend, 789, 789, 5, "现在私聊继续", true))
        .await
        .unwrap();
    let third = within(models.recv()).await.unwrap();
    let source = continuity_boxes(&third.body).join("\n");
    assert!(source.contains("anchor-question-unique") && source.contains("anchor-answer-unique"));
    assert!(source.contains("second-group-answer-unique"));
    assert!(source.contains("123:group:456") && source.contains("123:group:654"));
    third.reply.send(answer("私聊接续完成")).unwrap();
    let delivered = within(sends.recv()).await.unwrap();
    assert_eq!(delivered["user_id"], 789);
    wait_completed(&host, 3).await;

    // The network delivery is real even when optional context metadata fails.
    // This aborts registration after continuity inserted its recent-turn row,
    // exercising the host's savepoint rollback without losing the send ledger.
    host.store.call(|db| {
        db.execute_batch("CREATE TRIGGER reject_context_dependencies BEFORE INSERT ON ai_context_reply_dependencies
            BEGIN SELECT RAISE(ABORT, 'synthetic metadata failure'); END;")?;
        Ok(())
    }).await.unwrap();
    router
        .dispatch(incoming(
            Scene::Friend,
            789,
            789,
            6,
            "确认发送但登记失败",
            true,
        ))
        .await
        .unwrap();
    let fourth = within(models.recv()).await.unwrap();
    fourth
        .reply
        .send(answer("metadata-failure-confirmed-answer"))
        .unwrap();
    let delivered = within(sends.recv()).await.unwrap();
    assert_eq!(delivered["user_id"], 789);
    within(async {
        loop {
            let persisted: bool = host.store.call(|db| Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM outbox o JOIN messages m ON m.scope=o.scope AND m.key=o.scope || ':' || o.message_seq
                 WHERE o.scope='123:friend:789' AND o.message_seq=10003 AND o.state='confirmed')",
                [], |row| row.get(0),
            )?)).await.unwrap();
            if persisted { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await;
    let (state, role, conversation, epoch, text, recent): (String, String, String, i64, String, i64) = host.store.call(|db| {
        Ok(db.query_row(
            "SELECT o.state,m.role,m.conversation_id,m.epoch,m.content,(SELECT count(*) FROM ai_recent_turns)
             FROM outbox o JOIN messages m ON m.scope=o.scope AND m.key=o.scope || ':' || o.message_seq
             WHERE o.scope='123:friend:789' AND o.message_seq=10003",
            [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)),
        )?)
    }).await.unwrap();
    assert_eq!(state, "confirmed");
    assert_eq!(role, "system");
    assert!(conversation.is_empty());
    assert_eq!(epoch, 0);
    assert_eq!(text, "metadata-failure-confirmed-answer");
    assert_eq!(
        recent, 3,
        "the failed metadata insert must roll back its recent turn"
    );

    within(router.shutdown()).await.unwrap();
    drop(host);
    cancel.cancel();
    within(server).await.unwrap();
}
