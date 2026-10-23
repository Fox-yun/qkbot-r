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
    embeddings: mpsc::Receiver<ModelRequest>,
    sends: mpsc::Receiver<Value>,
    quotes: mpsc::Receiver<Value>,
    quote_requests: mpsc::Receiver<ModelRequest>,
    resources: mpsc::Receiver<ModelRequest>,
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
        let (embedding_tx, embeddings) = mpsc::channel(32);
        let (send_tx, sends) = mpsc::channel(32);
        let (quote_tx, quotes) = mpsc::channel(32);
        let (quote_request_tx, quote_requests) = mpsc::channel(32);
        let (resource_tx, resources) = mpsc::channel(32);
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
                let embedding_tx = embedding_tx.clone();
                let send_tx = send_tx.clone();
                let quote_tx = quote_tx.clone();
                let quote_request_tx = quote_request_tx.clone();
                let resource_tx = resource_tx.clone();
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
                            "/api/get_resource_temp_url" => {
                                let (reply, receive) = oneshot::channel();
                                if resource_tx.send(ModelRequest { body, reply }).await.is_err() { return; }
                                let Ok(response) = receive.await else { return; }; response
                            }
                            "/v1/chat/completions" => {
                                let (reply, receive) = oneshot::channel();
                                if model_tx.send(ModelRequest { body, reply }).await.is_err() { return; }
                                let Ok(response) = receive.await else { return; }; response
                            }
                            "/v1/embeddings" => {
                                let (reply, receive) = oneshot::channel();
                                if embedding_tx.send(ModelRequest { body, reply }).await.is_err() { return; }
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
            embeddings,
            sends,
            quotes,
            quote_requests,
            resources,
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

fn ordinary_message(seq: i64, text: &str) -> Incoming {
    let mut incoming = message(seq, text);
    incoming.mentions.clear();
    incoming
}

async fn set_auto_reply(host: &Host, router: &Router, scope: &Scope, enabled: bool) {
    host.access
        .set_auto_reply_enabled(scope.clone(), enabled)
        .await
        .unwrap();
    router.chat.cancel_scope(scope);
}

fn recall_message(seq: i64) -> Incoming {
    let mut incoming = ordinary_message(-seq, "");
    incoming.quote_seq = Some(seq);
    incoming.notification = Some(qkbot::types::NudgeEvent {
        event_type: "message_recall".into(),
        initiator: incoming.sender_id,
        target: incoming.sender_id,
        raw_event: Value::Null,
    });
    incoming
}

async fn add_learning_note(host: &Host, text: &str, scope: &str) -> i64 {
    use qkbot::plugins::ai::learning;
    learning::configure(&host.store, 123, true).await.unwrap();
    let snapshot = learning::snapshot(&host.store, 123).await.unwrap();
    learning::mutate(&host.store, 123, json!({
        "revision":snapshot["revision"], "op":"add",
        "request_id":uuid::Uuid::new_v4().to_string(), "admin_id":"test-admin", "session_id":"test-session",
        "kind":"experience", "text":text, "scope":scope, "locked":true,
        "_max_note_chars":2000,
    })).await.unwrap()["id"].as_i64().unwrap()
}

#[tokio::test]
async fn learning_notes_are_optional_group_data_separate_from_private_and_personal_memory() {
    for (enabled, read) in [(true, true), (false, true), (true, false)] {
        let mut mock = MockServer::start().await;
        let (_temp, host, router) = setup_with(&mock, true, |config| {
            config.plugins.ai.learning.enabled = enabled;
            config.plugins.ai.defaults.memory_read = read;
            config.plugins.ai.defaults.summary = false;
        })
        .await;
        add_learning_note(
            &host,
            "shared-learning-marker：解释错误先说明现象。",
            "shared",
        )
        .await;
        add_learning_note(
            &host,
            "scoped-learning-marker：本群术语应结合上下文。",
            "123:group:456",
        )
        .await;
        for (index, scene, peer) in [
            (1, Scene::Group, 456),
            (2, Scene::Group, 654),
            (3, Scene::Friend, 789),
        ] {
            let mut incoming = message(index, "请回答当前问题");
            incoming.key.scope.scene = scene;
            incoming.key.scope.peer_id = peer;
            router.dispatch(incoming).await.unwrap();
            let request = mock.model().await;
            let messages = request.body["messages"].as_array().unwrap();
            assert!(
                !messages[0]["content"]
                    .as_str()
                    .unwrap()
                    .contains("learning-marker"),
                "notes must never become system policy"
            );
            let prompt = request.body["messages"].to_string();
            let expected = enabled && read && scene == Scene::Group;
            assert_eq!(prompt.contains("shared-learning-marker"), expected);
            assert_eq!(
                prompt.contains("scoped-learning-marker"),
                expected && peer == 456
            );
            assert!(prompt.contains("请回答当前问题"));
            request.reply.send(answer("当前问题已处理")).unwrap();
            assert_eq!(mock.sent().await, "当前问题已处理");
        }
        within(router.shutdown()).await.unwrap();
        assert!(mock.models.try_recv().is_err());
        drop(host);
        mock.stop().await;
    }
}

#[tokio::test]
async fn learning_note_changes_cancel_both_text_and_structured_stale_answers() {
    use qkbot::plugins::ai::learning;
    for structured in [false, true] {
        let mut mock = MockServer::start().await;
        let (_temp, host, router) = setup_with(&mock, true, |config| {
            config.plugins.ai.learning.enabled = true;
            config.plugins.ai.defaults.allowed_tools = vec!["reply_message".into()];
        })
        .await;
        let id = add_learning_note(&host, "old-learning-marker", "shared").await;
        router.dispatch(message(1, "第一问")).await.unwrap();
        let stale = mock.model().await;
        assert!(
            stale.body["messages"]
                .to_string()
                .contains("old-learning-marker")
        );
        let snapshot = learning::snapshot(&host.store, 123).await.unwrap();
        let note = snapshot["notes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|note| note["id"] == id)
            .unwrap();
        learning::mutate(&host.store, 123, json!({
            "revision":snapshot["revision"], "op":"update", "id":id, "version":note["version"],
            "request_id":"edit-note", "admin_id":"test-admin", "session_id":"test-session",
            "kind":"experience", "text":"new-learning-marker", "scope":"shared", "_max_note_chars":2000,
        })).await.unwrap();
        let response = if structured {
            json!({"model":"mock","choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[{"id":"old-reply","type":"function","function":{"name":"reply_message","arguments":json!({"text":"旧笔记生成的回复不得发送"}).to_string()}}]}}]})
        } else {
            answer("旧笔记生成的回复不得发送")
        };
        stale.reply.send(response).unwrap();
        router.dispatch(message(2, "第二问")).await.unwrap();
        let fresh = mock.model().await;
        assert!(
            fresh.body["messages"]
                .to_string()
                .contains("new-learning-marker")
        );
        assert!(
            !fresh.body["messages"]
                .to_string()
                .contains("old-learning-marker")
        );
        fresh.reply.send(answer("新笔记回复")).unwrap();
        assert_eq!(mock.sent().await, "新笔记回复");
        within(router.shutdown()).await.unwrap();
        assert!(mock.sends.try_recv().is_err());
        drop(host);
        mock.stop().await;
    }
}

#[tokio::test]
async fn learning_has_an_extra_twelve_thousand_budget_without_evicting_the_current_question() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.learning.enabled = true;
        config.plugins.ai.defaults.allowed_tools.clear();
        config.plugins.ai.defaults.input_budget = 6000;
    })
    .await;
    assert_eq!(host.config.plugins.ai.learning.injection_budget, 12_000);
    let note = format!("independent-learning-marker:{}", "经".repeat(1800));
    add_learning_note(&host, &note, "shared").await;
    let question = format!("current-question-marker:{}", "question ".repeat(80));
    router.dispatch(message(1, &question)).await.unwrap();
    let request = mock.model().await;
    let serialized = request.body["messages"].to_string();
    assert!(serialized.contains(&note));
    assert!(serialized.contains(&question));
    assert!(
        serialized.len() > 6000,
        "the extra note must not be squeezed into the old allowance"
    );
    assert!(serialized.len() + 72 <= 6000 + 12_000);
    request.reply.send(answer("两份预算均生效")).unwrap();
    assert_eq!(mock.sent().await, "两份预算均生效");
    within(router.shutdown()).await.unwrap();
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn learning_background_uses_its_quota_and_recall_invalidates_cross_group_reply_and_history() {
    use qkbot::plugins::ai::learning;
    for forget_personal_memory in [false, true] {
        let mut mock = MockServer::start().await;
        let (_temp, host, router) = setup_with(&mock, true, |config| {
            config.plugins.ai.learning.enabled = true;
            config.plugins.ai.learn_all_group_messages = true;
            config.plugins.ai.defaults.summary = false;
            config.plugins.ai.defaults.allowed_tools.clear();
        })
        .await;
        // This background-only source must be eligible without entering chat history.
        router
            .dispatch(ordinary_message(
                1,
                "learning-evidence-marker：检查报错时先确认输入类型。",
            ))
            .await
            .unwrap();
        let worker_host = host.clone();
        let worker =
            tokio::spawn(
                async move { learning::run(&worker_host, 123, &worker_host.cancel).await },
            );
        let request = mock.model().await;
        assert!(
            request
                .body
                .get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
        );
        let input: Value = serde_json::from_str(
            request.body["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let source = &input["messages"][0]["source_key"];
        request.reply.send(answer(&json!({"changes":[{"op":"add","kind":"experience","text":"learned-note-marker：先核对输入类型。","scope":"shared","evidence":[{"source_key":source,"quote":"检查报错时先确认输入类型"}]}]}).to_string())).unwrap();
        within(worker).await.unwrap().unwrap();
        let snapshot = learning::snapshot(&host.store, 123).await.unwrap();
        assert_eq!(snapshot["notes"].as_array().unwrap().len(), 1);
        let budget = qkbot::budget::Budget::new(host.store.clone(), host.config.limits.clone())
            .snapshot()
            .await
            .unwrap();
        assert_eq!(budget["used"]["learning"], 1);
        assert_eq!(budget["used"]["foreground"], 0);
        assert_eq!(budget["used"]["background"], 0);
        let mut question = message(1, "跨群应用经验");
        question.key.scope.peer_id = 654;
        router.dispatch(question.clone()).await.unwrap();
        let stale = mock.model().await;
        assert!(
            stale.body["messages"]
                .to_string()
                .contains("learned-note-marker")
        );
        if forget_personal_memory {
            let source_message =
                ordinary_message(1, "learning-evidence-marker：检查报错时先确认输入类型。");
            let conversation = qkbot::plugins::ai::conversation(&host, &source_message)
                .await
                .unwrap();
            let id = host
                .memory
                .remember(
                    &conversation.guard,
                    789,
                    &source_message.key.key(),
                    "检查报错时先确认输入类型",
                    qkbot::plugins::ai::memory::MemoryKind::Fact,
                    None,
                )
                .await
                .unwrap();
            let returned = host
                .memory
                .forget(&conversation.guard, 789, id)
                .await
                .unwrap();
            assert!(
                host.valid(&returned).await.unwrap(),
                "forget must return the actual epoch after learning invalidation"
            );
        } else {
            router.dispatch(recall_message(1)).await.unwrap();
        }
        stale
            .reply
            .send(answer("撤回来源的旧回复不得发送"))
            .unwrap();
        let snapshot = learning::snapshot(&host.store, 123).await.unwrap();
        assert!(!snapshot.to_string().contains("learned-note-marker"));
        assert!(
            snapshot["history"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["rollback_allowed"] == false)
        );
        question.key.message_seq = 2;
        question.text = "撤回后的问题".into();
        router.dispatch(question).await.unwrap();
        let fresh = mock.model().await;
        assert!(
            !fresh.body["messages"]
                .to_string()
                .contains("learned-note-marker")
        );
        fresh.reply.send(answer("撤回后的正常回答")).unwrap();
        assert_eq!(mock.sent().await, "撤回后的正常回答");
        within(router.shutdown()).await.unwrap();
        assert!(mock.sends.try_recv().is_err());
        drop(host);
        mock.stop().await;
    }
}

#[tokio::test]
async fn auto_reply_custom_policy_and_zero_cooldown_take_effect_in_real_turns() {
    let mut mock = MockServer::start().await;
    let policy = "只在讨论天文时主动接话；参与时先分享一条相关知识。marker-custom-policy";
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.auto_reply.prompt = policy.into();
        config.plugins.ai.auto_reply.cooldown_seconds = 0;
        config.plugins.ai.auto_reply.debounce_ms = 100;
        config.plugins.ai.auto_reply.max_wait_ms = 200;
    })
    .await;
    set_auto_reply(&host, &router, &message(0, "").key.scope, true).await;
    for seq in 1..=2 {
        router
            .dispatch(ordinary_message(seq, &format!("天文话题 {seq}")))
            .await
            .unwrap();
        let request = tokio::time::timeout(Duration::from_secs(1), mock.model())
            .await
            .expect("configured 100ms debounce and zero cooldown must both take effect");
        let system = request.body["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains(policy));
        assert!(system.contains("本轮参与方式：observe"));
        assert!(system.contains("无需参与仅输出<NO_REPLY>"));
        assert!(
            !system.contains("可接续与你的对话或合适闲聊"),
            "the custom policy replaces the default policy"
        );
        request.reply.send(answer("天文回复")).unwrap();
        assert_eq!(mock.sent().await, "天文回复");
        wait_for_outbox(&host, "confirmed", seq).await;
    }
    router
        .dispatch(message(3, "明确提问仍然优先"))
        .await
        .unwrap();
    let explicit = mock.model().await;
    let system = explicit.body["messages"][0]["content"].as_str().unwrap();
    assert!(!system.contains(policy));
    assert!(!system.contains("本轮参与方式：observe"));
    explicit.reply.send(answer("<NO_REPLY>")).unwrap();
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn long_automatic_policy_does_not_block_explicit_requests() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.auto_reply.prompt = "策".repeat(2000);
    })
    .await;
    set_auto_reply(&host, &router, &message(0, "").key.scope, true).await;
    router
        .dispatch(message(1, "explicit-request-with-long-automatic-policy"))
        .await
        .unwrap();
    let request = mock.model().await;
    assert!(
        !request.body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains(&"策".repeat(2000))
    );
    request.reply.send(answer("explicit-answer")).unwrap();
    assert_eq!(mock.sent().await, "explicit-answer");
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn auto_reply_custom_cooldown_expires_at_its_configured_duration() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.auto_reply.cooldown_seconds = 1;
        config.plugins.ai.auto_reply.debounce_ms = 100;
        config.plugins.ai.auto_reply.max_wait_ms = 200;
    })
    .await;
    set_auto_reply(&host, &router, &message(0, "").key.scope, true).await;
    router
        .dispatch(ordinary_message(1, "first-topic"))
        .await
        .unwrap();
    mock.model()
        .await
        .reply
        .send(answer("first-answer"))
        .unwrap();
    assert_eq!(mock.sent().await, "first-answer");
    wait_for_outbox(&host, "confirmed", 1).await;
    router
        .dispatch(ordinary_message(2, "topic-during-one-second-cooldown"))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), mock.models.recv())
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(1000)).await;
    router
        .dispatch(ordinary_message(3, "topic-after-one-second-cooldown"))
        .await
        .unwrap();
    let request = mock.model().await;
    assert!(
        request.body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("topic-after-one-second-cooldown")
    );
    request.reply.send(answer("<NO_REPLY>")).unwrap();
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn auto_reply_custom_batch_limit_flushes_without_losing_earlier_messages() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.auto_reply.max_messages = 2;
        config.plugins.ai.auto_reply.debounce_ms = 1000;
        config.plugins.ai.auto_reply.max_wait_ms = 2000;
    })
    .await;
    set_auto_reply(&host, &router, &message(0, "").key.scope, true).await;
    for seq in 1..=3 {
        router
            .dispatch(ordinary_message(seq, &format!("fragment-{seq}")))
            .await
            .unwrap();
    }
    let first = tokio::time::timeout(Duration::from_millis(800), mock.model())
        .await
        .expect("the configured two-message batch must flush before its debounce deadline");
    let current = first.body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(current.contains("fragment-1") && current.contains("fragment-2"));
    assert!(!current.contains("fragment-3"));
    first.reply.send(answer("<NO_REPLY>")).unwrap();
    let second = mock.model().await;
    let current = second.body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(current.contains("fragment-3"));
    assert!(!current.contains("fragment-1") && !current.contains("fragment-2"));
    second.reply.send(answer("<NO_REPLY>")).unwrap();
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn auto_reply_custom_maximum_wait_caps_a_continuously_extended_batch() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.auto_reply.debounce_ms = 1000;
        config.plugins.ai.auto_reply.max_wait_ms = 1300;
    })
    .await;
    set_auto_reply(&host, &router, &message(0, "").key.scope, true).await;
    for seq in 1..=3 {
        if seq > 1 {
            tokio::time::sleep(Duration::from_millis(450)).await;
        }
        router
            .dispatch(ordinary_message(seq, &format!("continuing-fragment-{seq}")))
            .await
            .unwrap();
    }
    let request = tokio::time::timeout(Duration::from_millis(800), mock.model())
        .await
        .expect("the 1300ms maximum wait must cap repeated 1000ms debounce extensions");
    let current = request.body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    for seq in 1..=3 {
        assert!(current.contains(&format!("continuing-fragment-{seq}")));
    }
    request.reply.send(answer("<NO_REPLY>")).unwrap();
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn auto_reply_defaults_off_and_opt_in_is_isolated_from_other_groups_and_private_chats() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, true).await;
    let scope = message(0, "").key.scope;
    router
        .dispatch(ordinary_message(1, "default-off-background"))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1800), mock.models.recv())
            .await
            .is_err(),
        "ordinary group messages must not call the model before opt-in"
    );
    set_auto_reply(&host, &router, &scope, true).await;
    let mut other_group = ordinary_message(2, "other-group-still-disabled");
    other_group.key.scope.peer_id = 457;
    router.dispatch(other_group).await.unwrap();

    // The same numeric peer in another scene has its own switch. A private
    // chat without opt-in keeps its existing immediate, direct-reply behavior.
    let mut private = ordinary_message(2, "private-legacy-direct-reply");
    private.key.scope.scene = Scene::Friend;
    router.dispatch(private).await.unwrap();
    let private_request = tokio::time::timeout(Duration::from_secs(1), mock.model())
        .await
        .expect("unconfigured private chats must not acquire the automatic delay");
    assert_eq!(
        current_message_payload(&private_request.body)["text"],
        "private-legacy-direct-reply"
    );
    private_request
        .reply
        .send(answer("private-answer"))
        .unwrap();
    assert_eq!(mock.sent().await, "private-answer");

    router
        .dispatch(ordinary_message(3, "enabled-group-background"))
        .await
        .unwrap();
    let request = mock.model().await;
    let context = request.body["messages"].to_string();
    assert!(context.contains("enabled-group-background"));
    assert!(!context.contains("other-group-still-disabled"));
    assert!(!context.contains("private-legacy-direct-reply"));
    request.reply.send(answer("enabled-group-answer")).unwrap();
    assert_eq!(mock.sent().await, "enabled-group-answer");
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn auto_reply_batches_messages_and_reuses_model_persona_history_and_memory() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.llm.model = "shared-chat-model".into();
        config.plugins.ai.defaults.persona = "test-persona".into();
        config.plugins.ai.personas.insert(
            "test-persona".into(),
            "自动接话也应保留这段专用人设 marker-persona".into(),
        );
    })
    .await;
    let first = message(1, "我喜欢白茶，context-before-auto-reply");
    router.dispatch(first.clone()).await.unwrap();
    mock.model()
        .await
        .reply
        .send(answer("confirmed-answer-before-auto-reply"))
        .unwrap();
    mock.sent().await;
    wait_for_outbox(&host, "confirmed", 1).await;
    let conversation = qkbot::plugins::ai::conversation(&host, &first)
        .await
        .unwrap();
    host.memory
        .remember(
            &conversation.guard,
            first.sender_id,
            &first.key.key(),
            "喜欢白茶",
            qkbot::plugins::ai::memory::MemoryKind::Preference,
            None,
        )
        .await
        .unwrap();
    set_auto_reply(&host, &router, &first.key.scope, true).await;
    let mut other_speaker = ordinary_message(2, "batch-first-fragment");
    other_speaker.sender_id = 888;
    router.dispatch(other_speaker).await.unwrap();
    router
        .dispatch(ordinary_message(3, "batch-second-fragment"))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), mock.models.recv())
            .await
            .is_err(),
        "short adjacent messages should wait for one merged decision"
    );
    let request = mock.model().await;
    assert_eq!(request.body["model"], "shared-chat-model");
    let context = request.body["messages"].to_string();
    for expected in [
        "marker-persona",
        "context-before-auto-reply",
        "confirmed-answer-before-auto-reply",
        "batch-first-fragment",
        "batch-second-fragment",
        "喜欢白茶",
        "关于用户 789",
    ] {
        assert!(
            context.contains(expected),
            "missing shared context: {expected}"
        );
    }
    request.reply.send(answer(" \n<NO_REPLY>\t ")).unwrap();

    // Silence must neither produce an outbox row nor consume the 30-second
    // automatic-send cooldown; the next ordinary message may be considered.
    router
        .dispatch(ordinary_message(4, "a-new-topic-after-silence"))
        .await
        .unwrap();
    let next = mock.model().await;
    assert!(
        next.body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("a-new-topic-after-silence")
    );
    assert!(mock.sends.try_recv().is_err());
    let counts: (i64, i64) = host
        .store
        .call(|db| {
            Ok((
                db.query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))?,
                db.query_row(
                    "SELECT COUNT(*) FROM messages WHERE role='assistant'",
                    [],
                    |row| row.get(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (1, 1));
    assert!(
        !next.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "assistant"
                    && message["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("<NO_REPLY>"))
            })
    );
    next.reply.send(answer("one-new-topic-answer")).unwrap();
    assert_eq!(mock.sent().await, "one-new-topic-answer");
    within(router.shutdown()).await.unwrap();
    assert!(
        mock.models.try_recv().is_err(),
        "a batch must use one model decision"
    );
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn auto_reply_private_opt_in_batches_and_disabling_restores_direct_replies() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, true).await;
    let scope = Scope {
        bot_id: 123,
        scene: Scene::Friend,
        peer_id: 789,
    };
    set_auto_reply(&host, &router, &scope, true).await;
    for (seq, text) in [(1, "private-fragment-one"), (2, "private-fragment-two")] {
        let mut incoming = ordinary_message(seq, text);
        incoming.key.scope = scope.clone();
        router.dispatch(incoming).await.unwrap();
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(300), mock.models.recv())
            .await
            .is_err()
    );
    let request = mock.model().await;
    let context = request.body["messages"].to_string();
    assert!(context.contains("private-fragment-one"));
    assert!(context.contains("private-fragment-two"));
    request.reply.send(answer("private-merged-answer")).unwrap();
    assert_eq!(mock.sent().await, "private-merged-answer");
    wait_for_outbox(&host, "confirmed", 1).await;

    // Disabling is independent of automatic cooldown and preserves history.
    set_auto_reply(&host, &router, &scope, false).await;
    let mut direct = ordinary_message(3, "private-direct-after-disabling");
    direct.key.scope = scope;
    router.dispatch(direct).await.unwrap();
    let request = tokio::time::timeout(Duration::from_secs(1), mock.model())
        .await
        .expect("disabling auto reply must restore the original private trigger");
    let context = request.body["messages"].to_string();
    assert!(context.contains("private-direct-after-disabling"));
    assert!(context.contains("private-merged-answer"));
    request.reply.send(answer("private-direct-answer")).unwrap();
    assert_eq!(mock.sent().await, "private-direct-answer");
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn auto_reply_batch_keeps_earlier_quotes_real_mentions_and_reply_references() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        config.llm.vision = false;
        config.limits.max_model_calls = 1;
    })
    .await;
    set_auto_reply(&host, &router, &message(0, "").key.scope, true).await;
    let mut first = image_message(1, "earlier-message-with-quote-and-mention");
    first.mentions = vec![999];
    first.quote_seq = Some(77);
    router.dispatch(first).await.unwrap();
    router
        .dispatch(ordinary_message(
            2,
            "latest-message-without-quote-or-mention",
        ))
        .await
        .unwrap();
    let request = mock.model().await;
    let content = request.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(content.contains("quoted-group-456-77"));
    assert!(content.contains("识图已关闭"));
    assert!(!content.contains("untrusted-event-preview.png"));
    let first_payload = content
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|payload| payload["text"] == "earlier-message-with-quote-and-mention")
        .expect("the earlier batch message needs its own identity envelope");
    assert_eq!(first_payload["mentions"][0]["user_id"], "999");
    assert!(mock.resources.try_recv().is_err());
    request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "reply-to-earlier-batch-source",
            "reply_message",
            json!({
                "reply_to":first_payload["message_ref"],
                "mention_users":[first_payload["mentions"][0]["user_ref"]],
                "text":"earlier-source-refs-still-authorized"
            }),
        )]))
        .unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    assert_eq!(
        sent["message"],
        json!([
            {"type":"reply","data":{"message_seq":1}},
            {"type":"mention","data":{"user_id":999}},
            {"type":"text","data":{"text":"earlier-source-refs-still-authorized"}}
        ])
    );
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn auto_reply_batch_reads_an_earlier_image_with_its_original_message_key() {
    let mut mock = MockServer::start().await;
    mock.control_quotes.store(true, Ordering::Relaxed);
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.llm.vision = true;
    })
    .await;
    set_auto_reply(&host, &router, &message(0, "").key.scope, true).await;
    let mut first = image_message(1, "earlier-image-message");
    first.mentions.clear();
    router.dispatch(first).await.unwrap();
    router
        .dispatch(ordinary_message(2, "later-text-message"))
        .await
        .unwrap();
    let lookup = within(mock.quote_requests.recv()).await.unwrap();
    assert_eq!(
        lookup.body,
        json!({"message_scene":"group","peer_id":456,"message_seq":1})
    );
    assert!(mock.resources.try_recv().is_err());
    let mut verified = quoted(&lookup.body, 789, "earlier-image-message");
    verified["data"]["message"]["segments"] = json!([
        {"type":"text","data":{"text":"earlier-image-message"}},
        {"type":"image","data":{"resource_id":"pic-one"}}
    ]);
    lookup.reply.send(verified).unwrap();
    let resource = within(mock.resources.recv()).await.unwrap();
    assert_eq!(resource.body, json!({"resource_id":"pic-one"}));
    resource
        .reply
        .send(json!({"status":"ok","retcode":0,"data":{
            "url":"https://127.0.0.1/unreadable-batch-image.png"
        }}))
        .unwrap();
    let request = mock.model().await;
    let context = request.body["messages"].to_string();
    assert!(context.contains("earlier-image-message"));
    assert!(context.contains("later-text-message"));
    assert!(context.contains("新增消息 1") && context.contains("未能读取"));
    assert!(!context.contains("untrusted-event-preview.png"));
    assert!(!context.contains("unreadable-batch-image.png"));
    request
        .reply
        .send(answer("batch-image-fallback-answer"))
        .unwrap();
    assert_eq!(mock.sent().await, "batch-image-fallback-answer");
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.quote_requests.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn auto_reply_cooldown_does_not_delay_mentions_keywords_or_verified_replies() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, true).await;
    set_auto_reply(&host, &router, &message(0, "").key.scope, true).await;
    router
        .dispatch(ordinary_message(1, "first-automatic-topic"))
        .await
        .unwrap();
    mock.model()
        .await
        .reply
        .send(answer("first-automatic-answer"))
        .unwrap();
    assert_eq!(mock.sent().await, "first-automatic-answer");
    wait_for_outbox(&host, "confirmed", 1).await;
    router
        .dispatch(ordinary_message(2, "automatic-topic-during-cooldown"))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1800), mock.models.recv())
            .await
            .is_err(),
        "the same chat must not start another automatic turn during cooldown"
    );
    for incoming in [
        message(3, "explicit-mention-during-cooldown"),
        ordinary_message(4, "小鱼 explicit-keyword-during-cooldown"),
        reply_message(5, 100000, "explicit-quote-during-cooldown"),
    ] {
        let text = incoming.text.clone();
        router.dispatch(incoming).await.unwrap();
        let request = tokio::time::timeout(Duration::from_secs(1), mock.model())
            .await
            .expect("explicit triggers must bypass automatic cooldown and merging");
        let current = request.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|message| message["role"] == "user")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        // Explicit replies append quote context after the current envelope.
        let envelope: Value = serde_json::from_str(current.lines().next().unwrap()).unwrap();
        assert_eq!(envelope["text"], text);
        request.reply.send(answer("explicit-answer")).unwrap();
        assert_eq!(mock.sent().await, "explicit-answer");
    }
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn auto_reply_pending_batch_yields_to_an_explicit_message_without_duplicate_reply() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, true).await;
    set_auto_reply(&host, &router, &message(0, "").key.scope, true).await;
    router
        .dispatch(ordinary_message(1, "pending-background-fragment"))
        .await
        .unwrap();
    router
        .dispatch(message(2, "explicit-message-before-batch-deadline"))
        .await
        .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(1), mock.model())
        .await
        .expect("a pending automatic batch must not hold an explicit request");
    let context = request.body["messages"].to_string();
    assert!(context.contains("pending-background-fragment"));
    assert_eq!(
        current_message_payload(&request.body)["text"],
        "explicit-message-before-batch-deadline"
    );
    request
        .reply
        .send(answer("single-explicit-answer"))
        .unwrap();
    assert_eq!(mock.sent().await, "single-explicit-answer");
    assert!(
        tokio::time::timeout(Duration::from_millis(1800), mock.models.recv())
            .await
            .is_err(),
        "the superseded batch must not create a duplicate automatic turn"
    );
    within(router.shutdown()).await.unwrap();
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn auto_reply_pending_batches_are_cancelled_by_disable_reset_and_recall() {
    for change in ["disable", "reset", "recall"] {
        let mut mock = MockServer::start().await;
        let (_temp, host, router) = setup(&mock, true).await;
        let scope = message(0, "").key.scope;
        set_auto_reply(&host, &router, &scope, true).await;
        router
            .dispatch(ordinary_message(1, "cancelled-before-automatic-deadline"))
            .await
            .unwrap();
        match change {
            "disable" => set_auto_reply(&host, &router, &scope, false).await,
            "reset" => web_change(&host, &router, qkbot::admin::memory::Action::Reset).await,
            "recall" => router.dispatch(recall_message(1)).await.unwrap(),
            _ => unreachable!(),
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(1800), mock.models.recv())
                .await
                .is_err(),
            "{change} must cancel an unsubmitted automatic batch"
        );
        assert!(mock.sends.try_recv().is_err(), "{change}");
        router
            .dispatch(message(2, "new-explicit-request"))
            .await
            .unwrap();
        let request = mock.model().await;
        assert_eq!(
            current_message_payload(&request.body)["text"],
            "new-explicit-request"
        );
        request.reply.send(answer("new-explicit-answer")).unwrap();
        assert_eq!(mock.sent().await, "new-explicit-answer");
        within(router.shutdown()).await.unwrap();
        assert!(mock.models.try_recv().is_err(), "{change}");
        assert!(mock.sends.try_recv().is_err(), "{change}");
        drop(host);
        mock.stop().await;
    }
}

#[tokio::test]
async fn auto_reply_inflight_answers_are_cancelled_by_disable_reset_recall_and_account_change() {
    for change in ["disable", "reset", "recall", "account"] {
        let mut mock = MockServer::start().await;
        let (_temp, host, router) = setup(&mock, true).await;
        let scope = message(0, "").key.scope;
        set_auto_reply(&host, &router, &scope, true).await;
        router
            .dispatch(ordinary_message(1, "obsolete-automatic-question"))
            .await
            .unwrap();
        let old = mock.model().await;
        match change {
            "disable" => set_auto_reply(&host, &router, &scope, false).await,
            "reset" => web_change(&host, &router, qkbot::admin::memory::Action::Reset).await,
            "recall" => router.dispatch(recall_message(1)).await.unwrap(),
            "account" => {
                mock.login_id.store(999, Ordering::Relaxed);
                assert!(host.milky.login().await.is_err());
                mock.login_id.store(123, Ordering::Relaxed);
                host.milky.login().await.unwrap();
            }
            _ => unreachable!(),
        }
        let _ = old
            .reply
            .send(answer("obsolete-automatic-answer-must-not-send"));
        // Another chat's explicit request is an ordering barrier under the
        // single global model slot. It cannot itself cancel the obsolete turn.
        let mut fresh = message(2, "fresh-explicit-question");
        fresh.key.scope.peer_id = 457;
        router.dispatch(fresh).await.unwrap();
        let request = mock.model().await;
        assert_eq!(
            current_message_payload(&request.body)["text"],
            "fresh-explicit-question"
        );
        assert!(
            mock.sends.try_recv().is_err(),
            "{change} must suppress the stale answer"
        );
        request.reply.send(answer("fresh-explicit-answer")).unwrap();
        assert_eq!(mock.sent().await, "fresh-explicit-answer", "{change}");
        within(router.shutdown()).await.unwrap();
        assert!(mock.models.try_recv().is_err(), "{change}");
        assert!(mock.sends.try_recv().is_err(), "{change}");
        drop(host);
        mock.stop().await;
    }
}

#[tokio::test]
async fn auto_reply_rechecks_every_batch_source_at_the_final_submission_boundary() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, true).await;
    set_auto_reply(&host, &router, &message(0, "").key.scope, true).await;
    for (seq, text) in [
        (1, "earlier-source-to-revoke"),
        (2, "current-source-still-valid"),
    ] {
        router.dispatch(ordinary_message(seq, text)).await.unwrap();
    }
    let request = mock.model().await;
    host.store
        .call(|db| {
            // This is the same deterministic final-boundary race fixture used
            // for reply and sticker provenance elsewhere in this test suite.
            db.execute_batch(
                "CREATE TRIGGER revoke_earlier_auto_source_after_prepare AFTER INSERT ON outbox
                 WHEN NEW.origin_plugin='ai' AND NEW.state='prepared'
                 BEGIN
                   INSERT INTO ai_revoked_sources(source_key,revoked_at)
                     VALUES('123:group:456:1',1);
                 END;",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    request
        .reply
        .send(answer("must-not-submit-from-partly-revoked-batch"))
        .unwrap();
    wait_for_outbox(&host, "failed", 1).await;
    assert!(mock.sends.try_recv().is_err());
    let assistants: i64 = host
        .store
        .call(|db| {
            Ok(db.query_row(
                "SELECT COUNT(*) FROM messages WHERE role='assistant'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(assistants, 0);
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn auto_reply_allows_reads_and_replies_but_rejects_local_writes_without_explicit_trigger() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        config
            .plugins
            .ai
            .defaults
            .allowed_tools
            .extend(["reminder_list".into(), "reminder_cancel".into()]);
    })
    .await;
    set_auto_reply(&host, &router, &message(0, "").key.scope, true).await;
    let mut earlier = ordinary_message(1, "earlier-speaker-mentions-a-possible-reminder");
    earlier.sender_id = 888;
    router.dispatch(earlier).await.unwrap();
    router
        .dispatch(ordinary_message(2, "another-speaker-is-only-chatting"))
        .await
        .unwrap();
    let automatic = mock.model().await;
    let definitions = automatic.body["tools"].as_array().unwrap();
    for allowed in ["clock_now", "reminder_list", "reply_message"] {
        assert!(
            definitions
                .iter()
                .any(|tool| tool["function"]["name"] == allowed),
            "automatic replies should retain {allowed}"
        );
    }
    for forbidden in ["reminder_create", "reminder_cancel"] {
        assert!(
            !definitions
                .iter()
                .any(|tool| tool["function"]["name"] == forbidden),
            "automatic observation must not advertise {forbidden}"
        );
    }
    // A provider can still invent a hidden tool call. Neither member's casual
    // speech may become authorization to create a reminder for the last sender.
    automatic
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "forged-automatic-write",
            "reminder_create",
            json!({"after_seconds":600,"text":"must-not-create-from-observation"}),
        )]))
        .unwrap();
    // The provider validator rejects unoffered tools before the chat loop;
    // automatic failures end silently instead of requesting a correction.
    // An explicit request in another chat proves the shared slot was released
    // without superseding or cancelling the automatic turn under test.
    let mut barrier = message(1, "separate-chat-completion-barrier");
    barrier.key.scope.peer_id = 457;
    router.dispatch(barrier).await.unwrap();
    let completed = mock.model().await;
    assert_eq!(
        current_message_payload(&completed.body)["text"],
        "separate-chat-completion-barrier"
    );
    let counts: (i64, i64) = host
        .store
        .call(|db| {
            Ok((
                db.query_row("SELECT COUNT(*) FROM reminder_jobs", [], |row| row.get(0))?,
                db.query_row("SELECT COUNT(*) FROM tool_runs", [], |row| row.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (0, 0));
    assert!(mock.sends.try_recv().is_err());
    completed.reply.send(answer("<NO_REPLY>")).unwrap();
    router
        .dispatch(ordinary_message(3, "new-observation-may-read-the-clock"))
        .await
        .unwrap();
    let reading = mock.model().await;
    assert!(
        reading.body["messages"]
            .to_string()
            .contains("new-observation-may-read-the-clock")
    );
    reading
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "automatic-read-clock",
            "clock_now",
            json!({}),
        )]))
        .unwrap();
    let final_reply = mock.model().await;
    assert_eq!(
        last_model_tool_result(&final_reply.body, "automatic-read-clock")["status"],
        "succeeded"
    );
    final_reply
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "automatic-final-reply",
            "reply_message",
            json!({"text":"read-and-reply-still-work"}),
        )]))
        .unwrap();
    assert_eq!(mock.sent().await, "read-and-reply-still-work");

    router
        .dispatch(message(
            4,
            "explicit-request-keeps-the-original-tool-policy",
        ))
        .await
        .unwrap();
    let explicit = mock.model().await;
    let definitions = explicit.body["tools"].as_array().unwrap();
    for allowed in [
        "clock_now",
        "reminder_list",
        "reply_message",
        "reminder_create",
        "reminder_cancel",
    ] {
        assert!(
            definitions
                .iter()
                .any(|tool| tool["function"]["name"] == allowed),
            "an explicit request in the opted-in chat should still offer {allowed}"
        );
    }
    explicit.reply.send(answer("<NO_REPLY>")).unwrap();
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
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
    // Cached quote text still needs one on-demand media snapshot lookup.
    assert_eq!(
        within(mock.quotes.recv()).await.unwrap(),
        json!({"message_scene":"group","peer_id":456,"message_seq":1})
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
async fn no_reply_uses_history_without_sending_and_keeps_later_questions_runnable() {
    for tool_calling in [false, true] {
        let mut mock = MockServer::start().await;
        let (_temp, host, router) = setup_with(&mock, true, |config| {
            config.llm.tool_calling = tool_calling;
        })
        .await;
        router
            .dispatch(message(1, "previous-question"))
            .await
            .unwrap();
        mock.model()
            .await
            .reply
            .send(answer("previous-confirmed-answer"))
            .unwrap();
        assert_eq!(mock.sent().await, "previous-confirmed-answer");
        wait_for_outbox(&host, "confirmed", 1).await;

        router.dispatch(message(2, "重复收到，谢谢")).await.unwrap();
        let repeated = mock.model().await;
        let history = repeated.body["messages"].as_array().unwrap();
        assert!(history.iter().any(|message| {
            message["role"] == "assistant" && message["content"] == "previous-confirmed-answer"
        }));
        assert_eq!(
            current_message_payload(&repeated.body)["text"],
            "重复收到，谢谢"
        );
        router
            .dispatch(message(3, "new-independent-question"))
            .await
            .unwrap();
        repeated.reply.send(answer(" \n<NO_REPLY>\t ")).unwrap();

        // A new model request proves the silent turn finished and released the lane.
        let next = mock.model().await;
        assert_eq!(
            current_message_payload(&next.body)["text"],
            "new-independent-question"
        );
        assert!(
            mock.sends.try_recv().is_err(),
            "silence must not send a marker or error receipt"
        );
        let counts: (i64, i64, i64) = host
            .store
            .call(|db| {
                Ok((
                    db.query_row(
                        "SELECT COUNT(*) FROM messages WHERE role='assistant'",
                        [],
                        |row| row.get(0),
                    )?,
                    db.query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))?,
                    db.query_row("SELECT COUNT(*) FROM tool_runs", [], |row| row.get(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(
            counts,
            (1, 1, 0),
            "a silent turn must leave no outbound or tool records"
        );
        assert!(
            !next.body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| {
                    message["role"] == "assistant"
                        && message["content"]
                            .as_str()
                            .is_some_and(|text| text.contains("<NO_REPLY>"))
                })
        );

        let literal = "正文中的 <NO_REPLY> 是示例，不是独立的静默标记。";
        next.reply.send(answer(literal)).unwrap();
        assert_eq!(mock.sent().await, literal);
        wait_for_outbox(&host, "confirmed", 2).await;
        within(router.shutdown()).await.unwrap();
        assert!(mock.models.try_recv().is_err());
        assert!(mock.sends.try_recv().is_err());
        drop(host);
        mock.stop().await;
    }
}

#[tokio::test]
async fn no_reply_discards_attached_tool_calls_before_any_side_effect() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |_| {}).await;
    router.dispatch(message(1, "机器人重复接话")).await.unwrap();
    let request = mock.model().await;
    for name in ["clock_now", "reminder_create", "reply_message"] {
        assert!(
            request.body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["function"]["name"] == name)
        );
    }
    let mut response = model_tool_response(vec![
        model_tool_call("silent-clock", "clock_now", json!({})),
        model_tool_call(
            "silent-reminder",
            "reminder_create",
            json!({"after_seconds":60,"text":"静默时不能创建提醒"}),
        ),
        model_tool_call(
            "silent-send",
            "reply_message",
            json!({"text":"静默时不能发送"}),
        ),
    ]);
    response["choices"][0]["message"]["content"] = json!("<NO_REPLY>");
    router.dispatch(message(2, "新的实际问题")).await.unwrap();
    request.reply.send(response).unwrap();

    let next = mock.model().await;
    assert_eq!(current_message_payload(&next.body)["text"], "新的实际问题");
    assert!(
        !next.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool")
    );
    assert!(mock.sends.try_recv().is_err());
    let counts: (i64, i64, i64, i64) = host
        .store
        .call(|db| {
            Ok((
                db.query_row("SELECT COUNT(*) FROM tool_runs", [], |row| row.get(0))?,
                db.query_row("SELECT COUNT(*) FROM reminder_jobs", [], |row| row.get(0))?,
                db.query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))?,
                db.query_row(
                    "SELECT COUNT(*) FROM messages WHERE role='assistant'",
                    [],
                    |row| row.get(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (0, 0, 0, 0));
    next.reply.send(answer("新的问题照常回答")).unwrap();
    assert_eq!(mock.sent().await, "新的问题照常回答");
    wait_for_outbox(&host, "confirmed", 1).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn no_reply_in_final_reply_tool_suppresses_quote_and_mentions() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        config.limits.max_model_calls = 1;
    })
    .await;
    router.dispatch(message(1, "不需要再接话")).await.unwrap();
    let request = mock.model().await;
    let current = current_message_payload(&request.body);
    router
        .dispatch(message(2, "需要正常回答的新问题"))
        .await
        .unwrap();
    request.reply.send(model_tool_response(vec![model_tool_call(
        "silent-final-reply", "reply_message",
        json!({"text":"<NO_REPLY>","reply_to":current["message_ref"],"mention_users":[current["sender_ref"]]}),
    )])).unwrap();
    let next = mock.model().await;
    assert_eq!(
        current_message_payload(&next.body)["text"],
        "需要正常回答的新问题"
    );
    assert!(
        mock.sends.try_recv().is_err(),
        "a silent structured reply must not emit a mention or quote"
    );
    let counts: (i64, i64) = host
        .store
        .call(|db| {
            Ok((
                db.query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))?,
                db.query_row(
                    "SELECT COUNT(*) FROM messages WHERE role='assistant'",
                    [],
                    |row| row.get(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (0, 0));
    let literal = "说明中保留 <NO_REPLY> 示例。";
    next.reply
        .send(model_tool_response(vec![model_tool_call(
            "normal-final-reply",
            "reply_message",
            json!({"text":literal}),
        )]))
        .unwrap();
    assert_eq!(mock.sent().await, literal);
    wait_for_outbox(&host, "confirmed", 1).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
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
async fn native_reminder_can_require_owner_confirmation_when_configured() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.reminder.llm_writes_require_confirmation = true;
    })
    .await;
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
async fn native_reminder_create_and_cancel_need_no_confirmation_and_commit_once() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.defaults.allowed_tools = vec![
            "reminder_create".into(),
            "reminder_list".into(),
            "reminder_cancel".into(),
        ];
    })
    .await;
    assert!(!host.config.plugins.reminder.llm_writes_require_confirmation);
    let creation = message(1, "小鱼，提醒我20分钟后吃饭");
    let expected_due = creation.received_at + 20 * 60;
    router.dispatch(creation.clone()).await.unwrap();
    let mut request = mock.model().await;
    for tool in ["reminder_create", "reminder_list", "reminder_cancel"] {
        assert!(
            request.body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| { entry["function"]["name"] == tool })
        );
    }
    let mut created = None;
    // A provider retry with a fresh call ID must reuse the same durable operation.
    for call_id in ["create-first", "create-repeated"] {
        request
            .reply
            .send(model_tool_response(vec![model_tool_call(
                call_id,
                "reminder_create",
                json!({"after_seconds":1200,"text":"吃饭"}),
            )]))
            .unwrap();
        request = mock.model().await;
        let result = last_model_tool_result(&request.body, call_id);
        assert_eq!(result["status"], "succeeded");
        assert_eq!(result["result"]["due_at"], expected_due);
        assert_eq!(result["result"]["text"], "吃饭");
        assert_eq!(result["result"]["status"], "pending");
        assert!(!result.to_string().contains("/bot confirm"));
        if let Some(previous) = &created {
            assert_eq!(&result, previous);
        } else {
            created = Some(result);
        }
        assert!(
            mock.sends.try_recv().is_err(),
            "no confirmation preview is sent"
        );
    }
    assert!(request.body.get("tools").is_none());
    request
        .reply
        .send(answer("好，20分钟后提醒你吃饭。"))
        .unwrap();
    assert_eq!(mock.sent().await, "好，20分钟后提醒你吃饭。");
    wait_for_outbox(&host, "confirmed", 1).await;
    router.dispatch(creation).await.unwrap();

    let cancellation = message(2, "取消刚才吃饭的提醒");
    router.dispatch(cancellation.clone()).await.unwrap();
    mock.model()
        .await
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "list-before-cancel",
            "reminder_list",
            json!({}),
        )]))
        .unwrap();
    let request = mock.model().await;
    let list = last_model_tool_result(&request.body, "list-before-cancel");
    assert_eq!(list["status"], "succeeded");
    let reminders = list["result"]["reminders"].as_array().unwrap();
    assert_eq!(reminders.len(), 1);
    let id = reminders[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(created.unwrap()["result"]["id"], id);
    request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "cancel-listed-reminder",
            "reminder_cancel",
            json!({"reminder_id":id}),
        )]))
        .unwrap();
    let request = mock.model().await;
    let result = last_model_tool_result(&request.body, "cancel-listed-reminder");
    assert_eq!(result["status"], "succeeded");
    assert_eq!(result["result"], json!({"id":id,"status":"cancelled"}));
    assert!(
        request.body.get("tools").is_none(),
        "list and cancel fit the default three model calls"
    );
    assert!(
        mock.sends.try_recv().is_err(),
        "cancellation does not request confirmation"
    );
    request.reply.send(answer("已取消吃饭提醒。")).unwrap();
    assert_eq!(mock.sent().await, "已取消吃饭提醒。");
    wait_for_outbox(&host, "confirmed", 2).await;
    router.dispatch(cancellation).await.unwrap();

    let counts: (i64, i64, i64, i64) = host.store.call(|db| {
        db.execute("UPDATE reminder_jobs SET due_at=?1", [now()-1])?;
        Ok((
            db.query_row("SELECT COUNT(*) FROM reminder_jobs", [], |row| row.get(0))?,
            db.query_row("SELECT COUNT(*) FROM reminder_jobs WHERE status='cancelled'", [], |row| row.get(0))?,
            db.query_row("SELECT COUNT(*) FROM tool_runs WHERE tool_name='reminder_create' AND status='succeeded'", [], |row| row.get(0))?,
            db.query_row("SELECT COUNT(*) FROM tool_runs WHERE status='pending_confirmation' OR token IS NOT NULL", [], |row| row.get(0))?,
        ))
    }).await.unwrap();
    assert_eq!(counts, (1, 1, 1, 0));
    router.reminders().await.unwrap();
    within(router.shutdown()).await.unwrap();
    assert!(
        mock.models.try_recv().is_err(),
        "replayed messages must not run the model again"
    );
    assert!(
        mock.sends.try_recv().is_err(),
        "a cancelled reminder must never be delivered"
    );
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn native_reminder_list_and_cancel_are_scoped_to_the_requesting_user_and_chat() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.defaults.allowed_tools =
            vec!["reminder_list".into(), "reminder_cancel".into()];
    })
    .await;
    for (seq, actor, peer, text) in [
        (1, 789, 456, "本人本群"),
        (2, 999, 456, "其他用户"),
        (3, 789, 654, "另一个群"),
    ] {
        let mut incoming = message(seq, &format!("/remind add 1200 {text}"));
        incoming.sender_id = actor;
        incoming.key.scope.peer_id = peer;
        router.dispatch(incoming).await.unwrap();
        assert!(mock.sent().await.contains("已创建提醒"));
    }
    let ids: Vec<(String, String)> = host
        .store
        .call(|db| {
            let mut query = db.prepare("SELECT id,text FROM reminder_jobs ORDER BY text")?;
            Ok(query
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?)
        })
        .await
        .unwrap();
    let own_id = &ids.iter().find(|(_, text)| text == "本人本群").unwrap().0;
    for (offset, (foreign_id, _)) in ids
        .iter()
        .filter(|(_, text)| text != "本人本群")
        .enumerate()
    {
        router
            .dispatch(message(10 + offset as i64, "取消我的提醒"))
            .await
            .unwrap();
        mock.model()
            .await
            .reply
            .send(model_tool_response(vec![model_tool_call(
                "list-own-reminders",
                "reminder_list",
                json!({}),
            )]))
            .unwrap();
        let request = mock.model().await;
        let result = last_model_tool_result(&request.body, "list-own-reminders");
        let reminders = result["result"]["reminders"].as_array().unwrap();
        assert_eq!(reminders.len(), 1);
        assert_eq!(reminders[0]["id"], *own_id);
        assert_eq!(reminders[0]["text"], "本人本群");
        request
            .reply
            .send(model_tool_response(vec![model_tool_call(
                "foreign-cancel",
                "reminder_cancel",
                json!({"reminder_id":foreign_id}),
            )]))
            .unwrap();
        let request = mock.model().await;
        assert_failed_tool_results(&request.body, &["foreign-cancel"]);
        request
            .reply
            .send(answer("只能取消你在当前聊天中的提醒。"))
            .unwrap();
        assert_eq!(mock.sent().await, "只能取消你在当前聊天中的提醒。");
    }
    let pending: i64 = host
        .store
        .call(|db| {
            Ok(db.query_row(
                "SELECT COUNT(*) FROM reminder_jobs WHERE status='pending'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        pending, 3,
        "cross-user and cross-chat cancellation must not change any reminder"
    );
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
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

const CROSS_CHAT_PREFERENCE: &str = "长期偏好乌龙茶";
const SOURCE_CHAT_ONLY: &str = "仅在来源群短期出现的暗号";

async fn seed_cross_chat_preference(
    host: &Host,
    router: &Router,
    mock: &mut MockServer,
) -> (qkbot::types::ContextGuard, i64) {
    let mut source = message(1, &format!("{CROSS_CHAT_PREFERENCE}；{SOURCE_CHAT_ONLY}"));
    source.sender_id = 7;
    router.dispatch(source.clone()).await.unwrap();
    mock.model()
        .await
        .reply
        .send(answer("已收到来源群消息"))
        .unwrap();
    assert_eq!(mock.sent().await, "已收到来源群消息");
    wait_for_outbox(host, "confirmed", 1).await;
    let conversation = qkbot::plugins::ai::conversation(host, &source)
        .await
        .unwrap();
    let id = host
        .memory
        .remember(
            &conversation.guard,
            7,
            &source.key.key(),
            CROSS_CHAT_PREFERENCE,
            qkbot::plugins::ai::memory::MemoryKind::Preference,
            None,
        )
        .await
        .unwrap();
    (conversation.guard, id)
}

#[tokio::test]
async fn global_memory_shares_same_qq_across_groups_and_private_chat_without_history_or_other_people()
 {
    for enabled in [true, false] {
        let mut mock = MockServer::start().await;
        let (_temp, host, router) = setup_with(&mock, true, |config| {
            // The enabled case deliberately uses the default setting.
            if !enabled {
                config.plugins.ai.global_memory_shared = false;
            }
            config.plugins.ai.defaults.summary = false;
            config.plugins.ai.defaults.memory_write = false;
        })
        .await;
        assert_eq!(host.config.plugins.ai.global_memory_shared, enabled);
        seed_cross_chat_preference(&host, &router, &mut mock).await;

        let mut group = message(1, "我平时喜欢喝什么茶？");
        group.key.scope.peer_id = 654;
        group.sender_id = 7;
        let mut private = message(1, "你记得我的饮茶偏好吗？");
        private.key.scope.scene = Scene::Friend;
        private.key.scope.peer_id = 7;
        private.sender_id = 7;
        private.mentions.clear();
        let mut other_person = message(2, "你知道我喜欢喝什么茶吗？");
        other_person.key.scope.peer_id = 654;
        other_person.sender_id = 8;

        for (incoming, same_person) in [(group, true), (private, true), (other_person, false)] {
            router.dispatch(incoming).await.unwrap();
            let request = mock.model().await;
            let prompt = request.body["messages"].to_string();
            assert_eq!(
                prompt.contains(CROSS_CHAT_PREFERENCE),
                enabled && same_person,
                "sharing={enabled}, same_person={same_person}: {prompt}"
            );
            assert!(
                !prompt.contains(SOURCE_CHAT_ONLY),
                "long-term sharing must keep source chat history separate"
            );
            assert!(!prompt.contains("已收到来源群消息"));
            if enabled && same_person {
                assert!(prompt.contains("关于用户 7"));
                assert!(prompt.contains("记录来源范围 123:group:456"));
            }
            // A generic reply keeps the next person's group history free of the
            // remembered preference, so this checks subject filtering directly.
            request.reply.send(answer("目标会话已处理")).unwrap();
            assert_eq!(mock.sent().await, "目标会话已处理");
        }
        within(router.shutdown()).await.unwrap();
        assert!(mock.models.try_recv().is_err());
        assert!(mock.sends.try_recv().is_err());
        drop(host);
        mock.stop().await;
    }
}

#[tokio::test]
async fn forgetting_shared_memory_during_another_groups_model_wait_blocks_its_stale_reply() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.ai.defaults.summary = false;
        config.plugins.ai.defaults.memory_write = false;
    })
    .await;
    let (source_guard, id) = seed_cross_chat_preference(&host, &router, &mut mock).await;
    let mut question = message(1, "说说我的饮茶偏好");
    question.key.scope.peer_id = 654;
    question.sender_id = 7;
    router.dispatch(question.clone()).await.unwrap();
    let old = mock.model().await;
    assert!(
        old.body["messages"]
            .to_string()
            .contains(CROSS_CHAT_PREFERENCE)
    );

    host.memory.forget(&source_guard, 7, id).await.unwrap();
    old.reply.send(answer("旧回复不得发送")).unwrap();
    question.key.message_seq = 2;
    question.text = "遗忘后的新问题".into();
    router.dispatch(question).await.unwrap();
    let fresh = mock.model().await;
    let prompt = fresh.body["messages"].to_string();
    assert!(!prompt.contains(CROSS_CHAT_PREFERENCE));
    assert!(!prompt.contains("说说我的饮茶偏好"));
    assert!(!prompt.contains(SOURCE_CHAT_ONLY));
    fresh.reply.send(answer("遗忘后的新回复")).unwrap();
    assert_eq!(mock.sent().await, "遗忘后的新回复");
    within(router.shutdown()).await.unwrap();
    assert!(mock.sends.try_recv().is_err());
    assert!(mock.models.try_recv().is_err());
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
    assert_eq!(
        within(mock.quotes.recv()).await.unwrap(),
        json!({"message_scene":"group","peer_id":456,"message_seq":100000})
    );

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
    assert_eq!(
        within(mock.quotes.recv()).await.unwrap(),
        json!({"message_scene":"group","peer_id":456,"message_seq":100000})
    );
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
    assert_eq!(
        within(mock.quotes.recv()).await.unwrap(),
        json!({"message_scene":"group","peer_id":456,"message_seq":100000})
    );
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
    assert_final_tools_prompt(&last.body, false);
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
            assert_eq!(request.body["tool_choice"], "none");
            assert_final_tools_prompt(&request.body, false);
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

async fn setup_reply_with(
    mock: &MockServer,
    configure: impl FnOnce(&mut Config),
) -> (tempfile::TempDir, Arc<Host>, Router) {
    setup_with(mock, true, |config| {
        config
            .plugins
            .ai
            .defaults
            .allowed_tools
            .push("reply_message".into());
        config.plugins.ai.defaults.input_budget = 20_000;
        configure(config);
    })
    .await
}

fn model_tool_call(id: &str, name: &str, arguments: Value) -> Value {
    json!({"id":id,"type":"function","function":{
        "name":name,"arguments":arguments.to_string()
    }})
}

fn model_tool_response(calls: Vec<Value>) -> Value {
    json!({"model":"mock","choices":[{"finish_reason":"tool_calls","message":{
        "role":"assistant","content":null,"tool_calls":calls
    }}]})
}

fn last_model_tool_result(body: &Value, call_id: &str) -> Value {
    let result = body["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(result["role"], "tool");
    assert_eq!(result["tool_call_id"], call_id);
    serde_json::from_str(result["content"].as_str().unwrap()).unwrap()
}

fn assert_final_tools_prompt(body: &Value, reply_enabled: bool) {
    let system = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "system")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    let rules = system.split_once("【当前人设】").unwrap().0;
    assert!(rules.contains("禁调历史工具"));
    assert!(rules.contains("仅empty可称空库"));
    assert!(!rules.contains("表情先search_stickers"));
    if reply_enabled {
        assert!(rules.contains("不再检索表情"));
        assert!(rules.contains("reply_message.sticker_ref"));
    } else {
        assert!(!rules.contains("reply_message"));
        assert!(rules.contains("最终直接输出正文"));
    }
    assert!(rules.contains(if reply_enabled {
        "本轮仅reply_message或文字结束"
    } else {
        "本轮仅文字结束"
    }));
}

fn enable_sticker_search(config: &mut Config, mock_url: &str) {
    if !config
        .plugins
        .ai
        .defaults
        .allowed_tools
        .iter()
        .any(|tool| tool == "search_stickers")
    {
        config
            .plugins
            .ai
            .defaults
            .allowed_tools
            .push("search_stickers".into());
    }
    config.embedding = qkbot::embedding::EmbeddingConfig {
        enabled: true,
        embeddings_url: format!("{mock_url}/v1/embeddings"),
        api_key: "synthetic".into(),
        dimensions: 64,
        ..Default::default()
    };
    config.stickers.auto_collect = false;
}

fn sticker_vector() -> Vec<f32> {
    let mut vector = vec![0.0; 64];
    vector[0] = 1.0;
    vector
}

async fn seed_sticker(host: &Host, incoming: &Incoming) {
    use sha2::{Digest, Sha256};
    let conversation = qkbot::plugins::ai::conversation(host, incoming)
        .await
        .unwrap();
    let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(48, 32, |x, y| {
        if x > 10 && x < 38 && y > 6 && y < 26 {
            image::Rgb([240, 170, 40])
        } else {
            image::Rgb([30, 90, 150])
        }
    }));
    let mut original = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut original, image::ImageFormat::Png)
        .unwrap();
    let original = original.into_inner();
    let mut thumbnail = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut thumbnail, 80)
        .encode_image(&image)
        .unwrap();
    let hash = format!("{:x}", Sha256::digest(&original));
    let thumbnail_hash = format!("{:x}", Sha256::digest(&thumbnail));
    let scope = incoming.key.scope.key();
    let directory = std::path::Path::new(&host.config.app.database)
        .parent()
        .unwrap()
        .join("stickers")
        .join(format!("{:x}", Sha256::digest(scope.as_bytes())));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join(format!("{hash}.original")), &original).unwrap();
    std::fs::write(directory.join(format!("{hash}.jpg")), &thumbnail).unwrap();
    let fingerprint = host.embedding.as_ref().unwrap().fingerprint().to_owned();
    let vector = qkbot::embedding::encode_vector(&sticker_vector()).unwrap();
    let source = format!("{scope}:990");
    host.store.call(move |db| {
        db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,learnable) VALUES(?1,?2,?3,?4,789,'control','[合成表情来源]',?5,0)", rusqlite::params![source,scope,conversation.guard.conversation_id,conversation.guard.epoch,now()])?;
        db.execute("INSERT INTO ai_stickers(scope,hash,mime,original_bytes,thumbnail_bytes,thumbnail_hash,fingerprint,dimensions,vector,created_at,updated_at) VALUES(?1,?2,'image/png',?3,?4,?5,?6,64,?7,?8,?8)",rusqlite::params![scope,hash,original.len(),thumbnail.len(),thumbnail_hash,fingerprint,vector,now()])?;
        let id = db.last_insert_rowid();
        db.execute("INSERT INTO ai_sticker_sources(sticker_id,source_key,scope,conversation_id,epoch) VALUES(?1,?2,?3,?4,?5)",rusqlite::params![id,source,scope,conversation.guard.conversation_id,conversation.guard.epoch])?;
        Ok(())
    }).await.unwrap();
}

#[tokio::test]
async fn sticker_search_shows_thumbnail_then_sends_one_selected_image_with_reply() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        config.plugins.ai.defaults = qkbot::config::AiSettings::default();
        enable_sticker_search(config, &mock.url);
    })
    .await;
    let incoming = message(1, "一起庆祝这次成功吧");
    seed_sticker(&host, &incoming).await;
    router.dispatch(incoming).await.unwrap();
    let request = mock.model().await;
    let current = current_message_payload(&request.body);
    assert!(
        request.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "search_stickers")
    );
    request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "find-celebration",
            "search_stickers",
            json!({"query":"开心庆祝成功"}),
        )]))
        .unwrap();
    let embedding = within(mock.embeddings.recv()).await.unwrap();
    assert_eq!(embedding.body["input"]["text"], "开心庆祝成功");
    embedding.reply.send(json!({"object":"list","model":"Qwen/Qwen3-VL-Embedding-8B","data":[{"object":"embedding","index":0,"embedding":sticker_vector()}],"usage":{"prompt_tokens":3,"total_tokens":3}})).unwrap();
    let selected = mock.model().await;
    let messages = selected.body["messages"].as_array().unwrap();
    let result = messages
        .iter()
        .find(|message| message["tool_call_id"] == "find-celebration")
        .unwrap();
    let result: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    let candidates = result["candidates"].as_array().unwrap();
    assert_eq!(result["status"], "ready");
    assert_eq!(result["candidate_count"], 1);
    assert_eq!(result["preview_included"], true);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["position"], 1);
    let previews: Vec<_> = messages
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .filter(|part| part["type"] == "image_url")
        .collect();
    assert_eq!(
        previews.len(),
        1,
        "selection must see the actual candidate thumbnail"
    );
    let preview = previews[0]["image_url"]["url"].as_str().unwrap();
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(preview.strip_prefix("data:image/jpeg;base64,").unwrap())
        .unwrap();
    let decoded = image::load_from_memory(&decoded).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (320, 320));
    assert!(!result.to_string().contains(".original"));
    assert!(
        mock.sends.try_recv().is_err(),
        "search must not send a message"
    );
    selected
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "selected-sticker",
            "reply_message",
            json!({
                "text":"一起庆祝！", "reply_to":current["message_ref"],
                "sticker_ref":candidates[0]["sticker_ref"],
            }),
        )]))
        .unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    let segments = sent["message"].as_array().unwrap();
    assert_eq!(
        segments.len(),
        3,
        "one terminal reply contains quote, text and one image"
    );
    assert_eq!(
        segments.iter().find(|s| s["type"] == "reply").unwrap()["data"]["message_seq"],
        1
    );
    assert_eq!(
        segments.iter().find(|s| s["type"] == "text").unwrap()["data"]["text"],
        "一起庆祝！"
    );
    let images: Vec<_> = segments.iter().filter(|s| s["type"] == "image").collect();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0]["data"]["sub_type"], "sticker");
    assert!(
        images[0]["data"]["uri"]
            .as_str()
            .unwrap()
            .contains("/native-media/")
    );
    wait_for_outbox(&host, "confirmed", 1).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.sends.try_recv().is_err());
    assert!(mock.models.try_recv().is_err());
    assert!(mock.embeddings.try_recv().is_err());
    assert!(mock.quotes.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn sticker_search_budget_omits_unseen_references_without_claiming_empty_library() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        config.plugins.ai.defaults = qkbot::config::AiSettings::default();
        enable_sticker_search(config, &mock.url);
    })
    .await;
    assert_eq!(host.config.plugins.ai.defaults.input_budget, 6000);
    let incoming = message(1, "发一个庆祝表情");
    seed_sticker(&host, &incoming).await;
    router.dispatch(incoming).await.unwrap();
    let request = mock.model().await;
    let current = current_message_payload(&request.body);
    let mut response = model_tool_response(vec![model_tool_call(
        "find-with-long-explanation",
        "search_stickers",
        json!({"query":"开心庆祝"}),
    )]);
    response["choices"][0]["message"]["content"] = json!("");
    let reply_tools: Vec<_> = request.body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tool| tool["function"]["name"] == "reply_message")
        .cloned()
        .collect();
    assert_eq!(reply_tools.len(), 1);
    let mut lower_bound_messages = request.body["messages"].as_array().unwrap().clone();
    assert_eq!(
        lower_bound_messages.len(),
        2,
        "the budget fixture must have no removable history"
    );
    assert_eq!(lower_bound_messages[0]["role"], "system");
    assert_eq!(lower_bound_messages[1]["role"], "user");
    lower_bound_messages.push(response["choices"][0]["message"].clone());
    let estimate = qkbot::plugins::ai::memory::estimate_tokens;
    let lower_bound = estimate(&serde_json::to_string(&lower_bound_messages).unwrap())
        + estimate(&serde_json::to_string(&reply_tools).unwrap())
        + 64;
    // Preserve the default budget. The complete native assistant message alone
    // leaves less than the 1280-image reserve, while retaining 1100 bytes for the
    // actual tool result and the explicit unavailable-preview explanation.
    let padding = 4900usize
        .checked_sub(lower_bound)
        .expect("the fixture must leave room for a bounded assistant explanation");
    assert!(padding > 0);
    response["choices"][0]["message"]["content"] = json!("a".repeat(padding));
    let assistant = response["choices"][0]["message"].clone();
    request.reply.send(response).unwrap();
    let embedding = within(mock.embeddings.recv()).await.unwrap();
    embedding.reply.send(json!({"object":"list","model":"Qwen/Qwen3-VL-Embedding-8B","data":[{"object":"embedding","index":0,"embedding":sticker_vector()}],"usage":{"prompt_tokens":3,"total_tokens":3}})).unwrap();
    let followup = mock.model().await;
    assert_final_tools_prompt(&followup.body, true);
    let messages = followup.body["messages"].as_array().unwrap();
    let index = messages
        .iter()
        .position(|message| message == &assistant)
        .unwrap();
    assert_eq!(messages[index + 1]["role"], "tool");
    assert_eq!(
        messages[index + 1]["tool_call_id"],
        "find-with-long-explanation"
    );
    let result: Value =
        serde_json::from_str(messages[index + 1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(result["status"], "preview_unavailable");
    assert!(result["candidate_count"].as_u64().unwrap() > 0);
    assert_eq!(result["preview_included"], false);
    assert!(result["candidates"].as_array().unwrap().is_empty());
    let note = result["note"].as_str().unwrap();
    assert!(note.contains("预算") && note.contains("预览"));
    assert!(!result.to_string().contains("sticker_ref"));
    assert!(
        messages
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .all(|part| part["type"] != "image_url")
    );
    assert!(mock.sends.try_recv().is_err());
    followup.reply.send(model_tool_response(vec![model_tool_call(
        "explain-unavailable-preview", "reply_message",
        json!({"text":"图库有候选，但本轮预算不足以展示预览，先用文字庆祝！", "reply_to":current["message_ref"]}),
    )])).unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    let expected = json!([
        {"type":"reply","data":{"message_seq":1}},
        {"type":"text","data":{"text":"图库有候选，但本轮预算不足以展示预览，先用文字庆祝！"}},
    ]);
    assert_eq!(sent["message"], expected);
    wait_for_outbox(&host, "confirmed", 1).await;
    let saved: String = host
        .store
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT COUNT(*) FROM ai_stickers", [], |row| row
                    .get::<_, i64>(0))?,
                1
            );
            assert_eq!(
                db.query_row("SELECT COUNT(*) FROM outbox", [], |row| row
                    .get::<_, i64>(0))?,
                1
            );
            Ok(db.query_row(
                "SELECT body FROM outbox WHERE state='confirmed'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&saved).unwrap()["segments"],
        expected
    );
    within(router.shutdown()).await.unwrap();
    assert!(mock.sends.try_recv().is_err());
    assert!(mock.models.try_recv().is_err());
    assert!(mock.embeddings.try_recv().is_err());
    assert!(mock.quotes.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn sticker_search_nonempty_library_reports_cooldown_and_index_pending_without_embedding() {
    for status in ["cooldown", "index_pending"] {
        let mut mock = MockServer::start().await;
        let (_temp, host, router) = setup_reply_with(&mock, |config| {
            config.plugins.ai.defaults = qkbot::config::AiSettings::default();
            enable_sticker_search(config, &mock.url);
        })
        .await;
        let incoming = message(1, "发一个庆祝表情");
        seed_sticker(&host, &incoming).await;
        host.store.call(move |db| {
            match status {
                "cooldown" => { db.execute("INSERT INTO ai_sticker_scopes(scope,last_sent_at) VALUES('123:group:456',?1)", [now()])?; }
                "index_pending" => { db.execute("UPDATE ai_stickers SET fingerprint='synthetic-previous-space'", [])?; }
                _ => unreachable!(),
            }
            Ok(())
        }).await.unwrap();
        router.dispatch(incoming).await.unwrap();
        let request = mock.model().await;
        let current = current_message_payload(&request.body);
        request
            .reply
            .send(model_tool_response(vec![model_tool_call(
                "find-unavailable-library",
                "search_stickers",
                json!({"query":"庆祝"}),
            )]))
            .unwrap();
        let followup = mock.model().await;
        let messages = followup.body["messages"].as_array().unwrap();
        let result = messages
            .iter()
            .find(|message| message["tool_call_id"] == "find-unavailable-library")
            .unwrap();
        let result: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
        assert_eq!(result["status"], status);
        assert_eq!(result["preview_included"], false);
        assert!(result["candidates"].as_array().unwrap().is_empty());
        assert!(
            messages
                .iter()
                .filter_map(|message| message["content"].as_array())
                .flatten()
                .all(|part| part["type"] != "image_url")
        );
        assert!(mock.embeddings.try_recv().is_err());
        assert!(mock.sends.try_recv().is_err());
        let text = if status == "cooldown" {
            "图库有图，目前仍在冷却。"
        } else {
            "图库有图，正在等待索引。"
        };
        followup
            .reply
            .send(model_tool_response(vec![model_tool_call(
                "explain-library-status",
                "reply_message",
                json!({"text":text,"reply_to":current["message_ref"]}),
            )]))
            .unwrap();
        let sent = within(mock.sends.recv()).await.unwrap();
        assert_eq!(
            sent["message"],
            json!([
                {"type":"reply","data":{"message_seq":1}},
                {"type":"text","data":{"text":text}},
            ])
        );
        wait_for_outbox(&host, "confirmed", 1).await;
        host.store
            .call(|db| {
                assert_eq!(
                    db.query_row("SELECT COUNT(*) FROM ai_stickers", [], |row| row
                        .get::<_, i64>(0))?,
                    1
                );
                Ok(())
            })
            .await
            .unwrap();
        within(router.shutdown()).await.unwrap();
        assert!(mock.sends.try_recv().is_err());
        assert!(mock.models.try_recv().is_err());
        assert!(mock.embeddings.try_recv().is_err());
        assert!(mock.quotes.try_recv().is_err());
        assert!(mock.resources.try_recv().is_err());
        drop(host);
        mock.stop().await;
    }
}

#[tokio::test]
async fn truncated_sticker_tool_response_never_searches_sends_an_image_or_retries() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        config.plugins.ai.defaults = qkbot::config::AiSettings::default();
        enable_sticker_search(config, &mock.url);
    })
    .await;
    let incoming = message(1, "发一个庆祝表情");
    seed_sticker(&host, &incoming).await;
    router.dispatch(incoming).await.unwrap();
    let request = mock.model().await;
    assert_eq!(request.body["max_tokens"], 800);
    assert!(
        request.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "search_stickers")
    );
    let mut response = model_tool_response(vec![model_tool_call(
        "cut-off-search",
        "search_stickers",
        json!({"query":"庆祝"}),
    )]);
    response["choices"][0]["finish_reason"] = json!("length");
    response["choices"][0]["message"]["content"] = json!("private unfinished model output");
    response["usage"] = json!({"prompt_tokens":100,"completion_tokens":800,"total_tokens":900,
        "completion_tokens_details":{"reasoning_tokens":780}});
    request.reply.send(response).unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    let segments = sent["message"].as_array().unwrap();
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0]["type"], "text");
    let diagnostic = segments[0]["data"]["text"].as_str().unwrap();
    assert!(diagnostic.contains("模型输出达到长度上限"));
    assert!(diagnostic.contains("本次调用未执行"));
    assert!(!diagnostic.contains("private"));
    wait_for_outbox(&host, "confirmed", 1).await;
    host.store.call(|db| {
        let saved: (String,String,String,bool) = db.query_row(
            "SELECT role,origin_plugin,conversation_id,learnable FROM messages WHERE key='123:group:456:100000'", [],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
        )?;
        assert_eq!(saved, ("system".into(), "host".into(), String::new(), false));
        assert_eq!(db.query_row("SELECT COUNT(*) FROM messages WHERE role='assistant'", [], |r| r.get::<_,i64>(0))?, 0);
        assert_eq!(db.query_row("SELECT COUNT(*) FROM ai_sticker_admissions", [], |r| r.get::<_,i64>(0))?, 0);
        assert_eq!(db.query_row("SELECT COUNT(*) FROM tool_runs", [], |r| r.get::<_,i64>(0))?, 0);
        Ok(())
    }).await.unwrap();
    within(router.shutdown()).await.unwrap();
    assert!(mock.embeddings.try_recv().is_err());
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    assert!(mock.quotes.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn sticker_search_in_empty_group_never_borrows_another_groups_image() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        enable_sticker_search(config, &mock.url);
    })
    .await;
    let incoming = message(1, "发个表情包");
    let mut other_group = incoming.clone();
    other_group.key.scope.peer_id = 987;
    seed_sticker(&host, &other_group).await;
    router.dispatch(incoming).await.unwrap();
    let request = mock.model().await;
    let current = current_message_payload(&request.body);
    request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "find-empty-group",
            "search_stickers",
            json!({"query":"开心"}),
        )]))
        .unwrap();
    let followup = mock.model().await;
    let messages = followup.body["messages"].as_array().unwrap();
    let result = messages
        .iter()
        .find(|message| message["tool_call_id"] == "find-empty-group")
        .unwrap();
    let result: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(result["status"], "empty");
    assert_eq!(result["candidate_count"], 0);
    assert_eq!(result["preview_included"], false);
    assert!(result["candidates"].as_array().unwrap().is_empty());
    assert!(
        messages
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .all(|part| part["type"] != "image_url")
    );
    assert!(
        mock.embeddings.try_recv().is_err(),
        "an empty group requires no vector request"
    );
    followup.reply.send(model_tool_response(vec![model_tool_call(
        "explain-empty", "reply_message", json!({"text":"本群还没有可用表情，可引用图片发送 /save 入库。","reply_to":current["message_ref"]}),
    )])).unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    assert!(
        sent["message"]
            .as_array()
            .unwrap()
            .iter()
            .all(|segment| segment["type"] != "image")
    );
    wait_for_outbox(&host, "confirmed", 1).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn sticker_reply_rejects_raw_image_arguments_without_sending_or_fetching() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        enable_sticker_search(config, &mock.url);
        config.limits.max_model_calls = 4;
    })
    .await;
    router.dispatch(message(1, "请正常回复")).await.unwrap();
    let invalid = [
        (
            "forged-uri",
            json!({"text":"不应发送", "sticker_ref":"https://example.invalid/arbitrary.png"}),
        ),
        (
            "forged-object",
            json!({"text":"不应发送", "sticker_ref":{"uri":"https://example.invalid/arbitrary.png"}}),
        ),
        (
            "forged-image",
            json!({"text":"不应发送", "image":{"uri":"https://example.invalid/arbitrary.png"}}),
        ),
    ];
    let mut request = mock.model().await;
    for (id, args) in invalid {
        request
            .reply
            .send(model_tool_response(vec![model_tool_call(
                id,
                "reply_message",
                args,
            )]))
            .unwrap();
        request = mock.model().await;
        assert_failed_tool_results(&request.body, &[id]);
        assert!(
            mock.sends.try_recv().is_err(),
            "invalid image arguments must have no send side effect"
        );
        assert!(mock.embeddings.try_recv().is_err());
        assert!(mock.quotes.try_recv().is_err());
        assert!(mock.resources.try_recv().is_err());
    }
    request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "text-only",
            "reply_message",
            json!({"text":"正常文字回复"}),
        )]))
        .unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    assert_eq!(
        sent["message"],
        json!([{"type":"text","data":{"text":"正常文字回复"}}])
    );
    wait_for_outbox(&host, "confirmed", 1).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.sends.try_recv().is_err());
    assert!(mock.models.try_recv().is_err());
    assert!(mock.embeddings.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

async fn assert_sticker_loss_keeps_one_text_reply(loss: &'static str) {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        config.plugins.ai.defaults = qkbot::config::AiSettings::default();
        enable_sticker_search(config, &mock.url);
    })
    .await;
    let incoming = message(1, "一起庆祝这次成功吧");
    seed_sticker(&host, &incoming).await;
    router.dispatch(incoming).await.unwrap();
    let request = mock.model().await;
    let current = current_message_payload(&request.body);
    request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "search-before-loss",
            "search_stickers",
            json!({"query":"开心庆祝成功"}),
        )]))
        .unwrap();
    let embedding = within(mock.embeddings.recv()).await.unwrap();
    embedding.reply.send(json!({"object":"list","model":"Qwen/Qwen3-VL-Embedding-8B","data":[{"object":"embedding","index":0,"embedding":sticker_vector()}],"usage":{"prompt_tokens":3,"total_tokens":3}})).unwrap();
    let selected = mock.model().await;
    let messages = selected.body["messages"].as_array().unwrap();
    let result = messages
        .iter()
        .find(|message| message["tool_call_id"] == "search-before-loss")
        .unwrap();
    let result: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    let candidates = result["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        messages
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .filter(|part| part["type"] == "image_url")
            .count(),
        1,
        "the candidate must become visible before it is invalidated"
    );
    match loss {
        "cooldown" => host.store.call(|db| {
            db.execute("INSERT INTO ai_sticker_scopes(scope,last_sent_at) VALUES('123:group:456',?1) ON CONFLICT(scope) DO UPDATE SET last_sent_at=excluded.last_sent_at",[now()])?;
            Ok(())
        }).await.unwrap(),
        "revoked" => host.stickers.revoke("123:group:456:990").await.unwrap(),
        "revoked_after_prepare" => host.store.call(|db| {
            // The first authorization has succeeded and the outbox still contains
            // an image. This trigger invalidates only its sticker provenance before
            // the host's final submit transaction, without a timing-dependent sleep.
            db.execute_batch("CREATE TABLE sticker_preparation_audit(images INTEGER NOT NULL);
                CREATE TRIGGER revoke_sticker_after_prepare AFTER INSERT ON outbox
                WHEN NEW.origin_plugin='ai' AND NEW.state='prepared'
                BEGIN
                    INSERT INTO sticker_preparation_audit(images) SELECT COUNT(*) FROM json_each(NEW.body,'$.segments') WHERE json_extract(value,'$.type')='image';
                    INSERT INTO ai_sticker_revoked(source_key,revoked_at) VALUES('123:group:456:990',1);
                END;")?;
            Ok(())
        }).await.unwrap(),
        _ => panic!("unknown synthetic sticker loss"),
    }
    assert!(mock.sends.try_recv().is_err());
    selected
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "reply-after-loss",
            "reply_message",
            json!({
                "reply_to":current["message_ref"], "text":"一起庆祝！",
                "sticker_ref":candidates[0]["sticker_ref"],
            }),
        )]))
        .unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    let expected = json!([
        {"type":"reply","data":{"message_seq":1}},
        {"type":"text","data":{"text":"一起庆祝！"}},
    ]);
    assert_eq!(
        sent["message"], expected,
        "{loss}: invalidated optional image must not suppress the reply"
    );
    wait_for_outbox(&host, "confirmed", 1).await;
    let (body, total, prepared_images): (String, i64, Option<i64>) = host
        .store
        .call(move |db| {
            let body = db.query_row(
                "SELECT body FROM outbox WHERE state='confirmed'",
                [],
                |row| row.get(0),
            )?;
            let total = db.query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))?;
            let prepared_images = if loss == "revoked_after_prepare" {
                Some(
                    db.query_row("SELECT images FROM sticker_preparation_audit", [], |row| {
                        row.get(0)
                    })?,
                )
            } else {
                None
            };
            Ok((body, total, prepared_images))
        })
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        body["segments"], expected,
        "{loss}: the confirmed ledger must match the actual text-only submission"
    );
    assert_eq!(
        total, 1,
        "{loss}: fallback must not enqueue a second message"
    );
    if let Some(images) = prepared_images {
        assert_eq!(
            images, 1,
            "the race must happen after a valid image was prepared"
        );
    }
    within(router.shutdown()).await.unwrap();
    assert!(mock.sends.try_recv().is_err());
    assert!(mock.models.try_recv().is_err());
    assert!(mock.embeddings.try_recv().is_err());
    assert!(mock.quotes.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn sticker_reply_cooldown_or_revocation_after_preview_preserves_text_and_quote() {
    for loss in ["cooldown", "revoked"] {
        assert_sticker_loss_keeps_one_text_reply(loss).await;
    }
}

#[tokio::test]
async fn sticker_reply_revocation_after_outbox_preparation_updates_the_submitted_body() {
    assert_sticker_loss_keeps_one_text_reply("revoked_after_prepare").await;
}

fn current_message_payload(body: &Value) -> Value {
    let message = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .unwrap();
    serde_json::from_str(message["content"].as_str().unwrap()).unwrap()
}

fn payload_with_text(body: &Value, text: &str) -> Value {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| serde_json::from_str::<Value>(message["content"].as_str()?).ok())
        .find(|payload| payload["text"] == text)
        .expect("the referenced message must be visible in the model request")
}

fn assert_failed_tool_results(body: &Value, expected_ids: &[&str]) {
    let messages = body["messages"].as_array().unwrap();
    let assistant_index = messages
        .iter()
        .rposition(|message| message.get("tool_calls").is_some())
        .unwrap();
    let calls = messages[assistant_index]["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), expected_ids.len());
    let results = &messages[assistant_index + 1..];
    assert_eq!(results.len(), expected_ids.len());
    for ((call, result), expected) in calls.iter().zip(results).zip(expected_ids) {
        assert_eq!(call["id"], *expected);
        assert_eq!(result["role"], "tool");
        assert_eq!(result["tool_call_id"], *expected);
        let content: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["status"], "failed");
        assert!(!content["error"].as_str().unwrap().is_empty());
    }
}

async fn wait_for_outbox(host: &Host, state: &str, count: i64) {
    within(async {
        loop {
            let state = state.to_owned();
            let found: i64 = host
                .store
                .call(move |db| {
                    Ok(db.query_row(
                        "SELECT COUNT(*) FROM outbox WHERE state=?1",
                        [state],
                        |row| row.get(0),
                    )?)
                })
                .await
                .unwrap();
            if found == count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
}

#[tokio::test]
async fn reply_tool_combines_quote_mention_and_text_once_even_with_one_model_call() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        config.limits.max_model_calls = 1;
    })
    .await;
    router
        .dispatch(message(7, "直接引用并提及我"))
        .await
        .unwrap();
    let request = mock.model().await;
    let current = current_message_payload(&request.body);
    assert_eq!(current["text"], "直接引用并提及我");
    assert!(!current["message_ref"].as_str().unwrap().is_empty());
    assert!(!current["sender_ref"].as_str().unwrap().is_empty());
    let tools = request.body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "only the final reply tool fits this budget");
    assert_eq!(tools[0]["function"]["name"], "reply_message");
    let mut response = model_tool_response(vec![model_tool_call(
        "final-reply",
        "reply_message",
        json!({"reply_to":current["message_ref"],"mention_users":[current["sender_ref"]],"text":"组合回复"}),
    )]);
    response["choices"][0]["message"]["content"] = json!("不应额外发送的模型说明");
    request.reply.send(response).unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    assert_eq!(sent["group_id"], 456);
    assert_eq!(
        sent["message"],
        json!([
            {"type":"reply","data":{"message_seq":7}},
            {"type":"mention","data":{"user_id":789}},
            {"type":"text","data":{"text":"组合回复"}}
        ])
    );
    wait_for_outbox(&host, "confirmed", 1).await;
    let stored: (String, String) = host
        .store
        .call(|db| {
            Ok(db.query_row(
                "SELECT role,content FROM messages WHERE key='123:group:456:100000'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(stored, ("assistant".into(), "组合回复".into()));
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn reply_tool_history_refs_distinguish_same_named_people_and_assistant_messages() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |_| {}).await;
    for (seq, sender, text, response) in [
        (1, 789, "第一位的消息", "第一条机器人答复"),
        (2, 999, "第二位的消息", "第二条机器人答复"),
    ] {
        let mut incoming = message(seq, text);
        incoming.sender_id = sender;
        incoming.sender_card = Some("同名成员".into());
        router.dispatch(incoming).await.unwrap();
        mock.model().await.reply.send(answer(response)).unwrap();
        assert_eq!(mock.sent().await, response);
        wait_for_outbox(&host, "confirmed", seq).await;
    }
    let mut incoming = message(3, "请回复第一位并提醒第二位和我");
    incoming.sender_id = 888;
    incoming.sender_card = Some("同名成员".into());
    router.dispatch(incoming).await.unwrap();
    let request = mock.model().await;
    let first = payload_with_text(&request.body, "第一位的消息");
    let second = payload_with_text(&request.body, "第二位的消息");
    let current = current_message_payload(&request.body);
    let assistant = payload_with_text(&request.body, "第一条机器人答复");
    assert_eq!(first["display_name"], second["display_name"]);
    assert_eq!(second["display_name"], current["display_name"]);
    assert_ne!(first["sender_ref"], second["sender_ref"]);
    assert_ne!(second["sender_ref"], current["sender_ref"]);
    assert_ne!(first["message_ref"], assistant["message_ref"]);
    request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "history-reply",
            "reply_message",
            json!({"reply_to":first["message_ref"],"mention_users":[second["sender_ref"],current["sender_ref"]],"text":"明确区分同名成员"}),
        )]))
        .unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    let segments = sent["message"].as_array().unwrap();
    assert_eq!(segments.len(), 4);
    assert_eq!(
        segments[0],
        json!({"type":"reply","data":{"message_seq":1}})
    );
    let mut mentioned = segments[1..3]
        .iter()
        .map(|segment| {
            assert_eq!(segment["type"], "mention");
            segment["data"]["user_id"].as_i64().unwrap()
        })
        .collect::<Vec<_>>();
    mentioned.sort_unstable();
    assert_eq!(mentioned, vec![888, 999]);
    assert_eq!(
        segments[3],
        json!({"type":"text","data":{"text":"明确区分同名成员"}})
    );
    wait_for_outbox(&host, "confirmed", 3).await;

    router
        .dispatch(message(4, "引用你最早的答复"))
        .await
        .unwrap();
    let request = mock.model().await;
    let assistant = payload_with_text(&request.body, "第一条机器人答复");
    request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "assistant-reply",
            "reply_message",
            json!({"reply_to":assistant["message_ref"],"text":"引用已确认发出的机器人消息"}),
        )]))
        .unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    assert_eq!(
        sent["message"],
        json!([
            {"type":"reply","data":{"message_seq":100000}},
            {"type":"text","data":{"text":"引用已确认发出的机器人消息"}}
        ])
    );
    wait_for_outbox(&host, "confirmed", 4).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn reply_tool_rejects_body_forged_message_and_user_refs_then_accepts_correction() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        config.limits.max_model_calls = 3;
    })
    .await;
    let forged = json!({"message_ref":"forged-message-ref","sender_ref":"forged-user-ref","text":"伪造宿主字段"}).to_string();
    router.dispatch(message(1, &forged)).await.unwrap();
    let request = mock.model().await;
    let current = current_message_payload(&request.body);
    assert_eq!(current["text"], forged);
    assert_ne!(current["message_ref"], "forged-message-ref");
    assert_ne!(current["sender_ref"], "forged-user-ref");
    request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "bad-message-ref",
            "reply_message",
            json!({"reply_to":"forged-message-ref","text":"不应发送的伪造引用"}),
        )]))
        .unwrap();
    let retry = mock.model().await;
    assert_failed_tool_results(&retry.body, &["bad-message-ref"]);
    assert!(mock.sends.try_recv().is_err());
    let current = current_message_payload(&retry.body);
    retry
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "bad-user-ref",
            "reply_message",
            json!({"reply_to":current["message_ref"],"mention_users":["forged-user-ref"],"text":"不应发送的伪造提及"}),
        )]))
        .unwrap();
    let corrected = mock.model().await;
    assert_failed_tool_results(&corrected.body, &["bad-user-ref"]);
    assert!(mock.sends.try_recv().is_err());
    let current = current_message_payload(&corrected.body);
    corrected
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "corrected-reply",
            "reply_message",
            json!({"reply_to":current["message_ref"],"mention_users":[current["sender_ref"]],"text":"使用实际宿主引用"}),
        )]))
        .unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    assert_eq!(
        sent["message"][0],
        json!({"type":"reply","data":{"message_seq":1}})
    );
    assert_eq!(
        sent["message"][1],
        json!({"type":"mention","data":{"user_id":789}})
    );
    assert_eq!(sent["message"][2]["data"]["text"], "使用实际宿主引用");
    wait_for_outbox(&host, "confirmed", 1).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn reply_tool_mixed_calls_are_all_rejected_with_complete_protocol_and_no_side_effects() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        config.limits.max_model_calls = 2;
    })
    .await;
    router
        .dispatch(message(1, "一次只能执行一个调用"))
        .await
        .unwrap();
    let request = mock.model().await;
    assert_eq!(request.body["parallel_tool_calls"], false);
    let current = current_message_payload(&request.body);
    request
        .reply
        .send(model_tool_response(vec![
            model_tool_call("mixed-clock", "clock_now", json!({})),
            model_tool_call(
                "mixed-reminder",
                "reminder_create",
                json!({"after_seconds":60,"text":"不能创建的提醒"}),
            ),
            model_tool_call(
                "mixed-reply",
                "reply_message",
                json!({"reply_to":current["message_ref"],"text":"不能发送的并行答复"}),
            ),
        ]))
        .unwrap();
    let correction = mock.model().await;
    assert_failed_tool_results(
        &correction.body,
        &["mixed-clock", "mixed-reminder", "mixed-reply"],
    );
    assert!(mock.sends.try_recv().is_err());
    let counts: (i64, i64, i64) = host
        .store
        .call(|db| {
            Ok((
                db.query_row("SELECT COUNT(*) FROM tool_runs", [], |row| row.get(0))?,
                db.query_row("SELECT COUNT(*) FROM reminder_jobs", [], |row| row.get(0))?,
                db.query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (0, 0, 0));
    correction
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "single-corrected-reply",
            "reply_message",
            json!({"text":"修正后唯一答复"}),
        )]))
        .unwrap();
    assert_eq!(mock.sent().await, "修正后唯一答复");
    wait_for_outbox(&host, "confirmed", 1).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn reply_tool_remains_available_after_the_ordinary_tool_budget_is_exhausted() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        config.limits.max_model_calls = 3;
        config.limits.max_tool_calls = 1;
    })
    .await;
    router
        .dispatch(message(1, "先看时间再引用回复"))
        .await
        .unwrap();
    mock.model()
        .await
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "only-clock",
            "clock_now",
            json!({}),
        )]))
        .unwrap();
    let final_request = mock.model().await;
    let tools = final_request.body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "reply_message");
    assert_final_tools_prompt(&final_request.body, true);
    let current = current_message_payload(&final_request.body);
    final_request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "reply-after-clock",
            "reply_message",
            json!({"reply_to":current["message_ref"],"text":"时间查询后的唯一答复"}),
        )]))
        .unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    assert_eq!(
        sent["message"],
        json!([
            {"type":"reply","data":{"message_seq":1}},
            {"type":"text","data":{"text":"时间查询后的唯一答复"}}
        ])
    );
    wait_for_outbox(&host, "confirmed", 1).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn reply_tool_private_mentions_fail_then_reply_without_a_mention_succeeds() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        config.limits.max_model_calls = 2;
    })
    .await;
    let mut private = message(3, "私聊里引用即可");
    private.key.scope.scene = Scene::Friend;
    private.key.scope.peer_id = 789;
    private.mentions.clear();
    router.dispatch(private).await.unwrap();
    let request = mock.model().await;
    let current = current_message_payload(&request.body);
    request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "private-mention",
            "reply_message",
            json!({"reply_to":current["message_ref"],"mention_users":[current["sender_ref"]],"text":"不能在私聊发出提及"}),
        )]))
        .unwrap();
    let correction = mock.model().await;
    assert_failed_tool_results(&correction.body, &["private-mention"]);
    assert!(mock.sends.try_recv().is_err());
    let current = current_message_payload(&correction.body);
    correction
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "private-reply",
            "reply_message",
            json!({"reply_to":current["message_ref"],"text":"私聊中的引用答复"}),
        )]))
        .unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    assert_eq!(sent["user_id"], 789);
    assert_eq!(
        sent["message"],
        json!([
            {"type":"reply","data":{"message_seq":3}},
            {"type":"text","data":{"text":"私聊中的引用答复"}}
        ])
    );
    wait_for_outbox(&host, "confirmed", 1).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn reply_tool_cannot_bypass_the_session_allowlist_or_disabled_tool_calling() {
    for disable_protocol in [false, true] {
        let mut mock = MockServer::start().await;
        let (_temp, host, router) = setup_reply_with(&mock, |config| {
            config.limits.max_model_calls = 1;
            if disable_protocol {
                config.llm.tool_calling = false;
            } else {
                config
                    .plugins
                    .ai
                    .defaults
                    .allowed_tools
                    .retain(|tool| tool != "reply_message");
            }
        })
        .await;
        router
            .dispatch(message(1, "不允许使用回复工具"))
            .await
            .unwrap();
        let request = mock.model().await;
        assert!(request.body.get("tools").is_none());
        request
            .reply
            .send(model_tool_response(vec![model_tool_call(
                "disabled-reply",
                "reply_message",
                json!({"text":"模型越权发送的正文"}),
            )]))
            .unwrap();
        let sent = within(mock.sends.recv()).await.unwrap();
        let segments = sent["message"].as_array().unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0]["type"], "text");
        let text = segments[0]["data"]["text"].as_str().unwrap();
        assert_ne!(text, "模型越权发送的正文");
        assert!(text.contains("未完成"));
        wait_for_outbox(&host, "confirmed", 1).await;
        within(router.shutdown()).await.unwrap();
        assert!(mock.models.try_recv().is_err());
        assert!(mock.sends.try_recv().is_err());
        drop(host);
        mock.stop().await;
    }
}

#[tokio::test]
async fn reply_tool_target_revoked_during_model_request_is_rejected_before_delivery() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        config.limits.max_model_calls = 2;
    })
    .await;
    let mut background = message(1, "稍后会被遗忘的目标");
    background.mentions.clear();
    router.dispatch(background).await.unwrap();
    router.dispatch(message(2, "请引用上条消息")).await.unwrap();
    let request = mock.model().await;
    let target = payload_with_text(&request.body, "稍后会被遗忘的目标");
    host.store
        .call(|db| {
            db.execute(
                "INSERT INTO ai_revoked_sources VALUES('123:group:456:1',1)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "revoked-target",
            "reply_message",
            json!({"reply_to":target["message_ref"],"text":"不可发送的失效引用"}),
        )]))
        .unwrap();
    let correction = mock.model().await;
    assert_failed_tool_results(&correction.body, &["revoked-target"]);
    assert!(mock.sends.try_recv().is_err());
    correction
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "without-revoked-target",
            "reply_message",
            json!({"text":"不再引用已遗忘消息"}),
        )]))
        .unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    assert_eq!(
        sent["message"],
        json!([
            {"type":"text","data":{"text":"不再引用已遗忘消息"}}
        ])
    );
    wait_for_outbox(&host, "confirmed", 1).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn reply_tool_lost_send_ack_is_unknown_and_never_generates_a_second_answer() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |_| {}).await;
    router
        .dispatch(message(1, "确认丢失也不能重复发送"))
        .await
        .unwrap();
    let request = mock.model().await;
    let current = current_message_payload(&request.body);
    mock.drop_send_ack.store(true, Ordering::Relaxed);
    request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "lost-ack-reply",
            "reply_message",
            json!({"reply_to":current["message_ref"],"text":"只提交这一次"}),
        )]))
        .unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    assert_eq!(
        sent["message"],
        json!([
            {"type":"reply","data":{"message_seq":1}},
            {"type":"text","data":{"text":"只提交这一次"}}
        ])
    );
    wait_for_outbox(&host, "unknown", 1).await;
    router.maintenance().await.unwrap();
    router.maintenance().await.unwrap();
    let counts: (i64, i64) = host
        .store
        .call(|db| {
            Ok((
                db.query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))?,
                db.query_row(
                    "SELECT COUNT(*) FROM messages WHERE role='assistant'",
                    [],
                    |row| row.get(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(
        counts,
        (1, 0),
        "an unconfirmed reply cannot enter assistant history"
    );
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn reply_tool_can_quote_and_mention_a_verified_remote_snapshot_absent_from_local_history() {
    let mut mock = MockServer::start().await;
    mock.control_quotes.store(true, Ordering::Relaxed);
    let (_temp, host, router) = setup_reply_with(&mock, |_| {}).await;
    let mut incoming = message(1, "请引用这条旧消息并提醒它的作者");
    incoming.quote_seq = Some(77);
    router.dispatch(incoming).await.unwrap();
    let lookup = within(mock.quote_requests.recv()).await.unwrap();
    assert_eq!(
        lookup.body,
        json!({
            "message_scene":"group","peer_id":456,"message_seq":77
        })
    );
    assert_eq!(within(mock.quotes.recv()).await.unwrap(), lookup.body);
    lookup
        .reply
        .send(quoted(&lookup.body, 6543, "仅协议查询可见的旧消息"))
        .unwrap();
    let request = mock.model().await;
    let content = request.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    let (current, quoted_content) = content
        .split_once("\n[引用消息]\n")
        .expect("the trusted quote snapshot must accompany the current message");
    let current: Value = serde_json::from_str(current).unwrap();
    let quote: Value = serde_json::from_str(quoted_content).unwrap();
    assert_eq!(quote["sender_id"], "6543");
    assert!(
        quote["text"]
            .as_str()
            .unwrap()
            .contains("仅协议查询可见的旧消息")
    );
    assert_ne!(quote["message_ref"], current["message_ref"]);
    assert_ne!(quote["sender_ref"], current["sender_ref"]);
    let local_rows: i64 = host
        .store
        .call(|db| {
            Ok(db.query_row(
                "SELECT COUNT(*) FROM messages WHERE key='123:group:456:77'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        local_rows, 0,
        "the grant must come from the resolved quote snapshot"
    );
    request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "reply-to-remote-snapshot",
            "reply_message",
            json!({
                "reply_to":quote["message_ref"],
                "mention_users":[quote["sender_ref"]],
                "text":"引用旧消息并提醒原作者"
            }),
        )]))
        .unwrap();
    let sent = within(mock.sends.recv()).await.unwrap();
    assert_eq!(sent["group_id"], 456);
    assert_eq!(
        sent["message"],
        json!([
            {"type":"reply","data":{"message_seq":77}},
            {"type":"mention","data":{"user_id":6543}},
            {"type":"text","data":{"text":"引用旧消息并提醒原作者"}}
        ])
    );
    wait_for_outbox(&host, "confirmed", 1).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    assert!(mock.quotes.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn reply_tool_revocation_after_outbox_preparation_blocks_the_final_http_submission() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_reply_with(&mock, |config| {
        config.limits.max_model_calls = 1;
    })
    .await;
    let mut background = message(1, "发送准备完成后将失效的目标");
    background.mentions.clear();
    router.dispatch(background).await.unwrap();
    router
        .dispatch(message(2, "引用目标并发送一次"))
        .await
        .unwrap();
    let request = mock.model().await;
    let target = payload_with_text(&request.body, "发送准备完成后将失效的目标");
    // This trigger is a deterministic synchronization point: resolve and the
    // first send authorization have already succeeded, but submission has not.
    host.store
        .call(|db| {
            db.execute_batch(
                "CREATE TRIGGER revoke_reply_after_prepare AFTER INSERT ON outbox
                 WHEN NEW.origin_plugin='ai' AND NEW.state='prepared'
                 BEGIN
                   INSERT INTO ai_revoked_sources(source_key,revoked_at)
                   VALUES('123:group:456:1',1);
                 END;",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    request
        .reply
        .send(model_tool_response(vec![model_tool_call(
            "prepared-then-revoked",
            "reply_message",
            json!({"reply_to":target["message_ref"],"text":"失效后绝不能提交到QQ"}),
        )]))
        .unwrap();
    wait_for_outbox(&host, "failed", 1).await;
    let (origin, body, detail, assistants, revoked): (String, String, String, i64, i64) = host
        .store
        .call(|db| {
            let (origin, body, detail) =
                db.query_row("SELECT origin_plugin,body,detail FROM outbox", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
            Ok((
                origin,
                body,
                detail,
                db.query_row(
                    "SELECT COUNT(*) FROM messages WHERE role='assistant'",
                    [],
                    |row| row.get(0),
                )?,
                db.query_row(
                    "SELECT COUNT(*) FROM ai_revoked_sources WHERE source_key='123:group:456:1'",
                    [],
                    |row| row.get(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(origin, "ai", "the validated reply must reach preparation");
    let body: qkbot::types::OutgoingMessage = serde_json::from_str(&body).unwrap();
    assert_eq!(
        body.segments,
        vec![
            qkbot::types::OutgoingSegment::Reply { message_seq: 1 },
            qkbot::types::OutgoingSegment::Text {
                text: "失效后绝不能提交到QQ".into()
            },
        ]
    );
    assert!(detail.starts_with("not submitted:"), "{detail}");
    assert_eq!((assistants, revoked), (0, 1));
    within(router.shutdown()).await.unwrap();
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

fn image_message(seq: i64, text: &str) -> Incoming {
    let mut incoming = message(seq, text);
    incoming.raw_message = json!({"segments":[{
        "type":"image","data":{
            "resource_id":"pic-one",
            "temp_url":"http://127.0.0.1:9/untrusted-event-preview.png"
        }
    }]});
    incoming
}

#[tokio::test]
async fn vision_untriggered_group_image_never_requests_a_message_resource_or_model() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.llm.vision = true;
    })
    .await;
    let mut incoming = image_message(1, "群里的普通图片");
    incoming.mentions.clear();
    router.dispatch(incoming).await.unwrap();
    within(router.shutdown()).await.unwrap();
    assert!(mock.quotes.try_recv().is_err());
    assert!(mock.quote_requests.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn vision_image_in_a_native_plugin_command_does_not_start_ai_or_resource_reads() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.llm.vision = true;
    })
    .await;
    router.dispatch(image_message(1, "/clock")).await.unwrap();
    let clock = mock.sent().await;
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(&clock)
            .unwrap()
            .offset()
            .local_minus_utc(),
        8 * 3600
    );
    wait_for_outbox(&host, "confirmed", 1).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.quotes.try_recv().is_err());
    assert!(mock.quote_requests.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn vision_unsafe_verified_resource_url_degrades_to_text_without_storing_image_data() {
    let mut mock = MockServer::start().await;
    mock.control_quotes.store(true, Ordering::Relaxed);
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.llm.vision = true;
    })
    .await;
    router
        .dispatch(image_message(5, "请描述这张图片"))
        .await
        .unwrap();
    let lookup = within(mock.quote_requests.recv()).await.unwrap();
    assert_eq!(
        lookup.body,
        json!({
            "message_scene":"group","peer_id":456,"message_seq":5
        })
    );
    assert_eq!(within(mock.quotes.recv()).await.unwrap(), lookup.body);
    // Resource lookup cannot precede verification that this exact message in
    // the current physical chat contains the incoming image resource ID.
    assert!(mock.resources.try_recv().is_err());
    let mut verified = quoted(&lookup.body, 789, "请描述这张图片");
    verified["data"]["message"]["segments"] = json!([
        {"type":"text","data":{"text":"请描述这张图片"}},
        {"type":"image","data":{"resource_id":"pic-one"}}
    ]);
    lookup.reply.send(verified).unwrap();
    let resource = within(mock.resources.recv()).await.unwrap();
    assert_eq!(resource.body, json!({"resource_id":"pic-one"}));
    resource
        .reply
        .send(json!({"status":"ok","retcode":0,"data":{
            "url":"https://127.0.0.1/private-image.png"
        }}))
        .unwrap();
    let request = mock.model().await;
    let current = request.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .as_str()
        .expect("an unreadable image must leave a normal text model request");
    assert!(current.contains("请描述这张图片"));
    assert!(current.contains("图片") && current.contains("未能读取"));
    let payload = request.body.to_string();
    assert!(!payload.contains("pic-one"));
    assert!(!payload.contains("private-image.png"));
    assert!(!payload.contains("untrusted-event-preview.png"));
    assert!(!payload.contains("data:image"));
    request
        .reply
        .send(answer("图片暂时无法读取，可以继续文字对话"))
        .unwrap();
    assert_eq!(mock.sent().await, "图片暂时无法读取，可以继续文字对话");
    wait_for_outbox(&host, "confirmed", 1).await;
    let contents: Vec<String> = host
        .store
        .call(|db| {
            let mut statement = db.prepare("SELECT content FROM messages")?;
            Ok(statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .unwrap();
    for content in contents {
        assert!(!content.contains("pic-one"));
        assert!(!content.contains("private-image.png"));
        assert!(!content.contains("untrusted-event-preview.png"));
        assert!(!content.contains("data:image"));
    }
    within(router.shutdown()).await.unwrap();
    assert!(mock.quotes.try_recv().is_err());
    assert!(mock.quote_requests.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn vision_disabled_skips_image_lookups_and_keeps_normal_text_chat_available() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.llm.vision = false;
    })
    .await;
    router.dispatch(image_message(1, "查看图片")).await.unwrap();
    let request = mock.model().await;
    let current = request.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(current.contains("识图已关闭"));
    assert!(!current.contains("untrusted-event-preview.png"));
    assert!(mock.quotes.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    request.reply.send(answer("当前只处理文字")).unwrap();
    assert_eq!(mock.sent().await, "当前只处理文字");
    wait_for_outbox(&host, "confirmed", 1).await;

    router
        .dispatch(message(2, "继续普通文字聊天"))
        .await
        .unwrap();
    let request = mock.model().await;
    let current = current_message_payload(&request.body);
    assert_eq!(current["text"], "继续普通文字聊天");
    request.reply.send(answer("文字聊天正常")).unwrap();
    assert_eq!(mock.sent().await, "文字聊天正常");
    wait_for_outbox(&host, "confirmed", 2).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.quotes.try_recv().is_err());
    assert!(mock.quote_requests.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

fn quoted_image(body: &Value, text: &str) -> Value {
    let mut response = quoted(body, 789, text);
    let mut segments = vec![];
    if !text.is_empty() {
        segments.push(json!({"type":"text","data":{"text":text}}));
    }
    segments.push(json!({"type":"image","data":{
        "resource_id":"quoted-pic-secret",
        "temp_url":"https://qpic.cn/unused-preview?rkey=preview-secret"
    }}));
    response["data"]["message"]["segments"] = json!(segments);
    response
}

async fn quoted_image_resource(mock: &mut MockServer, seq: i64, text: &str) -> ModelRequest {
    // The quote snapshot discovers its media, and the resource lookup verifies
    // its exact physical message key again before requesting a temporary URL.
    for _ in 0..2 {
        let lookup = within(mock.quote_requests.recv()).await.unwrap();
        assert_eq!(
            lookup.body,
            json!({
                "message_scene":"group","peer_id":456,"message_seq":seq
            })
        );
        assert_eq!(within(mock.quotes.recv()).await.unwrap(), lookup.body);
        assert!(mock.resources.try_recv().is_err());
        let response = quoted_image(&lookup.body, text);
        lookup.reply.send(response).unwrap();
    }
    let resource = within(mock.resources.recv()).await.unwrap();
    assert_eq!(resource.body, json!({"resource_id":"quoted-pic-secret"}));
    resource
}

fn fail_quoted_image_download(resource: ModelRequest) {
    resource
        .reply
        .send(json!({"status":"ok","retcode":0,"data":{
            "url":"https://127.0.0.1/quoted-image.png?rkey=signed-secret"
        }}))
        .unwrap();
}

fn assert_no_quoted_image_secrets(value: &str) {
    for secret in [
        "quoted-pic-secret",
        "unused-preview",
        "preview-secret",
        "quoted-image.png",
        "signed-secret",
        "data:image",
    ] {
        assert!(
            !value.contains(secret),
            "image resource data leaked: {secret}"
        );
    }
}

#[tokio::test]
async fn vision_quote_cached_text_does_not_hide_images_and_uses_the_quoted_message_key() {
    let mut mock = MockServer::start().await;
    mock.control_quotes.store(true, Ordering::Relaxed);
    let (_temp, host, router) = setup(&mock, true).await;
    let mut original = image_message(41, "本地引用文字必须保留");
    original.mentions.clear();
    router.dispatch(original).await.unwrap();
    let mut incoming = message(42, "请结合我引用的图片和文字回答");
    incoming.quote_seq = Some(41);
    router.dispatch(incoming).await.unwrap();
    let resource = quoted_image_resource(&mut mock, 41, "远端媒体快照不能覆盖本地引用文字").await;
    fail_quoted_image_download(resource);
    let request = mock.model().await;
    let payload = request.body["messages"].to_string();
    let current = request.body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(current.contains("本地引用文字必须保留"));
    assert!(current.contains("被引用消息的图片未能读取"));
    assert!(!payload.contains("远端媒体快照不能覆盖本地引用文字"));
    assert_no_quoted_image_secrets(&payload);
    request
        .reply
        .send(answer("已保留引用文字，图片暂时无法读取"))
        .unwrap();
    mock.sent().await;
    wait_for_outbox(&host, "confirmed", 1).await;

    router.dispatch(message(43, "继续文字对话")).await.unwrap();
    let request = mock.model().await;
    assert_no_quoted_image_secrets(&request.body["messages"].to_string());
    request.reply.send(answer("继续正常回答")).unwrap();
    mock.sent().await;
    wait_for_outbox(&host, "confirmed", 2).await;
    let stored = host
        .store
        .call(|db| {
            let mut statement = db.prepare("SELECT content FROM messages")?;
            Ok(statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .unwrap();
    for content in stored {
        assert_no_quoted_image_secrets(&content);
    }
    within(router.shutdown()).await.unwrap();
    assert!(mock.quotes.try_recv().is_err());
    assert!(mock.quote_requests.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn vision_quote_image_only_remote_message_is_loaded_without_current_images() {
    let mut mock = MockServer::start().await;
    mock.control_quotes.store(true, Ordering::Relaxed);
    let (_temp, host, router) = setup(&mock, true).await;
    let mut incoming = message(1, "引用的纯图片画了什么");
    incoming.quote_seq = Some(77);
    router.dispatch(incoming).await.unwrap();
    let resource = quoted_image_resource(&mut mock, 77, "").await;
    fail_quoted_image_download(resource);
    let request = mock.model().await;
    let payload = request.body["messages"].to_string();
    assert!(payload.contains("引用的纯图片画了什么"));
    assert!(payload.contains("被引用消息的图片未能读取"));
    assert_no_quoted_image_secrets(&payload);
    request.reply.send(answer("引用图片暂时无法读取")).unwrap();
    mock.sent().await;
    wait_for_outbox(&host, "confirmed", 1).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.quotes.try_recv().is_err());
    assert!(mock.quote_requests.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn vision_quote_disabled_keeps_quote_text_without_extra_media_queries() {
    let mut mock = MockServer::start().await;
    mock.control_quotes.store(true, Ordering::Relaxed);
    let (_temp, host, router) = setup_with(&mock, true, |config| config.llm.vision = false).await;
    let mut original = image_message(41, "关闭识图仍保留引用文字");
    original.mentions.clear();
    router.dispatch(original).await.unwrap();
    let mut incoming = message(42, "引用本地消息");
    incoming.quote_seq = Some(41);
    router.dispatch(incoming).await.unwrap();
    let request = mock.model().await;
    assert!(
        request.body["messages"]
            .to_string()
            .contains("关闭识图仍保留引用文字")
    );
    assert!(mock.quotes.try_recv().is_err());
    assert!(mock.quote_requests.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    request.reply.send(answer("引用文字正常")).unwrap();
    mock.sent().await;
    wait_for_outbox(&host, "confirmed", 1).await;

    let mut incoming = message(43, "引用远端消息");
    incoming.quote_seq = Some(77);
    router.dispatch(incoming).await.unwrap();
    let lookup = within(mock.quote_requests.recv()).await.unwrap();
    assert_eq!(
        lookup.body,
        json!({"message_scene":"group","peer_id":456,"message_seq":77})
    );
    assert_eq!(within(mock.quotes.recv()).await.unwrap(), lookup.body);
    let response = quoted_image(&lookup.body, "远端引用文字");
    lookup.reply.send(response).unwrap();
    let request = mock.model().await;
    let payload = request.body["messages"].to_string();
    assert!(payload.contains("远端引用文字"));
    assert!(payload.contains("识图已关闭"));
    assert_no_quoted_image_secrets(&payload);
    request.reply.send(answer("只根据引用文字回答")).unwrap();
    mock.sent().await;
    wait_for_outbox(&host, "confirmed", 2).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.quotes.try_recv().is_err());
    assert!(mock.quote_requests.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
    assert!(mock.models.try_recv().is_err());
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn vision_quote_revoked_targets_never_trigger_remote_media_resurrection() {
    for tombstone in [true, false] {
        let mut mock = MockServer::start().await;
        mock.control_quotes.store(true, Ordering::Relaxed);
        let (_temp, host, router) = setup(&mock, true).await;
        let mut original = image_message(41, "撤销后不能再读取的引用文字");
        original.mentions.clear();
        router.dispatch(original).await.unwrap();
        host.store.call(move |db| {
            if tombstone {
                db.execute("INSERT INTO ai_revoked_sources(source_key,revoked_at) VALUES('123:group:456:41',1)", [])?;
            } else {
                db.execute("UPDATE messages SET revoked=1 WHERE key='123:group:456:41'", [])?;
            }
            Ok(())
        }).await.unwrap();
        let mut incoming = message(42, "请看这条已经撤销的引用");
        incoming.quote_seq = Some(41);
        router.dispatch(incoming).await.unwrap();
        let request = mock.model().await;
        let payload = request.body["messages"].to_string();
        assert!(payload.contains("引用已遗忘"));
        assert!(!payload.contains("撤销后不能再读取的引用文字"));
        assert_no_quoted_image_secrets(&payload);
        request.reply.send(answer("引用已经遗忘")).unwrap();
        mock.sent().await;
        wait_for_outbox(&host, "confirmed", 1).await;
        within(router.shutdown()).await.unwrap();
        assert!(mock.quotes.try_recv().is_err());
        assert!(mock.quote_requests.try_recv().is_err());
        assert!(mock.resources.try_recv().is_err());
        assert!(mock.models.try_recv().is_err());
        assert!(mock.sends.try_recv().is_err());
        drop(host);
        mock.stop().await;
    }
}

#[tokio::test]
async fn vision_quote_revoked_while_loading_images_never_reaches_the_model() {
    let mut mock = MockServer::start().await;
    mock.control_quotes.store(true, Ordering::Relaxed);
    let (_temp, host, router) = setup(&mock, true).await;
    let mut incoming = message(1, "引用图片加载期间撤销");
    incoming.quote_seq = Some(77);
    router.dispatch(incoming).await.unwrap();
    let resource = quoted_image_resource(&mut mock, 77, "加载期间即将撤销的引用文字").await;
    // Holding the resource response is a deterministic boundary: the quote's
    // nonempty image snapshot is admitted, but image loading has not finished.
    host.store.call(|db| {
        db.execute("INSERT INTO ai_revoked_sources(source_key,revoked_at) VALUES('123:group:456:77',1)", [])?;
        Ok(())
    }).await.unwrap();
    fail_quoted_image_download(resource);
    let diagnostic = mock.sent().await;
    assert!(diagnostic.starts_with("本次请求未完成："));
    assert!(diagnostic.contains("引用图片来源已失效"));
    assert_no_quoted_image_secrets(&diagnostic);
    wait_for_outbox(&host, "confirmed", 1).await;
    let assistants: i64 = host
        .store
        .call(|db| {
            Ok(db.query_row(
                "SELECT COUNT(*) FROM messages WHERE role='assistant'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(assistants, 0);
    within(router.shutdown()).await.unwrap();
    assert!(
        mock.models.try_recv().is_err(),
        "revoked image context must never be sent to the model"
    );
    assert!(mock.quotes.try_recv().is_err());
    assert!(mock.quote_requests.try_recv().is_err());
    assert!(mock.resources.try_recv().is_err());
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

async fn assert_provider_blocked_response_is_not_conversation_data(response: Value) {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup(&mock, true).await;
    let conversation = qkbot::plugins::ai::conversation(&host, &message(1, "lookup"))
        .await
        .unwrap();
    const EXISTING_SUMMARY: &str = "既有摘要：此前讨论过白茶，必须保留。";
    let conversation_id = conversation.guard.conversation_id.clone();
    host.store
        .call(move |db| {
            db.execute(
                "UPDATE ai_conversations SET summary=?1 WHERE id=?2",
                [EXISTING_SUMMARY, &conversation_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    router
        .dispatch(message(1, "first synthetic greeting"))
        .await
        .unwrap();
    let request = mock.model().await;
    assert!(
        request.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "clock_now")
    );
    request.reply.send(response).unwrap();
    let diagnostic = mock.sent().await;
    assert!(diagnostic.starts_with("本次请求未完成："), "{diagnostic}");
    assert!(!diagnostic.contains("Google"));
    assert!(!diagnostic.contains("blocked-content-must-not-be-sent"));
    wait_for_outbox(&host, "confirmed", 1).await;
    assert!(
        mock.models.try_recv().is_err(),
        "a blocked response must not start an automatic retry"
    );
    let (saved, tool_runs): ((String, String, String, i64, bool, String), i64) = host
        .store
        .call(|db| {
            Ok((
                db.query_row(
                    "SELECT role,origin_plugin,conversation_id,epoch,learnable,content FROM messages WHERE key='123:group:456:100000'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
                )?,
                db.query_row("SELECT COUNT(*) FROM tool_runs", [], |row| row.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(
        saved,
        (
            "system".into(),
            "host".into(),
            String::new(),
            0,
            false,
            diagnostic.clone()
        )
    );
    assert_eq!(
        tool_runs, 0,
        "a blocked response must execute no tool calls"
    );

    router
        .dispatch(message(2, "请解释敏感词过滤这个术语"))
        .await
        .unwrap();
    let request = mock.model().await;
    let messages = request.body["messages"].as_array().unwrap();
    assert!(messages.iter().all(|entry| entry["role"] != "assistant"));
    let prompt = request.body["messages"].to_string();
    assert!(prompt.contains("first synthetic greeting"));
    assert!(prompt.contains("请解释敏感词过滤这个术语"));
    assert!(
        prompt.contains(EXISTING_SUMMARY),
        "existing summaries must not be cleared after a provider rejection"
    );
    assert!(!prompt.contains(&diagnostic));
    assert!(!prompt.contains("The prompt could not be submitted"));
    assert!(!prompt.contains("blocked-content-must-not-be-sent"));
    const NORMAL_REPLY: &str = "敏感词过滤是一种检查文本的方式；讨论这个术语属于普通问答。";
    request.reply.send(answer(NORMAL_REPLY)).unwrap();
    assert_eq!(mock.sent().await, NORMAL_REPLY);
    wait_for_outbox(&host, "confirmed", 2).await;

    let conversation = qkbot::plugins::ai::conversation(&host, &message(2, "lookup"))
        .await
        .unwrap();
    let through = host
        .store
        .call(|db| {
            Ok(db.query_row(
                "SELECT inbound_position FROM messages WHERE key='123:group:456:2'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    let batch = host
        .memory
        .prepare_batch(&conversation.guard, through, true)
        .await
        .unwrap();
    assert_eq!(batch.previous_summary, EXISTING_SUMMARY);
    assert_eq!(
        batch.sources.len(),
        3,
        "only two user messages and the normal answer belong to the summary"
    );
    assert!(batch.sources.iter().all(|source| source.summary_eligible));
    assert!(
        batch
            .sources
            .iter()
            .all(|source| source.key != "123:group:456:100000")
    );
    let assistant_sources: Vec<_> = batch
        .sources
        .iter()
        .filter(|source| source.role == "assistant")
        .collect();
    assert_eq!(assistant_sources.len(), 1);
    assert_eq!(assistant_sources[0].text, NORMAL_REPLY);
    assert!(!assistant_sources[0].memory_eligible);

    within(router.shutdown()).await.unwrap();
    assert!(
        mock.models.try_recv().is_err(),
        "no delayed retry is allowed"
    );
    assert!(mock.sends.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn provider_blocked_content_filter_rejects_tool_calls_and_cannot_pollute_history_or_summary()
{
    assert_provider_blocked_response_is_not_conversation_data(json!({
        "model": "mock",
        "choices": [{
            "finish_reason": "content_filter",
            "message": {
                "role": "assistant",
                "content": "blocked-content-must-not-be-sent",
                "tool_calls": [{
                    "id": "blocked-clock-call",
                    "type": "function",
                    "function": {"name": "clock_now", "arguments": "{}"}
                }]
            }
        }]
    }))
    .await;
}

#[tokio::test]
async fn provider_blocked_google_error_text_cannot_pollute_history_or_summary() {
    let error = "The prompt could not be submitted. The prompt contains sensitive words that violate Google's [Generative AI Prohibited Use policy](https://policies.google.com/terms/generative-ai/use-policy). Try rephrasing the prompt. If you think this was an error, [send feedback](https://ai.google.dev/gemini-api/docs/troubleshooting).";
    assert_provider_blocked_response_is_not_conversation_data(answer(error)).await;
}

fn roll_reply_text(sent: &Value, source_seq: i64) -> &str {
    let segments = sent["message"].as_array().unwrap();
    assert_eq!(segments.len(), 2, "roll has one quote and one result only");
    assert_eq!(
        segments[0],
        json!({"type":"reply","data":{"message_seq":source_seq}})
    );
    assert_eq!(segments[1]["type"], "text");
    segments[1]["data"]["text"].as_str().unwrap()
}

#[tokio::test]
async fn roll_quotes_its_current_group_or_private_command_and_deduplicates_delivery() {
    for scene in [Scene::Group, Scene::Friend] {
        let mut mock = MockServer::start().await;
        let (_temp, host, router) = setup_with(&mock, true, |config| {
            config.plugins.native.enabled = vec!["roll".into()];
        })
        .await;
        let mut incoming = message(37, "/roll a b c");
        incoming.key.scope.scene = scene;
        incoming.key.scope.peer_id = if scene == Scene::Group { 456 } else { 789 };
        incoming.quote_seq = Some(999);
        let source_key = incoming.key.key();
        router.dispatch(incoming.clone()).await.unwrap();
        // Deduplication must work both while the command task is running and
        // after its confirmed send. A replay cannot choose a second result.
        router.dispatch(incoming.clone()).await.unwrap();
        let sent = within(mock.sends.recv()).await.unwrap();
        if scene == Scene::Group {
            assert_eq!(sent["group_id"], 456);
        } else {
            assert_eq!(sent["user_id"], 789);
        }
        let result = roll_reply_text(&sent, 37).to_owned();
        assert!(["a", "b", "c"].contains(&result.as_str()));
        wait_for_outbox(&host, "confirmed", 1).await;
        incoming.text = "/roll replay-must-not-replace-the-first-result".into();
        router.dispatch(incoming).await.unwrap();
        host.store.call(move |db| {
            let source: (String,String,i64,String) = db.query_row(
                "SELECT command_plugin,conversation_id,learnable,role FROM messages WHERE key=?1",[source_key],
                |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
            )?;
            assert_eq!(source,("roll".into(),String::new(),0,"control".into()));
            assert_eq!(db.query_row("SELECT COUNT(*) FROM outbox WHERE origin_plugin='roll' AND state='confirmed'",[],|row|row.get::<_,i64>(0))?,1);
            assert_eq!(db.query_row("SELECT COUNT(*) FROM outbox",[],|row|row.get::<_,i64>(0))?,1);
            assert_eq!(db.query_row("SELECT content FROM messages WHERE origin_plugin='roll'",[],|row|row.get::<_,String>(0))?,result);
            assert_eq!(db.query_row("SELECT COUNT(*) FROM ai_conversations",[],|row|row.get::<_,i64>(0))?,0);
            Ok(())
        }).await.unwrap();
        within(router.shutdown()).await.unwrap();
        assert!(mock.sends.try_recv().is_err());
        assert!(mock.models.try_recv().is_err());
        assert!(
            mock.quotes.try_recv().is_err(),
            "the incoming quote is not an AI trigger"
        );
        drop(host);
        mock.stop().await;
    }
}

#[tokio::test]
async fn roll_single_option_is_returned_and_empty_options_show_usage_without_ai() {
    let mut mock = MockServer::start().await;
    let (_temp, host, router) = setup_with(&mock, true, |config| {
        config.plugins.native.enabled = vec!["roll".into()];
    })
    .await;
    for (seq, command) in [(11, "/roll 唯一选项"), (12, "/roll"), (13, "/roll \t\n")] {
        let mut incoming = message(seq, command);
        incoming.quote_seq = Some(777);
        router.dispatch(incoming).await.unwrap();
        let sent = within(mock.sends.recv()).await.unwrap();
        let result = roll_reply_text(&sent, seq);
        if seq == 11 {
            assert_eq!(result, "唯一选项");
        } else {
            assert!(result.contains("用法") && result.contains("/roll") && result.contains("选项"));
        }
    }
    wait_for_outbox(&host, "confirmed", 3).await;
    within(router.shutdown()).await.unwrap();
    assert!(mock.sends.try_recv().is_err());
    assert!(mock.models.try_recv().is_err());
    assert!(mock.quotes.try_recv().is_err());
    drop(host);
    mock.stop().await;
}

#[tokio::test]
async fn roll_disabled_globally_or_in_the_chat_retains_command_ownership_without_ai() {
    for globally_disabled in [true, false] {
        let mut mock = MockServer::start().await;
        let (_temp, host, router) = setup_with(&mock, true, |config| {
            config.plugins.native.enabled = if globally_disabled {
                vec![]
            } else {
                vec!["roll".into()]
            };
        })
        .await;
        let mut incoming = message(19, "/roll a b c");
        incoming.quote_seq = Some(888);
        if !globally_disabled {
            qkbot::catalog::set_enabled(
                &host.store,
                incoming.key.scope.clone(),
                "roll".into(),
                false,
            )
            .await
            .unwrap();
        }
        let (owner, handler) = host
            .plugins
            .select(&host, &incoming)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(owner, "roll");
        assert_eq!(handler.is_none(), globally_disabled);
        router.dispatch(incoming).await.unwrap();
        if globally_disabled {
            let sent = within(mock.sends.recv()).await.unwrap();
            let segments = sent["message"].as_array().unwrap();
            assert_eq!(segments.len(), 1);
            assert_eq!(segments[0]["type"], "text");
            assert!(
                segments[0]["data"]["text"]
                    .as_str()
                    .unwrap()
                    .contains("未在当前运行配置中启用")
            );
            wait_for_outbox(&host, "confirmed", 1).await;
        }
        host.store
            .call(|db| {
                assert_eq!(
                    db.query_row("SELECT COUNT(*) FROM ai_conversations", [], |row| row
                        .get::<_, i64>(0))?,
                    0
                );
                assert_eq!(
                    db.query_row(
                        "SELECT COUNT(*) FROM outbox WHERE origin_plugin='roll'",
                        [],
                        |row| row.get::<_, i64>(0)
                    )?,
                    0
                );
                Ok(())
            })
            .await
            .unwrap();
        within(router.shutdown()).await.unwrap();
        assert!(mock.sends.try_recv().is_err());
        assert!(mock.models.try_recv().is_err());
        assert!(mock.quotes.try_recv().is_err());
        drop(host);
        mock.stop().await;
    }
}

#[tokio::test]
async fn auto_reply_waiting_for_a_model_slot_yields_its_queue_capacity_to_mentions() {
    // Exercise the per-conversation bound independently, then also exhaust the
    // global queue: cancelling observation must release both permits immediately.
    for queued_global in [4, 1] {
        let mut mock = MockServer::start().await;
        let (_temp, host, router) = setup_with(&mock, true, |config| {
            config.limits.queued_per_conversation = 1;
            config.limits.queued_global = queued_global;
            config.llm.max_concurrent_requests = 1;
        })
        .await;
        set_auto_reply(&host, &router, &message(0, "").key.scope, true).await;

        let mut occupying = message(1, "another-chat-holds-the-model-slot");
        occupying.key.scope.peer_id = 654;
        router.dispatch(occupying).await.unwrap();
        let held = mock.model().await;
        router
            .dispatch(ordinary_message(
                1,
                "automatic-observation-awaiting-capacity",
            ))
            .await
            .unwrap();
        // Let the automatic debounce expire while the first model request is
        // deliberately held. The automatic worker now owns a waiting queue slot.
        tokio::time::sleep(Duration::from_millis(1800)).await;
        tokio::task::yield_now().await;
        assert_eq!(router.chat.pending(), 1);
        assert!(mock.models.try_recv().is_err());

        router
            .dispatch(message(
                2,
                "explicit-mention-must-replace-waiting-observation",
            ))
            .await
            .unwrap();
        assert_eq!(router.chat.pending(), 1, "queued_global={queued_global}");
        held.reply.send(answer("occupying-chat-complete")).unwrap();
        assert_eq!(
            mock.sent().await,
            "occupying-chat-complete",
            "the mention must not receive queue-full feedback"
        );
        let explicit = mock.model().await;
        assert_eq!(
            current_message_payload(&explicit.body)["text"],
            "explicit-mention-must-replace-waiting-observation"
        );
        explicit
            .reply
            .send(answer("explicit-mention-complete"))
            .unwrap();
        assert_eq!(mock.sent().await, "explicit-mention-complete");
        within(router.shutdown()).await.unwrap();
        assert!(mock.models.try_recv().is_err());
        assert!(mock.sends.try_recv().is_err());
        drop(host);
        mock.stop().await;
    }
}
