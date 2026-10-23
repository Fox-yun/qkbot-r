//! Real learning-note storage and authorization with a controlled local embedding service.
use std::{sync::Arc, time::Duration};

use axum::{Json, Router as HttpRouter, extract::State, http::StatusCode, routing::post};
use qkbot::{
    config::Config,
    host::{Host, Router},
    plugins::ai::{self, learning},
    types::{ContextGuard, Incoming, MessageKey, NudgeEvent, Scene, Scope, now},
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

struct Request {
    body: Value,
    reply: oneshot::Sender<(StatusCode, Value)>,
}

#[derive(Clone)]
struct MockState {
    models: mpsc::Sender<Request>,
    embeddings: mpsc::Sender<Request>,
    sends: mpsc::Sender<Value>,
}

struct MockServer {
    url: String,
    models: mpsc::Receiver<Request>,
    embeddings: mpsc::Receiver<Request>,
    sends: mpsc::Receiver<Value>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("learning semantic integration timeout")
}

async fn controlled(tx: mpsc::Sender<Request>, body: Value) -> (StatusCode, Json<Value>) {
    let (reply, receive) = oneshot::channel();
    tx.send(Request { body, reply }).await.unwrap();
    let (status, value) = receive.await.unwrap();
    (status, Json(value))
}

impl MockServer {
    async fn start() -> Self {
        let (model_tx, models) = mpsc::channel(16);
        let (embedding_tx, embeddings) = mpsc::channel(16);
        let (send_tx, sends) = mpsc::channel(16);
        let app = HttpRouter::new()
            .route("/api/get_login_info", post(|| async {
                Json(json!({"status":"ok","retcode":0,"data":{"uin":123}}))
            }))
            .route("/v1/chat/completions", post(|State(state): State<MockState>, Json(body): Json<Value>| async move {
                controlled(state.models, body).await
            }))
            .route("/v1/embeddings", post(|State(state): State<MockState>, Json(body): Json<Value>| async move {
                controlled(state.embeddings, body).await
            }))
            .route("/api/send_group_message", post(|State(state): State<MockState>, Json(body): Json<Value>| async move {
                state.sends.send(body).await.unwrap();
                Json(json!({"status":"ok","retcode":0,"data":{"message_seq":10000,"time":now()}}))
            }))
            .with_state(MockState { models: model_tx, embeddings: embedding_tx, sends: send_tx });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        });
        Self {
            url,
            models,
            embeddings,
            sends,
            cancel,
            task,
        }
    }

    async fn embedding(&mut self) -> Request {
        within(self.embeddings.recv())
            .await
            .expect("embedding mock closed")
    }

    async fn model(&mut self) -> Request {
        within(self.models.recv()).await.expect("model mock closed")
    }

    fn no_embedding(&mut self) {
        assert!(
            self.embeddings.try_recv().is_err(),
            "unexpected embedding HTTP request"
        );
    }

    async fn stop(self) {
        self.cancel.cancel();
        within(self.task).await.unwrap().unwrap();
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    host: Arc<Host>,
    router: Router,
    mock: MockServer,
}

impl Fixture {
    async fn new(configure: impl FnOnce(&mut Config)) -> Self {
        let mock = MockServer::start().await;
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.app.database = temp
            .path()
            .join("learning.sqlite3")
            .to_string_lossy()
            .into_owned();
        config.milky.url = mock.url.clone();
        config.milky.bot_id = Some(123);
        config.llm.chat_completions_url = format!("{}/v1/chat/completions", mock.url);
        config.llm.api_key = "synthetic".into();
        config.llm.model = "mock".into();
        config.embedding.enabled = true;
        config.embedding.embeddings_url = format!("{}/v1/embeddings", mock.url);
        config.embedding.api_key = "synthetic".into();
        config.embedding.dimensions = 64;
        config.plugins.ai.enabled = true;
        config.plugins.ai.learning.enabled = true;
        config.plugins.ai.learn_all_group_messages = true;
        config.plugins.ai.defaults.summary = false;
        config.plugins.ai.defaults.allowed_tools.clear();
        config.limits.send_cooldown_secs = 0;
        configure(&mut config);
        let host = Host::create(config).await.unwrap();
        host.milky.login().await.unwrap();
        learning::configure(&host.store, 123, host.config.plugins.ai.learning.enabled)
            .await
            .unwrap();
        let router = Router::new(host.clone());
        Self {
            _temp: temp,
            host,
            router,
            mock,
        }
    }

    async fn query(&self, seq: i64, peer: i64, text: &str) -> Query {
        let incoming = message(seq, peer, text);
        self.router.dispatch(incoming.clone()).await.unwrap();
        let guard = ai::conversation(&self.host, &incoming).await.unwrap().guard;
        Query { incoming, guard }
    }

    async fn index(&mut self, axis: impl FnOnce(&str) -> usize) {
        let host = self.host.clone();
        let worker = tokio::spawn(async move { learning::index_next(&host, &host.cancel).await });
        let request = self.mock.embedding().await;
        let text = request.body["input"]["text"]
            .as_str()
            .expect("text embedding input");
        let vector_axis = axis(text);
        request
            .reply
            .send((StatusCode::OK, embedding(vector_axis)))
            .unwrap();
        assert!(within(worker).await.unwrap().unwrap());
    }

    async fn vector_count(&self) -> i64 {
        self.host
            .store
            .call(|db| {
                Ok(db.query_row("SELECT count(*) FROM ai_learning_vectors", [], |r| r.get(0))?)
            })
            .await
            .unwrap()
    }

    async fn stop(self) {
        within(self.router.shutdown()).await.unwrap();
        drop(self.host);
        self.mock.stop().await;
    }
}

#[derive(Clone)]
struct Query {
    incoming: Incoming,
    guard: ContextGuard,
}

impl Query {
    async fn recall(
        &self,
        host: &Host,
        budget: usize,
    ) -> anyhow::Result<learning::LearningContext> {
        learning::recall(
            host,
            &self.guard,
            &self.incoming.key.scope,
            &self.incoming.key.key(),
            self.incoming.sender_id,
            &self.incoming.text,
            budget,
            &host.cancel,
        )
        .await
    }
}

fn message(seq: i64, peer: i64, text: &str) -> Incoming {
    Incoming {
        key: MessageKey {
            scope: Scope {
                bot_id: 123,
                scene: Scene::Group,
                peer_id: peer,
            },
            message_seq: seq,
        },
        sender_id: 789,
        sender_nickname: None,
        sender_card: None,
        text: text.into(),
        mentions: Vec::new(),
        quote_seq: None,
        received_at: now(),
        raw_message: Value::Null,
        notification: None,
    }
}

fn embedding(axis: usize) -> Value {
    let mut vector = vec![0.0; 64];
    vector[axis] = 1.0;
    json!({"object":"list","model":"Qwen/Qwen3-VL-Embedding-8B","data":[{"object":"embedding","index":0,"embedding":vector}],"usage":{"prompt_tokens":3,"total_tokens":3}})
}

fn answer(text: &str) -> Value {
    json!({"model":"mock","choices":[{"finish_reason":"stop","message":{"role":"assistant","content":text}}],"usage":{"prompt_tokens":20,"completion_tokens":5}})
}

async fn add_note(host: &Host, bot: i64, text: &str, scope: &str) -> i64 {
    learning::configure(&host.store, bot, true).await.unwrap();
    let snapshot = learning::snapshot(&host.store, bot).await.unwrap();
    learning::mutate(&host.store, bot, json!({
        "revision":snapshot["revision"],"op":"add","request_id":uuid::Uuid::new_v4().to_string(),
        "admin_id":"test-admin","session_id":"test-session","kind":"experience","text":text,"scope":scope,
        "_max_note_chars":10000,"_defaults":host.config.plugins.ai.defaults,
    })).await.unwrap()["id"].as_i64().unwrap()
}

async fn change_note(host: &Host, id: i64, op: &str, text: &str) {
    let snapshot = learning::snapshot(&host.store, 123).await.unwrap();
    let note = snapshot["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|note| note["id"] == id)
        .unwrap();
    let mut request = json!({
        "revision":snapshot["revision"],"op":op,"id":id,"version":note["version"],
        "request_id":uuid::Uuid::new_v4().to_string(),"admin_id":"test-admin","session_id":"test-session",
        "_max_note_chars":10000,"_defaults":host.config.plugins.ai.defaults,
    });
    if op == "update" {
        request["text"] = json!(text);
        request["kind"] = json!("experience");
        request["scope"] = note["scope"].clone();
    }
    learning::mutate(&host.store, 123, request).await.unwrap();
}

async fn semantic_recall(
    fixture: &mut Fixture,
    query: &Query,
    budget: usize,
    axis: usize,
) -> learning::LearningContext {
    let host = fixture.host.clone();
    let query = query.clone();
    let worker = tokio::spawn(async move { query.recall(&host, budget).await });
    let request = fixture.mock.embedding().await;
    request
        .reply
        .send((StatusCode::OK, embedding(axis)))
        .unwrap();
    within(worker).await.unwrap().unwrap()
}

fn note_ids(context: &learning::LearningContext) -> Vec<i64> {
    context
        .texts
        .iter()
        .map(|text| {
            let (_, body) = text.split_once('\n').unwrap();
            serde_json::from_str::<Value>(body).unwrap()["learning_note"]
                .as_i64()
                .unwrap()
        })
        .collect()
}

fn context_cost(context: &learning::LearningContext) -> usize {
    context
        .texts
        .iter()
        .map(|text| {
            serde_json::to_vec(&json!({"role":"user","content":text}))
                .unwrap()
                .len()
                + 1
        })
        .sum()
}

#[tokio::test]
async fn semantic_relevance_recalls_an_older_note_without_literal_query_overlap() {
    let mut fixture = Fixture::new(|config| config.plugins.ai.learning.recall_limit = 1).await;
    let relevant = add_note(
        &fixture.host,
        123,
        "项目部署异常先检查监听端口是否被占用。",
        "shared",
    )
    .await;
    let unrelated = add_note(
        &fixture.host,
        123,
        "海边日落适合使用暖色摄影滤镜。",
        "shared",
    )
    .await;
    fixture
        .host
        .store
        .call(move |db| {
            db.execute(
                "UPDATE ai_learning_notes SET updated_at=?2 WHERE id=?1",
                rusqlite::params![relevant, now() - 3600],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let query = fixture
        .query(
            1,
            456,
            "Web service fails to boot because its address is unavailable",
        )
        .await;
    assert_eq!(
        note_ids(&query.recall(&fixture.host, 12000).await.unwrap()),
        vec![unrelated]
    );
    fixture.mock.no_embedding();
    for _ in 0..2 {
        fixture
            .index(|text| if text.contains("监听端口") { 0 } else { 1 })
            .await;
    }
    let result = semantic_recall(&mut fixture, &query, 12000, 0).await;
    assert_eq!(note_ids(&result), vec![relevant]);
    assert!(context_cost(&result) <= 12000);
    fixture.stop().await;
}

#[tokio::test]
async fn semantic_candidates_share_only_with_the_same_bot_and_respect_group_scope() {
    let mut fixture = Fixture::new(|_| {}).await;
    let shared = add_note(&fixture.host, 123, "shared diagnostic reference", "shared").await;
    let local = add_note(
        &fixture.host,
        123,
        "local terminology reference",
        "123:group:456",
    )
    .await;
    add_note(&fixture.host, 999, "other-account-secret", "shared").await;
    for _ in 0..2 {
        fixture
            .index(|text| {
                assert!(!text.contains("other-account-secret"));
                0
            })
            .await;
    }
    assert_eq!(fixture.vector_count().await, 2);
    assert!(
        !learning::index_next(&fixture.host, &fixture.host.cancel)
            .await
            .unwrap()
    );
    fixture.mock.no_embedding();
    let own = fixture.query(1, 456, "how should we investigate").await;
    let own_result = semantic_recall(&mut fixture, &own, 12000, 0).await;
    assert_eq!(note_ids(&own_result).len(), 2);
    assert!(note_ids(&own_result).contains(&shared));
    assert!(note_ids(&own_result).contains(&local));
    let other_group = fixture
        .query(1, 654, "where should investigation begin")
        .await;
    let other_result = semantic_recall(&mut fixture, &other_group, 12000, 0).await;
    assert_eq!(note_ids(&other_result), vec![shared]);
    assert!(
        !other_result
            .texts
            .join("\n")
            .contains("other-account-secret")
    );
    fixture.stop().await;
}

#[tokio::test]
async fn private_disabled_zero_budget_and_disabled_semantics_do_not_call_embeddings() {
    for variant in [
        "private",
        "disabled",
        "zero",
        "semantic-disabled",
        "embedding-disabled",
    ] {
        let mut fixture = Fixture::new(|config| match variant {
            "disabled" => config.plugins.ai.learning.enabled = false,
            "semantic-disabled" => config.plugins.ai.learning.semantic_recall = false,
            "embedding-disabled" => config.embedding.enabled = false,
            _ => {}
        })
        .await;
        add_note(&fixture.host, 123, "available note", "shared").await;
        let mut query = fixture.query(1, 456, "ordinary group question").await;
        if variant == "private" {
            query.incoming.key.scope.scene = Scene::Friend;
            query.incoming.key.scope.peer_id = 789;
        }
        let budget = if variant == "zero" { 0 } else { 12000 };
        let result = query.recall(&fixture.host, budget).await.unwrap();
        assert_eq!(
            result.texts.is_empty(),
            matches!(variant, "private" | "disabled" | "zero")
        );
        if matches!(
            variant,
            "disabled" | "semantic-disabled" | "embedding-disabled"
        ) {
            assert!(
                !learning::index_next(&fixture.host, &fixture.host.cancel)
                    .await
                    .unwrap()
            );
        }
        fixture.mock.no_embedding();
        assert!(fixture.mock.models.try_recv().is_err());
        fixture.stop().await;
    }
}

#[tokio::test]
async fn edited_and_deleted_notes_invalidate_cached_and_inflight_vectors() {
    for final_op in ["update", "delete"] {
        let mut fixture = Fixture::new(|_| {}).await;
        let id = add_note(&fixture.host, 123, "original-note-marker", "shared").await;
        fixture.index(|_| 0).await;
        let query = fixture.query(1, 456, "which prior guidance applies").await;
        let prior = semantic_recall(&mut fixture, &query, 12000, 0).await;
        assert_eq!(note_ids(&prior), vec![id]);
        change_note(&fixture.host, id, "update", "intermediate-note-marker").await;
        assert_eq!(fixture.vector_count().await, 0);
        assert!(
            fixture
                .host
                .store
                .call(move |db| prior.check.validate(db))
                .await
                .is_err()
        );
        let host = fixture.host.clone();
        let worker = tokio::spawn(async move { learning::index_next(&host, &host.cancel).await });
        let pending = fixture.mock.embedding().await;
        assert!(
            pending
                .body
                .to_string()
                .contains("intermediate-note-marker")
        );
        change_note(&fixture.host, id, final_op, "latest-note-marker").await;
        pending.reply.send((StatusCode::OK, embedding(0))).unwrap();
        let _ = within(worker).await.unwrap();
        assert_eq!(
            fixture.vector_count().await,
            0,
            "stale HTTP result must not commit"
        );
        let current = query.recall(&fixture.host, 12000).await.unwrap();
        let text = current.texts.join("\n");
        assert!(!text.contains("original-note-marker"));
        assert!(!text.contains("intermediate-note-marker"));
        assert_eq!(text.contains("latest-note-marker"), final_op == "update");
        assert_eq!(current.texts.is_empty(), final_op == "delete");
        fixture.mock.no_embedding();
        fixture.stop().await;
    }
}

#[tokio::test]
async fn recalled_learning_evidence_cancels_inflight_index_and_cannot_be_retrieved() {
    let mut fixture = Fixture::new(|_| {}).await;
    let source = message(1, 456, "诊断服务启动失败时，先检查监听端口是否被占用。");
    fixture.router.dispatch(source.clone()).await.unwrap();
    let host = fixture.host.clone();
    let worker = tokio::spawn(async move { learning::run(&host, 123, &host.cancel).await });
    let request = fixture.mock.model().await;
    let input: Value = serde_json::from_str(
        request.body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let source_key = &input["messages"][0]["source_key"];
    assert_eq!(source_key.as_str(), Some(source.key.key().as_str()));
    let changes: Vec<_> = ["先核对端口占用。", "排查启动异常时检查监听状态。"]
        .into_iter()
        .map(|text| json!({
            "op":"add","kind":"experience","text":format!("source-backed-note-marker：{text}"),"scope":"shared",
            "evidence":[{"source_key":source_key,"quote":"先检查监听端口是否被占用"}]
        }))
        .collect();
    request
        .reply
        .send((
            StatusCode::OK,
            answer(&json!({"changes":changes}).to_string()),
        ))
        .unwrap();
    within(worker).await.unwrap().unwrap();
    fixture.index(|_| 0).await;
    assert_eq!(fixture.vector_count().await, 1);
    let host = fixture.host.clone();
    let worker = tokio::spawn(async move { learning::index_next(&host, &host.cancel).await });
    let pending = fixture.mock.embedding().await;
    assert!(
        pending
            .body
            .to_string()
            .contains("source-backed-note-marker")
    );
    let mut recalled = message(-1, 456, "");
    recalled.quote_seq = Some(1);
    recalled.notification = Some(NudgeEvent {
        event_type: "message_recall".into(),
        initiator: 789,
        target: 789,
        raw_event: Value::Null,
    });
    fixture.router.dispatch(recalled).await.unwrap();
    pending.reply.send((StatusCode::OK, embedding(0))).unwrap();
    let _ = within(worker).await.unwrap();
    assert_eq!(fixture.vector_count().await, 0);
    let query = fixture
        .query(2, 654, "how to diagnose failed startup")
        .await;
    assert!(
        query
            .recall(&fixture.host, 12000)
            .await
            .unwrap()
            .texts
            .is_empty()
    );
    assert!(
        !learning::snapshot(&fixture.host.store, 123)
            .await
            .unwrap()
            .to_string()
            .contains("source-backed-note-marker")
    );
    fixture.mock.no_embedding();
    fixture.stop().await;
}

#[tokio::test]
async fn missing_cache_and_provider_failure_fall_back_without_interrupting_chat() {
    let mut fixture = Fixture::new(|_| {}).await;
    add_note(
        &fixture.host,
        123,
        "fallback-note-marker：故障检查先确认输入。",
        "shared",
    )
    .await;
    let mut first = message(1, 456, "请帮我分析故障");
    first.mentions.push(123);
    fixture.router.dispatch(first).await.unwrap();
    let request = fixture.mock.model().await;
    assert!(
        request.body["messages"]
            .to_string()
            .contains("fallback-note-marker")
    );
    fixture.mock.no_embedding();
    request
        .reply
        .send((StatusCode::OK, answer("没有缓存仍正常回复")))
        .unwrap();
    assert!(
        within(fixture.mock.sends.recv())
            .await
            .unwrap()
            .to_string()
            .contains("没有缓存仍正常回复")
    );
    fixture.index(|_| 0).await;
    let mut next = message(2, 456, "请继续说明排查方法");
    next.mentions.push(123);
    fixture.router.dispatch(next).await.unwrap();
    let request = fixture.mock.embedding().await;
    request
        .reply
        .send((
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":"temporary failure"}),
        ))
        .unwrap();
    let request = fixture.mock.model().await;
    assert!(
        request.body["messages"]
            .to_string()
            .contains("fallback-note-marker")
    );
    request
        .reply
        .send((StatusCode::OK, answer("向量服务失败仍正常回复")))
        .unwrap();
    assert!(
        within(fixture.mock.sends.recv())
            .await
            .unwrap()
            .to_string()
            .contains("向量服务失败仍正常回复")
    );
    fixture.stop().await;
}

#[tokio::test]
async fn semantic_recall_preserves_the_independent_serialized_input_budget() {
    let mut fixture = Fixture::new(|_| {}).await;
    for text in [
        "说明引用：\"先核对\"\n中文🙂\\路径",
        "第二条提示：逐步排查边界条件。",
        "第三条提示：保留能复现错误的输入。",
    ] {
        add_note(&fixture.host, 123, text, "shared").await;
        fixture.index(|_| 0).await;
    }
    let query = fixture.query(1, 456, "how to explain this failure").await;
    let full = semantic_recall(&mut fixture, &query, 12000, 0).await;
    assert_eq!(full.texts.len(), 3);
    let budget = serde_json::to_vec(&json!({"role":"user","content":full.texts[0]}))
        .unwrap()
        .len()
        + 2;
    let limited = semantic_recall(&mut fixture, &query, budget, 0).await;
    assert!(!limited.texts.is_empty());
    assert!(limited.texts.len() < full.texts.len());
    assert!(context_cost(&limited) <= budget);
    fixture.stop().await;
}
