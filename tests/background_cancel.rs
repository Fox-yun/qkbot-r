//! A stalled model must not keep a disabled group's background slot occupied.
use axum::{Json, Router, extract::State, routing::post};
use qkbot::{
    config::Config,
    host::Host,
    plugins::ai::chat::Chat,
    types::{Scene, Scope},
};
use rusqlite::params;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

struct ModelRequest {
    body: Value,
    reply: oneshot::Sender<Value>,
}
impl ModelRequest {
    fn conversation(&self) -> String {
        let current = self.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "user")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        serde_json::from_str::<Value>(current).unwrap()["guard"]["conversation_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    fn answer(self, summary: &str) {
        let _ = self.reply.send(json!({
            "model":"mock",
            "choices":[{"finish_reason":"stop","message":{
                "role":"assistant",
                "content":json!({"summary":summary,"changes":[]}).to_string()
            }}],
            "usage":{"prompt_tokens":20,"completion_tokens":5}
        }));
    }
}

async fn model(
    State(requests): State<mpsc::Sender<ModelRequest>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let (reply, response) = oneshot::channel();
    requests.send(ModelRequest { body, reply }).await.unwrap();
    Json(response.await.unwrap_or(Value::Null))
}

fn scope(peer_id: i64) -> Scope {
    Scope {
        bot_id: 1,
        scene: Scene::Group,
        peer_id,
    }
}
fn conversation(peer_id: i64) -> String {
    format!("{}:v0:group", scope(peer_id).key())
}

async fn next_request(requests: &mut mpsc::Receiver<ModelRequest>) -> ModelRequest {
    tokio::time::timeout(Duration::from_secs(3), requests.recv())
        .await
        .expect("background model slot remained occupied")
        .expect("model server stopped")
}

fn compact(chat: &Arc<Chat>) -> JoinHandle<anyhow::Result<()>> {
    let chat = chat.clone();
    tokio::spawn(async move { chat.compact_idle().await })
}

async fn completed(task: JoinHandle<anyhow::Result<()>>) {
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("background task did not release on cancellation")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn disabled_scope_releases_background_slot_without_cancelling_other_scopes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/model", listener.local_addr().unwrap());
    let milky_url = format!("http://{}", listener.local_addr().unwrap());
    let (requests, mut observed) = mpsc::channel(8);
    let app = Router::new()
        .route("/model", post(model))
        .route(
            "/api/get_login_info",
            post(|| async { Json(json!({"status":"ok","retcode":0,"data":{"uin":1}})) }),
        )
        .with_state(requests);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut config = Config::default();
    config.app.database = ":memory:".into();
    config.llm.chat_completions_url = endpoint;
    config.milky.url = milky_url;
    config.llm.model = "mock".into();
    config.llm.max_concurrent_requests = 1;
    config.plugins.ai.defaults.recent_messages = 2;
    config.plugins.ai.defaults.memory_write = false;
    let host = Host::create(config).await.unwrap();
    host.milky.login().await.unwrap();
    host.store
        .call(|db| {
            for peer in [2, 3, 4] {
                let id = conversation(peer);
                let scope = scope(peer).key();
                db.execute(
                    "INSERT INTO ai_conversations(id,scope,mode) VALUES(?1,?2,'group')",
                    params![id, scope],
                )?;
                for seq in 1..=3 {
                    db.execute(
                        "INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,learnable)
                         VALUES(?1,?2,?3,1,10,'user',?4,?5,?6,1)",
                        params![format!("{scope}:{seq}"), scope, id, format!("message {seq}"), peer, seq],
                    )?;
                    db.execute("UPDATE messages SET inbound_position=id WHERE id=last_insert_rowid()", [])?;
                }
            }
            Ok(())
        })
        .await
        .unwrap();
    let chat = Chat::new(host.clone());

    let first = compact(&chat);
    let old_request = next_request(&mut observed).await;
    assert_eq!(old_request.conversation(), conversation(2));
    assert!(!host.llm.as_ref().unwrap().has_capacity());
    // A concurrent maintenance tick must not enqueue another background job.
    completed(compact(&chat)).await;
    assert!(observed.try_recv().is_err());

    host.access.set_enabled(scope(2), false).await.unwrap();
    chat.cancel_before(&conversation(2), 2);
    completed(first).await;
    assert!(host.llm.as_ref().unwrap().has_capacity());

    let second = compact(&chat);
    let other_request = next_request(&mut observed).await;
    assert_eq!(other_request.conversation(), conversation(3));
    // A late result/cancellation from the disabled scope, and a receipt for the
    // current generation, must leave this different scope's request running.
    old_request.answer("must never be stored");
    chat.cancel_scope(&scope(2));
    chat.cancel_before(&conversation(2), 2);
    chat.cancel_before(&conversation(3), 1);
    other_request.answer("other group summary");
    completed(second).await;
    let summaries: (String, String) = host
        .store
        .call(|db| {
            let read = |peer| {
                db.query_row(
                    "SELECT summary FROM ai_conversations WHERE id=?1",
                    [conversation(peer)],
                    |row| row.get::<_, String>(0),
                )
            };
            Ok((read(2)?, read(3)?))
        })
        .await
        .unwrap();
    assert_eq!(summaries, (String::new(), "other group summary".into()));

    // Dropping the caller must release both the registration and model permits.
    let interrupted = compact(&chat);
    let abandoned_request = next_request(&mut observed).await;
    assert_eq!(abandoned_request.conversation(), conversation(4));
    interrupted.abort();
    assert!(interrupted.await.unwrap_err().is_cancelled());
    assert!(host.llm.as_ref().unwrap().has_capacity());
    // Cancellation releases the slot immediately, but the new per-conversation
    // compression cooldown still prevents a hot loop of model requests.
    completed(compact(&chat)).await;
    assert!(observed.try_recv().is_err());
    host.store
        .call(|db| {
            db.execute(
                "UPDATE ai_context_compaction SET last_attempt_at=0 WHERE conversation_id=?1",
                [conversation(4)],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let final_task = compact(&chat);
    let shutdown_request = next_request(&mut observed).await;
    assert_eq!(shutdown_request.conversation(), conversation(4));
    tokio::time::timeout(Duration::from_secs(3), chat.shutdown())
        .await
        .expect("shutdown must cancel and drain background work");
    completed(final_task).await;
    assert!(host.llm.as_ref().unwrap().has_capacity());
    abandoned_request.answer("aborted result");
    shutdown_request.answer("shutdown result");
    let final_summary: String = host
        .store
        .call(|db| {
            Ok(db.query_row(
                "SELECT summary FROM ai_conversations WHERE id=?1",
                [conversation(4)],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(final_summary.is_empty());
    host.store.stop().await.unwrap();
    server.abort();
    let _ = server.await;
}
