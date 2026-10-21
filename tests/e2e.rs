//! Real host + SQLite + local HTTP mocks. No QQ account or model credentials are used.
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    time::Duration,
};

use qkbot::{
    config::Config,
    host::{Host, Router},
    types::{Incoming, MessageKey, Scene, Scope, now},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

struct ModelRequest {
    body: Value,
    reply: oneshot::Sender<Value>,
}
struct MockServer {
    url: String,
    models: mpsc::Receiver<ModelRequest>,
    sends: mpsc::Receiver<Value>,
    quotes: mpsc::Receiver<Value>,
    quote_requests: mpsc::Receiver<ModelRequest>,
    control_quotes: Arc<AtomicBool>,
    drop_send_ack: Arc<AtomicBool>,
    login_id: Arc<AtomicI64>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("mock integration timeout")
}

impl MockServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (model_tx, models) = mpsc::channel(32);
        let (send_tx, sends) = mpsc::channel(32);
        let (quote_tx, quotes) = mpsc::channel(32);
        let (quote_request_tx, quote_requests) = mpsc::channel(32);
        let control_quotes = Arc::new(AtomicBool::new(false));
        let controlled = control_quotes.clone();
        let drop_send_ack = Arc::new(AtomicBool::new(false));
        let lose_ack = drop_send_ack.clone();
        let login_id = Arc::new(AtomicI64::new(123));
        let account = login_id.clone();
        let sequence = Arc::new(AtomicI64::new(100000));
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let task = tokio::spawn(async move {
            let tasks = TaskTracker::new();
            loop {
                let incoming = tokio::select! {
                    _ = shutdown.cancelled() => break,
                    incoming = listener.accept() => incoming,
                };
                let Ok((mut stream, _)) = incoming else {
                    break;
                };
                let model_tx = model_tx.clone();
                let send_tx = send_tx.clone();
                let quote_tx = quote_tx.clone();
                let quote_request_tx = quote_request_tx.clone();
                let controlled = controlled.clone();
                let lose_ack = lose_ack.clone();
                let sequence = sequence.clone();
                let account = account.clone();
                let cancel = shutdown.clone();
                tasks.spawn(async move {
                    let transaction = async {
                        let mut bytes = Vec::new(); let mut buffer = [0;4096];
                        let (end, length) = loop {
                            let Ok(length) = stream.read(&mut buffer).await else { return; };
                            if length == 0 || bytes.len() + length > 1024 * 1024 { return; }
                            bytes.extend_from_slice(&buffer[..length]);
                            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                                let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                                let length: usize = headers.lines().find_map(|line|line.strip_prefix("content-length:").and_then(|n|n.trim().parse().ok())).unwrap_or(0);
                                if bytes.len() >= end + 4 + length { break (end,length); }
                            }
                        };
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let path = headers.split_whitespace().nth(1).unwrap_or("");
                        let Ok(body) = serde_json::from_slice::<Value>(&bytes[end+4..end+4+length]) else { return; };
                        let response = match path {
                            "/api/get_login_info" => json!({"status":"ok","retcode":0,"data":{"uin":account.load(Ordering::Relaxed)}}),
                            "/api/send_group_message" | "/api/send_private_message" => {
                                if send_tx.send(body).await.is_err() { return; }
                                if lose_ack.load(Ordering::Relaxed) { return; }
                                json!({"status":"ok","retcode":0,"data":{"message_seq":sequence.fetch_add(1,Ordering::Relaxed),"time":now()}})
                            }
                            "/api/get_message" => {
                                if quote_tx.send(body.clone()).await.is_err() { return; }
                                if controlled.load(Ordering::Relaxed) {
                                    let (reply, receive) = oneshot::channel();
                                    if quote_request_tx.send(ModelRequest { body, reply }).await.is_err() { return; }
                                    let Ok(response) = receive.await else { return; };
                                    response
                                } else {
                                let marker = format!("quoted-{}-{}-{}",body["message_scene"].as_str().unwrap(),body["peer_id"],body["message_seq"]);
                                json!({"status":"ok","retcode":0,"data":{"message":{
                                    "message_scene":body["message_scene"],"peer_id":body["peer_id"],"message_seq":body["message_seq"],
                                    "sender_id":789,"time":now(),"group_member":{"user_id":789,"group_id":body["peer_id"],"nickname":"引用昵称","card":"引用名片"},"segments":[{"type":"text","data":{"text":marker}}]
                                }}})
                                }
                            }
                            "/v1/chat/completions" => {
                                let (reply, receive) = oneshot::channel();
                                if model_tx.send(ModelRequest { body, reply }).await.is_err() { return; }
                                let Ok(response) = receive.await else { return; }; response
                            }
                            _ => json!({"status":"failed","retcode":-404}),
                        }.to_string();
                        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len());
                        let _ = stream.write_all(response.as_bytes()).await;
                    };
                    tokio::select! {
                        _ = cancel.cancelled() => {},
                        _ = tokio::time::timeout(Duration::from_secs(5),transaction) => {},
                    }
                });
            }
            tasks.close();
            tasks.wait().await;
        });
        Self {
            url,
            models,
            sends,
            quotes,
            quote_requests,
            control_quotes,
            drop_send_ack,
            login_id,
            cancel,
            task,
        }
    }
    async fn model(&mut self) -> ModelRequest {
        within(self.models.recv()).await.expect("model mock closed")
    }
    async fn sent(&mut self) -> String {
        within(self.sends.recv()).await.unwrap()["message"][0]["data"]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    async fn stop(self) {
        self.cancel.cancel();
        within(self.task).await.unwrap();
    }
}

fn answer(text: &str) -> Value {
    json!({"model":"mock", "choices":[{"finish_reason":"stop","message":{"role":"assistant","content":text}}],"usage":{"prompt_tokens":20,"completion_tokens":5}})
}

fn quoted(body: &Value, sender: i64, text: &str) -> Value {
    json!({"status":"ok","retcode":0,"data":{"message":{
        "message_scene":body["message_scene"],"peer_id":body["peer_id"],
        "message_seq":body["message_seq"],"sender_id":sender,"time":now(),
        "segments":[{"type":"text","data":{"text":text}}]
    }}})
}

fn reply_message(seq: i64, quote: i64, text: &str) -> Incoming {
    let mut incoming = message(seq, text);
    incoming.mentions.clear();
    incoming.quote_seq = Some(quote);
    incoming
}

fn message(seq: i64, text: &str) -> Incoming {
    Incoming {
        key: MessageKey {
            scope: Scope {
                bot_id: 123,
                scene: Scene::Group,
                peer_id: 456,
            },
            message_seq: seq,
        },
        sender_id: 789,
        sender_nickname: None,
        sender_card: None,
        text: text.into(),
        mentions: vec![123],
        quote_seq: None,
        received_at: now(),
        raw_message: serde_json::Value::Null,
        notification: None,
    }
}

async fn setup(mock: &MockServer, enable_ai: bool) -> (tempfile::TempDir, Arc<Host>, Router) {
    setup_with(mock, enable_ai, |_| {}).await
}

async fn setup_with(
    mock: &MockServer,
    enable_ai: bool,
    configure: impl FnOnce(&mut Config),
) -> (tempfile::TempDir, Arc<Host>, Router) {
    let temp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.app.database = temp
        .path()
        .join("test.sqlite3")
        .to_string_lossy()
        .into_owned();
    config.milky.url = mock.url.clone();
    config.milky.bot_id = Some(123);
    config.llm.chat_completions_url = format!("{}/v1/chat/completions", mock.url);
    config.llm.model = "mock".into();
    config.llm.api_key = "synthetic".into();
    config.llm.max_concurrent_requests = 1;
    config.plugins.ai.enabled = enable_ai;
    config.plugins.ai.defaults.allowed_tools = vec!["clock_now".into(), "reminder_create".into()];
    config.plugins.reminder.enabled = true;
    config.limits.send_cooldown_secs = 0;
    configure(&mut config);
    let host = Host::create(config).await.unwrap();
    host.milky.login().await.unwrap();
    let router = Router::new(host.clone());
    (temp, host, router)
}

#[tokio::test]
async fn nicknames_follow_current_history_and_quoted_messages_without_changing_evidence() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, true).await;
    let mut first = message(1, "原始正文喜欢白茶");
    first.sender_nickname = Some("账号昵称".into());
    first.sender_card = Some("旧名片".into());
    router.dispatch(first).await.unwrap();
    let request = mock.model().await;
    let payload = request.body["messages"].to_string();
    assert!(payload.contains("旧名片"));
    assert!(payload.contains("789"));
    request.reply.send(answer("已收到第一条消息")).unwrap();
    mock.sent().await;
    let mut next = message(2, "现在改了名片");
    next.sender_nickname = Some("账号昵称".into());
    next.sender_card = Some("新名片".into());
    next.quote_seq = Some(1);
    router.dispatch(next).await.unwrap();
    let request = mock.model().await;
    let payload = request.body["messages"].to_string();
    assert!(
        payload.contains("旧名片"),
        "history must retain its original name snapshot"
    );
    assert!(
        payload.contains("新名片"),
        "current message must use its new name"
    );
    assert!(payload.contains("引用消息"));
    assert!(
        payload.contains("旧名片"),
        "local quotes retain the original name snapshot"
    );
    request.reply.send(answer("已收到新名片")).unwrap();
    mock.sent().await;
    let saved: (String, String, String) = host
        .store
        .call(|db| {
            Ok(db.query_row(
        "SELECT content,sender_nickname,sender_card FROM messages WHERE key='123:group:456:1'",
        [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))
    )?)
        })
        .await
        .unwrap();
    assert_eq!(
        saved,
        (
            "原始正文喜欢白茶".into(),
            "账号昵称".into(),
            "旧名片".into()
        )
    );
    assert!(mock.quotes.try_recv().is_err());
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn facts_continue_with_summary_off_and_summary_resumes_without_relearning() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.defaults.summary = false;
        config.plugins.ai.defaults.memory_read = false;
        config.plugins.ai.defaults.recent_messages = 2;
        config.plugins.ai.learn_all_group_messages = true;
        config.plugins.ai.compact_after_messages = 4;
    })
    .await;
    for seq in 1..=6 {
        let text = if seq == 1 {
            "我喜欢白茶".to_owned()
        } else {
            format!("background-{seq}")
        };
        let mut incoming = message(seq, &text);
        incoming.mentions.clear();
        router.dispatch(incoming).await.unwrap();
    }
    let chat = router.chat.clone();
    let work = tokio::spawn(async move { chat.compact_idle().await });
    let request = mock.model().await;
    let batch: Value = serde_json::from_str(
        request.body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(batch["summary_enabled"], false);
    assert_eq!(batch["memory_write"], true);
    let learned_through = batch["through_position"].as_i64().unwrap();
    request
        .reply
        .send(answer(
            &json!({"summary":"","changes":[{
                "op":"upsert","id":null,"subject_id":789,"kind":"preference","text":"喜欢白茶",
                "evidence":[{"source_key":"123:group:456:1","quote":"喜欢白茶"}],"ttl_days":null
            }]})
            .to_string(),
        ))
        .unwrap();
    within(work).await.unwrap().unwrap();
    let state: (i64, i64, String) = host
        .store
        .call(|connection| {
            Ok(connection.query_row(
                "SELECT compacted_until,memory_processed_until,summary FROM ai_conversations",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(state.0, 0);
    assert_eq!(state.1, learned_through);
    assert!(state.2.is_empty());

    web_change(
        &host,
        &router,
        qkbot::admin::memory::Action::Settings {
            settings: qkbot::config::AiSettings {
                summary: true,
                ..host.config.plugins.ai.defaults.clone()
            },
        },
    )
    .await;
    let chat = router.chat.clone();
    let work = tokio::spawn(async move { chat.compact_idle().await });
    let request = mock.model().await;
    let batch: Value = serde_json::from_str(
        request.body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(batch["summary_enabled"], true);
    assert_eq!(batch["memory_write"], false);
    let summarized_through = batch["through_position"].as_i64().unwrap();
    assert!(
        batch["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|source| source["memory_eligible"] == false)
    );
    request
        .reply
        .send(answer(
            &json!({"summary":"用户789喜欢白茶；summary-restored-marker","changes":[]}).to_string(),
        ))
        .unwrap();
    within(work).await.unwrap().unwrap();
    let (summary_cursor, memory_cursor, memories): (i64, i64, i64) = host.store.call(|connection| Ok(connection.query_row(
        "SELECT compacted_until,memory_processed_until,(SELECT count(*) FROM ai_memories) FROM ai_conversations", [],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))
    )?)).await.unwrap();
    // Facts now include recent complete messages independently of summary retention.
    // Resuming summaries covers only the old prefix and must not move facts backward.
    assert_eq!(summary_cursor, summarized_through);
    assert_eq!(memory_cursor, learned_through);
    assert!(summary_cursor < memory_cursor);
    assert_eq!(memories, 1);
    router
        .dispatch(message(8, "use-restored-summary"))
        .await
        .unwrap();
    let request = mock.model().await;
    assert!(
        request.body["messages"]
            .to_string()
            .contains("summary-restored-marker")
    );
    for recent in ["background-5", "background-6"] {
        assert!(request.body["messages"].to_string().contains(recent));
    }
    request
        .reply
        .send(answer("summary-used-successfully"))
        .unwrap();
    assert_eq!(mock.sent().await, "summary-used-successfully");
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn unverified_account_defers_confirmed_reminders_until_original_account_recovers() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, false).await;
    router
        .dispatch(message(1, "/remind add 60 keep-this-reminder"))
        .await
        .unwrap();
    assert!(mock.sent().await.contains("已创建提醒"));
    host.store
        .call(|connection| {
            connection.execute("UPDATE reminder_jobs SET due_at=?1", [now() - 1])?;
            Ok(())
        })
        .await
        .unwrap();
    mock.login_id.store(999, Ordering::Relaxed);
    assert!(host.milky.login().await.is_err());
    router.reminders().await.unwrap();
    let state: String = host
        .store
        .call(|connection| {
            Ok(connection.query_row("SELECT status FROM reminder_jobs", [], |row| row.get(0))?)
        })
        .await
        .unwrap();
    assert_eq!(state, "pending");
    assert!(mock.sends.try_recv().is_err());
    assert!(matches!(
        host.send(
            &message(1, "").key.scope,
            "reminder",
            None,
            0,
            "must-not-send".into()
        )
        .await
        .unwrap(),
        qkbot::milky::SendOutcome::Deferred { .. }
    ));
    assert!(mock.sends.try_recv().is_err());
    mock.login_id.store(123, Ordering::Relaxed);
    assert_eq!(host.milky.login().await.unwrap(), 123);
    router.reminders().await.unwrap();
    assert!(mock.sent().await.contains("keep-this-reminder"));
    let state: String = host
        .store
        .call(|connection| {
            Ok(connection.query_row("SELECT status FROM reminder_jobs", [], |row| row.get(0))?)
        })
        .await
        .unwrap();
    assert_eq!(state, "sent");
    within(router.shutdown()).await.unwrap();
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn reset_and_mode_controls_commit_before_following_message_ingress() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, true).await;
    router
        .dispatch(message(1, "old-generation-question"))
        .await
        .unwrap();
    let abandoned = mock.model().await;
    web_change(&host, &router, qkbot::admin::memory::Action::Reset).await;
    // The web mutation commits and cancels queued work before new ingress.
    router
        .dispatch(message(3, "fresh-generation-question"))
        .await
        .unwrap();
    let next = mock.model().await;
    assert!(
        next.body["messages"]
            .to_string()
            .contains("fresh-generation-question")
    );
    assert!(
        !next.body["messages"]
            .to_string()
            .contains("old-generation-question")
    );
    let _ = abandoned.reply.send(answer("obsolete-answer"));
    next.reply.send(answer("fresh-generation-answer")).unwrap();
    let receipts = [mock.sent().await];
    assert!(
        receipts
            .iter()
            .any(|text| text == "fresh-generation-answer")
    );

    web_change(
        &host,
        &router,
        qkbot::admin::memory::Action::Mode {
            mode: "user".into(),
        },
    )
    .await;
    let mut personal = message(5, "new-personal-question");
    personal.sender_id = 999;
    router.dispatch(personal).await.unwrap();
    let next = mock.model().await;
    assert!(
        next.body["messages"]
            .to_string()
            .contains("new-personal-question")
    );
    assert!(
        !next.body["messages"]
            .to_string()
            .contains("fresh-generation")
    );
    let stored: (String, i64) = host
        .store
        .call(|connection| {
            Ok(connection.query_row(
                "SELECT conversation_id,epoch FROM messages WHERE key='123:group:456:5'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert!(stored.0.ends_with(":v1:user:999"));
    next.reply.send(answer("personal-answer")).unwrap();
    assert_eq!(mock.sent().await, "personal-answer");
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn usage_logging_failure_does_not_discard_or_repeat_successful_model_response() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, true).await;
    host.store.call(|connection| {
        connection.execute_batch("CREATE TRIGGER reject_usage BEFORE INSERT ON usage_daily BEGIN SELECT RAISE(FAIL,'simulated usage log failure'); END;")?;
        Ok(())
    }).await.unwrap();
    router
        .dispatch(message(1, "question-with-log-failure"))
        .await
        .unwrap();
    mock.model()
        .await
        .reply
        .send(answer("answer-survives-log-failure"))
        .unwrap();
    assert_eq!(mock.sent().await, "answer-survives-log-failure");
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn queued_prompt_sees_previous_reply_excludes_future_input_and_deduplicates() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, true).await;
    router
        .dispatch(message(1, "first-question-A"))
        .await
        .unwrap();
    let first = mock.model().await;
    router
        .dispatch(message(2, "second-question-B"))
        .await
        .unwrap();
    let mut future = message(3, "later-background-C");
    future.mentions.clear();
    router.dispatch(future).await.unwrap();
    router
        .dispatch(message(2, "second-question-B"))
        .await
        .unwrap();
    first.reply.send(answer("confirmed-reply-A")).unwrap();
    assert_eq!(mock.sent().await, "confirmed-reply-A");
    let second = mock.model().await;
    let prompt = second.body["messages"].to_string();
    assert!(prompt.contains("confirmed-reply-A"), "{prompt}");
    assert!(prompt.contains("second-question-B"));
    assert!(!prompt.contains("later-background-C"));
    second.reply.send(answer("confirmed-reply-B")).unwrap();
    assert_eq!(mock.sent().await, "confirmed-reply-B");
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn reset_during_model_wait_releases_capacity_and_suppresses_stale_reply() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, true).await;
    router
        .dispatch(message(1, "old-context-question"))
        .await
        .unwrap();
    let old = mock.model().await;
    web_change(&host, &router, qkbot::admin::memory::Action::Reset).await;

    router
        .dispatch(message(3, "fresh-context-question"))
        .await
        .unwrap();
    let fresh = mock.model().await;
    assert!(
        !fresh.body["messages"]
            .to_string()
            .contains("old-context-question")
    );
    let _ = old.reply.send(answer("stale-answer-must-not-send"));
    fresh.reply.send(answer("fresh-answer")).unwrap();
    assert_eq!(mock.sent().await, "fresh-answer");
    within(router.shutdown()).await.unwrap();
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn disabled_ai_keeps_clock_commands_and_does_not_call_model() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, false).await;
    router.dispatch(message(1, "/clock")).await.unwrap();
    let clock = mock.sent().await;
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(&clock)
            .unwrap()
            .offset()
            .local_minus_utc(),
        8 * 3600
    );
    router
        .dispatch(message(2, "please-chat-with-me"))
        .await
        .unwrap();
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn native_write_tool_waits_for_owner_confirmation_and_commits_once() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, true).await;
    router
        .dispatch(message(1, "remind-me-later"))
        .await
        .unwrap();
    let request = mock.model().await;
    request.reply.send(json!({"model":"mock","choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,
        "tool_calls":[{"id":"call1","type":"function","function":{"name":"reminder_create","arguments":"{\"after_seconds\":60,\"text\":\"喝水\"}"}}]}}]})).unwrap();
    let preview = mock.sent().await;
    let token = preview
        .split("/bot confirm ")
        .nth(1)
        .expect("confirmation command")
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let before: i64 = host
        .store
        .call(|c| Ok(c.query_row("SELECT COUNT(*) FROM reminder_jobs", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(before, 0);
    let mut wrong = message(2, &format!("/bot confirm {token}"));
    wrong.sender_id = 999;
    router.dispatch(wrong).await.unwrap();
    assert!(mock.sent().await.contains("指令未完成"));
    router
        .dispatch(message(3, &format!("/bot confirm {token}")))
        .await
        .unwrap();
    let receipt = mock.sent().await;
    assert!(!receipt.contains("指令未完成"), "{receipt}");
    router
        .dispatch(message(4, &format!("/bot confirm {token}")))
        .await
        .unwrap();
    let _ = mock.sent().await;
    let count: i64 = host
        .store
        .call(|c| Ok(c.query_row("SELECT COUNT(*) FROM reminder_jobs", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(count, 1);
    within(router.shutdown()).await.unwrap();
    assert!(
        mock.models.try_recv().is_err(),
        "confirmation must not regenerate model arguments"
    );
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn per_conversation_queue_rejects_excess_without_extra_model_work() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.limits.queued_per_conversation = 1;
        config.limits.queued_global = 4;
    })
    .await;
    router.dispatch(message(1, "running-A")).await.unwrap();
    let first = mock.model().await;
    router.dispatch(message(2, "queued-B")).await.unwrap();
    router.dispatch(message(3, "rejected-C")).await.unwrap();
    assert_eq!(router.chat.pending(), 1);
    assert!(mock.sent().await.contains("队列已满"));
    first.reply.send(answer("A-complete")).unwrap();
    assert_eq!(mock.sent().await, "A-complete");
    let next = mock.model().await;
    assert!(next.body["messages"].to_string().contains("queued-B"));
    assert!(!next.body["messages"].to_string().contains("rejected-C"));
    next.reply.send(answer("B-complete")).unwrap();
    assert_eq!(mock.sent().await, "B-complete");
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn global_queue_counts_other_sessions_waiting_for_turn_capacity() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.limits.queued_per_conversation = 1;
        config.limits.queued_global = 2;
    })
    .await;
    router
        .dispatch(message(1, "running-first-group"))
        .await
        .unwrap();
    let first = mock.model().await;
    for (group, text) in [
        (654, "queued-second-group"),
        (655, "queued-third-group"),
        (656, "rejected-fourth-group"),
    ] {
        let mut incoming = message(1, text);
        incoming.key.scope.peer_id = group;
        router.dispatch(incoming).await.unwrap();
    }
    assert_eq!(router.chat.pending(), 2);
    assert!(mock.sent().await.contains("队列已满"));
    first.reply.send(answer("first-complete")).unwrap();
    assert_eq!(mock.sent().await, "first-complete");
    for marker in ["queued-second-group", "queued-third-group"] {
        let queued = mock.model().await;
        assert!(queued.body["messages"].to_string().contains(marker));
        assert!(
            !queued.body["messages"]
                .to_string()
                .contains("rejected-fourth-group")
        );
        queued.reply.send(answer(marker)).unwrap();
        assert_eq!(mock.sent().await, marker);
    }
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn user_mode_separates_people_groups_and_private_conversations() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.defaults.mode = "user".into();
    })
    .await;
    let original = message(1, "owner-group-secret");
    let mut other_user = message(2, "other-user-secret");
    other_user.sender_id = 999;
    let mut private = message(1, "private-secret");
    private.key.scope.scene = Scene::Friend;
    private.key.scope.peer_id = 789;
    private.mentions.clear();
    let mut other_group = message(1, "other-group-secret");
    other_group.key.scope.peer_id = 654;
    let mut previous = Vec::new();
    for incoming in [original, other_user, private, other_group] {
        let own = incoming.text.clone();
        router.dispatch(incoming).await.unwrap();
        let request = mock.model().await;
        let prompt = request.body["messages"].to_string();
        assert!(prompt.contains(&own));
        for secret in &previous {
            assert!(
                !prompt.contains(secret),
                "unexpected cross-conversation text: {prompt}"
            );
        }
        request.reply.send(answer(&format!("reply-{own}"))).unwrap();
        mock.sent().await;
        previous.push(own);
    }
    router
        .dispatch(message(3, "follow-up-original-owner"))
        .await
        .unwrap();
    let followup = mock.model().await;
    let prompt = followup.body["messages"].to_string();
    assert!(prompt.contains("owner-group-secret"));
    assert!(prompt.contains("reply-owner-group-secret"));
    for secret in previous.iter().skip(1) {
        assert!(!prompt.contains(secret));
    }
    followup.reply.send(answer("owner-follow-up")).unwrap();
    assert_eq!(mock.sent().await, "owner-follow-up");
    within(router.shutdown()).await.unwrap();
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn quoted_same_sequence_is_resolved_with_full_scene_and_peer_key() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |_| {}).await;
    let mut previous = Vec::new();
    for (scene, peer) in [
        (Scene::Group, 456),
        (Scene::Group, 654),
        (Scene::Friend, 789),
    ] {
        let mut incoming = message(1, "explain-this-quote");
        incoming.key.scope.scene = scene;
        incoming.key.scope.peer_id = peer;
        incoming.quote_seq = Some(77);
        router.dispatch(incoming).await.unwrap();
        let request = mock.model().await;
        let lookup = within(mock.quotes.recv()).await.unwrap();
        assert_eq!(
            lookup,
            json!({"message_scene":scene.as_str(),"peer_id":peer,"message_seq":77})
        );
        let marker = format!("quoted-{}-{peer}-77", scene.as_str());
        let prompt = request.body["messages"].to_string();
        assert!(prompt.contains(&marker));
        for old in &previous {
            assert!(!prompt.contains(old));
        }
        previous.push(marker);
        request.reply.send(answer("quote-explained")).unwrap();
        assert_eq!(mock.sent().await, "quote-explained");
    }
    within(router.shutdown()).await.unwrap();
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn submitted_send_with_lost_ack_is_unknown_and_not_replayed() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, false).await;
    mock.drop_send_ack.store(true, Ordering::Relaxed);
    let outcome = host
        .send(
            &message(1, "").key.scope,
            "host",
            None,
            0,
            "one-submission-only".into(),
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, qkbot::milky::SendOutcome::Unknown { .. }),
        "{outcome:?}"
    );
    assert_eq!(mock.sent().await, "one-submission-only");
    router.maintenance().await.unwrap();
    router.maintenance().await.unwrap();
    let rows: Vec<(String, String)> = host
        .store
        .call(|connection| {
            let mut statement = connection.prepare("SELECT state,body FROM outbox")?;
            Ok(statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "unknown");
    let body: qkbot::types::OutgoingMessage = serde_json::from_str(&rows[0].1).unwrap();
    assert_eq!(body.text_content(), "one-submission-only");
    within(router.shutdown()).await.unwrap();
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn reply_trigger_uses_local_plugin_messages_and_preserves_original_evidence() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.read_group_background = false;
    })
    .await;
    host.send(
        &message(1, "").key.scope,
        "clock",
        None,
        0,
        "时钟插件原文".into(),
    )
    .await
    .unwrap();
    assert_eq!(mock.sent().await, "时钟插件原文");
    let incoming = reply_message(1, 100000, "为什么？");
    router.dispatch(incoming.clone()).await.unwrap();
    router.dispatch(incoming).await.unwrap();
    let request = mock.model().await;
    let payload = request.body["messages"].to_string();
    assert!(payload.contains("为什么？") && payload.contains("时钟插件原文"));
    assert!(payload.contains("来源插件：clock"));
    let stored: (String, String, bool) = host
        .store
        .call(|db| {
            Ok(db.query_row(
                "SELECT content,role,learnable FROM messages WHERE key='123:group:456:1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(stored, ("为什么？".into(), "user".into(), true));
    request.reply.send(answer("解释")).unwrap();
    assert_eq!(mock.sent().await, "解释");

    // Native commands retain ownership even when quoting the bot.
    router
        .dispatch(reply_message(2, 100000, "/clock"))
        .await
        .unwrap();
    mock.sent().await;
    assert!(mock.models.try_recv().is_err());

    // A local human message, including a dropped background message, is final
    // evidence of a different sender; a remote preview cannot override it.
    router
        .dispatch(reply_message(3, 1, "回复其他人"))
        .await
        .unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.quotes.try_recv().is_err());

    let mut overlapping = reply_message(4, 100000, "小鱼为什么？");
    overlapping.mentions.push(123);
    router.dispatch(overlapping.clone()).await.unwrap();
    router.dispatch(overlapping).await.unwrap();
    mock.model().await.reply.send(answer("只回复一次")).unwrap();
    mock.sent().await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err() && mock.quotes.try_recv().is_err());
    mock.stop().await;
}

#[tokio::test]
async fn reply_trigger_remote_lookup_deduplicates_orders_and_reuses_the_snapshot() {
    let mut mock = MockServer::start().await;
    mock.control_quotes.store(true, Ordering::Relaxed);
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.read_group_background = false;
    })
    .await;
    let first = reply_message(1, 77, "先来的引用问题");
    within(router.dispatch(first.clone())).await.unwrap();
    let lookup = within(mock.quote_requests.recv()).await.unwrap();
    router.dispatch(first).await.unwrap();
    router.dispatch(message(2, "后来的直接提及")).await.unwrap();
    assert!(mock.models.try_recv().is_err());

    let mut independent = message(3, "另一个群不等待");
    independent.key.scope.peer_id = 654;
    router.dispatch(independent).await.unwrap();
    let other = mock.model().await;
    assert!(
        other.body["messages"]
            .to_string()
            .contains("另一个群不等待")
    );
    other.reply.send(answer("其他群回复")).unwrap();
    mock.sent().await;
    lookup
        .reply
        .send(quoted(&lookup.body, 123, "本地缺失的机器人原文"))
        .unwrap();
    let first = mock.model().await;
    let payload = first.body["messages"].to_string();
    assert!(payload.contains("先来的引用问题") && payload.contains("本地缺失的机器人原文"));
    assert!(!payload.contains("后来的直接提及"));
    first.reply.send(answer("先回复")).unwrap();
    mock.sent().await;
    let second = mock.model().await;
    assert!(
        second.body["messages"]
            .to_string()
            .contains("后来的直接提及")
    );
    second.reply.send(answer("后回复")).unwrap();
    mock.sent().await;
    assert!(
        mock.quote_requests.try_recv().is_err(),
        "the model reuses the lookup snapshot"
    );
    let saved: String = host
        .store
        .call(|db| {
            Ok(db.query_row(
                "SELECT content FROM messages WHERE key='123:group:456:1'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(saved, "先来的引用问题");
    within(router.shutdown()).await.unwrap();
    mock.stop().await;
}

#[tokio::test]
async fn reply_trigger_rejects_other_senders_wrong_keys_and_untrusted_reply_previews() {
    let mut mock = MockServer::start().await;
    mock.control_quotes.store(true, Ordering::Relaxed);
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.defaults.paused = true;
        config.plugins.ai.defaults.once = true;
    })
    .await;
    for (seq, sender, wrong_key) in [(1, 789, false), (2, 123, true)] {
        let mut incoming = reply_message(seq, 77, "伪造引用预览");
        incoming.raw_message =
            json!({"segments":[{"type":"reply","data":{"message_seq":77,"sender_id":123}}]});
        router.dispatch(incoming).await.unwrap();
        let lookup = within(mock.quote_requests.recv()).await.unwrap();
        let mut response = quoted(&lookup.body, sender, "不可信目标");
        if wrong_key {
            response["data"]["message"]["peer_id"] = json!(654);
        }
        lookup.reply.send(response).unwrap();
        // A following direct trigger is an ordering barrier; it must still be
        // able to consume the one-shot permission the failed quote did not use.
        router
            .dispatch(message(seq + 10, "真正的提及"))
            .await
            .unwrap();
        let request = mock.model().await;
        let current = request.body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(current.contains("真正的提及"));
        request.reply.send(answer("一次放行")).unwrap();
        mock.sent().await;
        web_change(
            &host,
            &router,
            qkbot::admin::memory::Action::Settings {
                settings: host.config.plugins.ai.defaults.clone(),
            },
        )
        .await;
    }
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    mock.stop().await;
}

#[tokio::test]
async fn reply_trigger_timeouts_are_bounded_and_do_not_suppress_mentions() {
    let mut mock = MockServer::start().await;
    mock.control_quotes.store(true, Ordering::Relaxed);
    let (_temp, _host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.read_group_background = false;
    })
    .await;
    router
        .dispatch(reply_message(1, 77, "仅引用超时"))
        .await
        .unwrap();
    let stalled = within(mock.quote_requests.recv()).await.unwrap();
    router
        .dispatch(message(2, "超时之后仍能提及"))
        .await
        .unwrap();
    let request = mock.model().await;
    assert!(
        request.body["messages"]
            .to_string()
            .contains("超时之后仍能提及")
    );
    assert!(!request.body["messages"].to_string().contains("仅引用超时"));
    request.reply.send(answer("提及回复")).unwrap();
    mock.sent().await;
    drop(stalled);

    let mut direct = reply_message(3, 88, "带引用的提及超时");
    direct.mentions.push(123);
    router.dispatch(direct).await.unwrap();
    let stalled = within(mock.quote_requests.recv()).await.unwrap();
    let request = mock.model().await;
    assert!(request.body["messages"].to_string().contains("引用不可用"));
    request.reply.send(answer("仍然回复")).unwrap();
    mock.sent().await;
    drop(stalled);
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    mock.stop().await;
}

#[tokio::test]
async fn reply_trigger_stale_lookups_cannot_consume_a_new_generations_once_permission() {
    use qkbot::admin::memory::Action;
    for change in ["reset", "settings", "scope", "account", "forget"] {
        let mut mock = MockServer::start().await;
        mock.control_quotes.store(true, Ordering::Relaxed);
        let (_temp, host, router) = setup_with(&mock, true, |config| {
            config.plugins.ai.defaults.paused = true;
            config.plugins.ai.defaults.once = true;
            config.plugins.ai.read_group_background = false;
        })
        .await;
        router
            .dispatch(reply_message(1, 77, "过期的引用提问"))
            .await
            .unwrap();
        let lookup = within(mock.quote_requests.recv()).await.unwrap();
        match change {
            "reset" => web_change(&host, &router, Action::Reset).await,
            "settings" => {
                web_change(
                    &host,
                    &router,
                    Action::Settings {
                        settings: qkbot::config::AiSettings {
                            persona: "理性搭档".into(),
                            ..host.config.plugins.ai.defaults.clone()
                        },
                    },
                )
                .await
            }
            "scope" => {
                let scope = message(1, "").key.scope;
                host.access.set_enabled(scope.clone(), false).await.unwrap();
                router.chat.cancel_scope(&scope);
                host.access.set_enabled(scope, true).await.unwrap();
            }
            "account" => {
                mock.login_id.store(999, Ordering::Relaxed);
                assert!(host.milky.login().await.is_err());
                mock.login_id.store(123, Ordering::Relaxed);
                host.milky.login().await.unwrap();
            }
            "forget" => {
                host.store
                    .call(|db| {
                        db.execute(
                            "INSERT INTO ai_revoked_sources VALUES('123:group:456:77',1)",
                            [],
                        )?;
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let _ = lookup
            .reply
            .send(quoted(&lookup.body, 123, "过期机器人内容"));
        router.dispatch(message(2, "新的有效提及")).await.unwrap();
        let request = mock.model().await;
        let payload = request.body["messages"].to_string();
        assert!(payload.contains("新的有效提及"), "{change}");
        assert!(
            !payload.contains("过期的引用提问") && !payload.contains("过期机器人内容"),
            "{change}"
        );
        request.reply.send(answer("新版本的一次回复")).unwrap();
        mock.sent().await;
        within(router.shutdown()).await.unwrap();
        assert!(mock.models.try_recv().is_err(), "{change}");
        mock.stop().await;
    }
}

#[tokio::test]
async fn reply_trigger_switch_can_disable_quotes_without_disabling_mentions_or_private_chat() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.defaults.reply_to_bot = false;
        config.plugins.ai.read_group_background = false;
    })
    .await;
    host.send(
        &message(1, "").key.scope,
        "host",
        None,
        0,
        "机器人消息".into(),
    )
    .await
    .unwrap();
    mock.sent().await;
    router
        .dispatch(reply_message(1, 100000, "默认关闭引用"))
        .await
        .unwrap();
    assert!(mock.models.try_recv().is_err() && mock.quotes.try_recv().is_err());
    let mut own = reply_message(2, 100000, "机器人自己回复");
    own.sender_id = 123;
    own.mentions.push(123);
    router.dispatch(own).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    for mut incoming in [message(3, "原有提及"), reply_message(4, 0, "原有私聊")] {
        if incoming.key.message_seq == 4 {
            incoming.key.scope.scene = Scene::Friend;
            incoming.key.scope.peer_id = 789;
            incoming.quote_seq = None;
        }
        router.dispatch(incoming).await.unwrap();
        mock.model()
            .await
            .reply
            .send(answer("原有触发回复"))
            .unwrap();
        mock.sent().await;
    }
    web_change(
        &host,
        &router,
        qkbot::admin::memory::Action::Settings {
            settings: qkbot::config::AiSettings {
                reply_to_bot: true,
                ..host.config.plugins.ai.defaults.clone()
            },
        },
    )
    .await;
    router
        .dispatch(reply_message(5, 100000, "本会话单独开启"))
        .await
        .unwrap();
    mock.model().await.reply.send(answer("引用已开启")).unwrap();
    mock.sent().await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.quotes.try_recv().is_err());
    mock.stop().await;
}

#[tokio::test]
async fn reply_trigger_lookup_concurrency_and_waiting_queue_are_bounded() {
    let mut mock = MockServer::start().await;
    mock.control_quotes.store(true, Ordering::Relaxed);
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.limits.queued_per_conversation = 1;
        config.limits.queued_global = 4;
        config.plugins.ai.read_group_background = false;
    })
    .await;
    let mut held = vec![];
    for peer in [456, 654] {
        let mut incoming = reply_message(1, 77, "等待查询");
        incoming.key.scope.peer_id = peer;
        router.dispatch(incoming).await.unwrap();
        held.push(within(mock.quote_requests.recv()).await.unwrap());
    }
    let mut excess = reply_message(1, 77, "补查并发已满");
    excess.key.scope.peer_id = 655;
    router.dispatch(excess).await.unwrap();
    router
        .dispatch(message(2, "排队等待前面的引用"))
        .await
        .unwrap();
    router
        .dispatch(message(3, "超出单会话预处理容量"))
        .await
        .unwrap();
    assert!(mock.sent().await.contains("队列已满"));
    let mut barrier = message(2, "其他群仍可正常处理");
    barrier.key.scope.peer_id = 655;
    router.dispatch(barrier).await.unwrap();
    let request = mock.model().await;
    assert!(
        request.body["messages"]
            .to_string()
            .contains("其他群仍可正常处理")
    );
    request.reply.send(answer("正常回复")).unwrap();
    mock.sent().await;
    assert!(mock.quote_requests.try_recv().is_err());
    for lookup in held {
        lookup
            .reply
            .send(quoted(&lookup.body, 789, "其他成员"))
            .unwrap();
    }
    let request = mock.model().await;
    assert!(
        request.body["messages"]
            .to_string()
            .contains("排队等待前面的引用")
    );
    assert!(
        !request.body["messages"]
            .to_string()
            .contains("超出单会话预处理容量")
    );
    request.reply.send(answer("排队消息回复")).unwrap();
    mock.sent().await;
    let calls: i64 = host
        .store
        .call(|db| {
            Ok(db.query_row(
                "SELECT COALESCE(sum(requests),0) FROM usage_daily",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(calls, 2);
    within(router.shutdown()).await.unwrap();
    mock.stop().await;
}

#[tokio::test]
async fn reply_trigger_keeps_current_user_context_and_rechecks_queued_quote_revocation() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.defaults.mode = "user".into();
    })
    .await;
    router
        .dispatch(message(1, "用户甲的独立历史"))
        .await
        .unwrap();
    mock.model()
        .await
        .reply
        .send(answer("可以公开引用的一条回复"))
        .unwrap();
    mock.sent().await;
    let mut second_user = reply_message(2, 100000, "用户乙的追问");
    second_user.sender_id = 888;
    router.dispatch(second_user).await.unwrap();
    let request = mock.model().await;
    let payload = request.body["messages"].to_string();
    assert!(payload.contains("用户乙的追问") && payload.contains("可以公开引用的一条回复"));
    assert!(!payload.contains("用户甲的独立历史"));
    request.reply.send(answer("乙的回答")).unwrap();
    mock.sent().await;

    // A reset permits an explicit quote of the one old reply, without reviving
    // the old question or any other member's context.
    web_change(&host, &router, qkbot::admin::memory::Action::Reset).await;
    router
        .dispatch(reply_message(3, 100000, "重置后显式引用"))
        .await
        .unwrap();
    let blocker = mock.model().await;
    let payload = blocker.body["messages"].to_string();
    assert!(payload.contains("可以公开引用的一条回复"));
    assert!(!payload.contains("用户甲的独立历史") && !payload.contains("乙的回答"));
    let mut queued = reply_message(4, 100000, "遗忘前已缓存的引用");
    queued.sender_id = 888;
    router.dispatch(queued).await.unwrap();
    host.store
        .call(|db| {
            db.execute(
                "INSERT INTO ai_revoked_sources VALUES('123:group:456:100000',1)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mut barrier = message(5, "遗忘后的普通提及");
    barrier.sender_id = 888;
    router.dispatch(barrier).await.unwrap();
    blocker.reply.send(answer("在途回答")).unwrap();
    mock.sent().await;
    let request = mock.model().await;
    let current = request.body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(current.contains("遗忘后的普通提及"));
    assert!(
        !request.body["messages"]
            .to_string()
            .contains("可以公开引用的一条回复")
    );
    request.reply.send(answer("普通回复")).unwrap();
    mock.sent().await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    mock.stop().await;
}

#[tokio::test]
async fn confirmed_send_survives_ledger_failure_and_restart_never_replays_it() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, false).await;
    host.store.call(|connection| {
        connection.execute_batch("CREATE TRIGGER reject_confirmed_ledger BEFORE UPDATE OF state ON outbox WHEN NEW.state='confirmed' BEGIN SELECT RAISE(ABORT, 'synthetic ledger failure'); END;")?;
        Ok(())
    }).await.unwrap();
    let outcome = host
        .send(
            &message(1, "").key.scope,
            "host",
            None,
            0,
            "acknowledged-once".into(),
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, qkbot::milky::SendOutcome::Confirmed { .. }),
        "{outcome:?}"
    );
    assert_eq!(mock.sent().await, "acknowledged-once");
    let state: String = host
        .store
        .call(|connection| {
            Ok(connection.query_row("SELECT state FROM outbox", [], |row| row.get(0))?)
        })
        .await
        .unwrap();
    assert_eq!(state, "submitted");
    let config = (*host.config).clone();
    within(router.shutdown()).await.unwrap();
    drop(router);
    drop(host);
    let restarted = Host::create(config).await.unwrap();
    let state: String = restarted
        .store
        .call(|connection| {
            Ok(connection.query_row("SELECT state FROM outbox", [], |row| row.get(0))?)
        })
        .await
        .unwrap();
    assert_eq!(state, "unknown");
    within(Router::new(restarted).shutdown()).await.unwrap();
    assert!(mock.sends.try_recv().is_err());
    mock.stop().await;
}

#[tokio::test]
async fn native_clock_result_has_matching_tool_id_and_final_request_omits_tools() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) =
        setup_with(&mock, true, |config| config.limits.max_model_calls = 2).await;
    router
        .dispatch(message(1, "what-time-is-it"))
        .await
        .unwrap();
    let first = mock.model().await;
    assert!(
        first.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "clock_now")
    );
    first.reply.send(json!({"model":"mock","choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,
        "tool_calls":[{"id":"clock-call-123","type":"function","function":{"name":"clock_now","arguments":"{}"}}]}}]})).unwrap();
    let last = mock.model().await;
    assert!(last.body.get("tools").is_none());
    let messages = last.body["messages"].as_array().unwrap();
    let assistant = &messages[messages.len() - 2];
    let result = &messages[messages.len() - 1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["tool_calls"][0]["id"], "clock-call-123");
    assert_eq!(result["role"], "tool");
    assert_eq!(result["tool_call_id"], "clock-call-123");
    let content: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(content["status"], "succeeded");
    last.reply.send(answer("time-read-successfully")).unwrap();
    assert_eq!(mock.sent().await, "time-read-successfully");
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn four_tool_rounds_preserve_protocol_and_end_at_the_smaller_http_budget() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.limits.max_model_calls = 8;
        config.limits.max_tool_calls = 4;
        config.limits.max_http_attempts = 5;
        config.plugins.ai.defaults.allowed_tools = vec!["clock_now".into()];
        config.plugins.ai.defaults.input_budget = 20_000;
    })
    .await;
    router
        .dispatch(message(1, "exercise-four-tool-rounds"))
        .await
        .unwrap();

    let mut operation_id = None;
    for round in 0..=4 {
        let request = mock.model().await;
        let messages = request.body["messages"].as_array().unwrap();
        let pairs = messages
            .windows(2)
            .filter(|pair| pair[0].get("tool_calls").is_some())
            .collect::<Vec<_>>();
        assert_eq!(pairs.len(), round, "tool history was dropped or duplicated");
        for (index, pair) in pairs.iter().enumerate() {
            let expected = format!("chain-clock-{index}");
            assert_eq!(pair[0]["role"], "assistant");
            assert_eq!(pair[0]["tool_calls"][0]["id"], expected);
            assert_eq!(pair[1]["role"], "tool");
            assert_eq!(pair[1]["tool_call_id"], expected);
            let result: Value = serde_json::from_str(pair[1]["content"].as_str().unwrap()).unwrap();
            assert_eq!(result["status"], "succeeded");
            assert!(result["result"]["time"].as_str().is_some());
            let current_operation = result["operation_id"].as_str().unwrap().to_owned();
            if let Some(previous) = &operation_id {
                assert_eq!(previous, &current_operation);
            } else {
                operation_id = Some(current_operation);
            }
        }

        if round == 4 {
            // The HTTP cap, not the larger model-call cap, determines the last
            // request; it must leave a text-only round to finish the answer.
            assert!(request.body.get("tools").is_none());
            assert!(request.body.get("tool_choice").is_none());
            request
                .reply
                .send(answer("four-tool-rounds-complete"))
                .unwrap();
        } else {
            assert_eq!(request.body["tools"].as_array().unwrap().len(), 1);
            assert_eq!(request.body["tools"][0]["function"]["name"], "clock_now");
            assert_eq!(request.body["parallel_tool_calls"], false);
            request
                .reply
                .send(json!({"model":"mock","choices":[{
                    "finish_reason":"tool_calls",
                    "message":{"role":"assistant","content":null,"tool_calls":[{
                        "id":format!("chain-clock-{round}"),"type":"function",
                        "function":{"name":"clock_now","arguments":"{}"}
                    }]}
                }]}))
                .unwrap();
        }
    }
    assert_eq!(mock.sent().await, "four-tool-rounds-complete");
    let runs: i64 = host
        .store
        .call(|connection| {
            Ok(connection.query_row(
                "SELECT count(*) FROM tool_runs WHERE tool_name='clock_now' AND status='succeeded'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    // New provider call IDs must not turn the same operation into repeated work.
    assert_eq!(runs, 1);
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

async fn web_change(host: &Host, router: &Router, action: qkbot::admin::memory::Action) {
    use qkbot::admin::memory::{Actor, MemoryAdmin, MutationRequest};
    let conv = qkbot::plugins::ai::conversation(host, &message(0, "lookup"))
        .await
        .unwrap();
    let service = MemoryAdmin::new(host.store.clone(), host.config.plugins.ai.clone());
    let actor = Actor {
        admin_id: "test-admin".into(),
        session_id: "test-session".into(),
    };
    let current = service
        .conversation(&actor, &conv.guard.conversation_id)
        .await
        .unwrap();
    service
        .mutate(
            &actor,
            MutationRequest {
                request_id: uuid::Uuid::new_v4().to_string(),
                conversation_id: conv.guard.conversation_id,
                epoch: current["epoch"].as_i64().unwrap(),
                memory_revision: current["memory_revision"].as_i64().unwrap(),
                action,
            },
        )
        .await
        .unwrap();
    router.chat.cancel_scope(&message(0, "").key.scope);
}

#[tokio::test]
async fn removed_management_commands_are_silent_and_never_stored_or_learned() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, true).await;
    for (i, text) in [
        "/ai",
        "/ai memory remember secret",
        "/ai new",
        "/bot status",
        "/bot plugins",
    ]
    .iter()
    .enumerate()
    {
        router.dispatch(message(i as i64 + 1, text)).await.unwrap();
    }
    let counts: (i64, i64) = host
        .store
        .call(|db| {
            Ok((
                db.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))?,
                db.query_row("SELECT count(*) FROM ai_conversations", [], |r| r.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (0, 0));
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    within(router.shutdown()).await.unwrap();
    drop(host);
    mock.stop().await;
}
