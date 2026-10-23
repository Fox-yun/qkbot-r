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
