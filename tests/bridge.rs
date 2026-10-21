//! Actual HTTP + WebSocket gateway, with a local Milky peer; no QQ account used.
use axum::{
    Json, Router as HttpRouter,
    extract::{Path, State},
    routing::post,
};
use futures_util::{SinkExt, StreamExt};
use qkbot::{
    config::{Config, ExternalPluginConfig},
    host::{Host, Router},
    types::{Incoming, MessageKey, Scene, Scope, now},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "bridge-test-only-credential-32-characters";
#[derive(Default)]
struct Peer {
    writes: AtomicUsize,
    models: AtomicUsize,
    uncertain: AtomicBool,
}
async fn milky(
    State(peer): State<Arc<Peer>>,
    Path(api): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let data = match api.as_str() {
        "model" => {
            peer.models.fetch_add(1, Ordering::SeqCst);
            return Json(json!({"error":"unexpected model request"}));
        }
        "get_login_info" => json!({"uin":123,"nickname":"fixture"}),
        "get_impl_info" => {
            json!({"impl_name":"test","impl_version":"1","qq_protocol_version":"1","qq_protocol_type":"windows","milky_version":"1.0"})
        }
        "get_message" => {
            json!({"message":raw(body["peer_id"].as_i64().unwrap(),body["message_seq"].as_i64().unwrap(),"quoted")})
        }
        "send_group_message" | "send_private_message" => {
            let seq = peer.writes.fetch_add(1, Ordering::SeqCst) + 100;
            if peer.uncertain.load(Ordering::SeqCst) {
                return Json(json!({"not_an_acknowledgement":true}));
            }
            json!({"message_seq":seq,"time":now()})
        }
        "send_group_message_reaction" => {
            peer.writes.fetch_add(1, Ordering::SeqCst);
            json!({})
        }
        "upload_group_file" | "upload_private_file" => {
            peer.writes.fetch_add(1, Ordering::SeqCst);
            json!({"file_id":"fixture-file"})
        }
        _ => {
            return Json(json!({"status":"failed","retcode":404,"message":"unsupported test API"}));
        }
    };
    Json(json!({"status":"ok","retcode":0,"data":data}))
}
fn raw(peer: i64, seq: i64, text: &str) -> Value {
    json!({"message_scene":"group","peer_id":peer,"message_seq":seq,"sender_id":789,"time":now(),
        "group":{"group_id":peer,"group_name":"fixture","member_count":3,"max_member_count":10},
        "group_member":{"user_id":789,"group_id":peer,"nickname":"fixture","card":"","sex":"unknown","title":"","level":0,"role":"member","join_time":0,"last_sent_time":0},
        "segments":[{"type":"text","data":{"text":text}}]})
}
fn incoming(seq: i64, text: &str) -> Incoming {
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
        mentions: vec![],
        quote_seq: None,
        received_at: now(),
        raw_message: raw(456, seq, text),
        notification: None,
    }
}
struct Fixture {
    _directory: tempfile::TempDir,
    host: Arc<Host>,
    router: Router,
    client: reqwest::Client,
    base: String,
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    peer: Arc<Peer>,
    cancel: CancellationToken,
    peer_task: tokio::task::JoinHandle<()>,
    bridge_task: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_ai(false).await
    }
    async fn with_ai(enable_ai: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let peer = Arc::new(Peer::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_url = format!("http://{}", listener.local_addr().unwrap());
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let app = HttpRouter::new()
            .route("/api/{api}", post(milky))
            .with_state(peer.clone());
        let peer_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let base = format!("http://{address}");
        let mut config = Config::default();
        config.plugins.ai.enabled = enable_ai;
        config.llm.chat_completions_url = format!("{peer_url}/api/model");
        config.llm.model = "mock".into();
        config.plugins.native.enabled.clear();
        config.app.database = directory
            .path()
            .join("test.sqlite")
            .to_string_lossy()
            .into();
        config.milky.url = peer_url;
        config.milky.bot_id = Some(123);
        config.nonebot.enabled = true;
        config.nonebot.token = TOKEN.into();
        config.nonebot.listen = address;
        config.nonebot.media_dir = directory.path().join("media").to_string_lossy().into();
        config.nonebot.plugins = vec![ExternalPluginConfig {
            enabled: true,
            ..Default::default()
        }];
        let host = Host::create(config).await.unwrap();
        host.milky.login().await.unwrap();
        let worker = host.clone();
        let bridge_task = tokio::spawn(async move {
            qkbot::bridge::serve(worker, listener).await.unwrap();
        });
        let mut request = (base.replace("http:", "ws:") + "/event")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        Self {
            _directory: directory,
            router: Router::new(host.clone()),
            host,
            client: reqwest::Client::new(),
            base,
            socket,
            peer,
            cancel,
            peer_task,
            bridge_task,
        }
    }
    async fn event(&mut self, seq: i64, text: &str) -> Value {
        self.event_message(incoming(seq, text)).await
    }
    async fn event_message(&mut self, message: Incoming) -> Value {
        self.router.dispatch(message).await.unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(3), self.socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(frame.to_text().unwrap()).unwrap()
    }
    async fn api(&self, name: &str, body: Value) -> Value {
        self.client
            .post(format!("{}/api/{name}", self.base))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn send(&self, event: &Value, id: &str, peer: i64) -> Value {
        let mut identity = event["qkbot"].clone();
        identity["operation_id"] = json!(id);
        self.api("send_group_message",json!({"group_id":peer,"message":[{"type":"text","data":{"text":"fixture response"}}],"qkbot":identity})).await
    }
    async fn stop(mut self) {
        let _ = self.socket.send(Message::Close(None)).await;
        drop(self.socket);
        self.host.cancel.cancel();
        self.bridge_task.await.unwrap();
        self.router.shutdown().await.unwrap();
        self.cancel.cancel();
        self.peer_task.await.unwrap();
    }
}
#[tokio::test]
async fn task_scope_idempotency_and_no_ai_learning() {
    let mut f = Fixture::new().await;
    let event = f.event(1, "/nbtest hello").await;
    assert_eq!(event["data"]["group_member"]["role"], "member");
    assert_eq!(f.send(&event, "same", 999).await["status"], "failed");
    assert_eq!(f.send(&event, "same", 456).await["status"], "ok");
    assert_eq!(f.send(&event, "same", 456).await["status"], "ok");
    assert_eq!(f.peer.writes.load(Ordering::SeqCst), 1);
    assert_eq!(f.send(&event, "different-call", 456).await["status"], "ok");
    assert_eq!(f.peer.writes.load(Ordering::SeqCst), 2);
    f.host
        .store
        .call(|db| {
            let (learn, owner): (i64, String) = db.query_row(
                "SELECT learnable,command_plugin FROM messages WHERE key='123:group:456:1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!(learn, 0);
            assert_eq!(owner, "nb_echo");
            assert_eq!(
                db.query_row("SELECT COUNT(*) FROM ai_conversations", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            assert_eq!(
                db.query_row(
                    "SELECT COUNT(*) FROM messages WHERE role='assistant'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
    f.stop().await;
}
#[tokio::test]
async fn lost_ack_is_never_retried_and_toggle_invalidates_old_task() {
    let mut f = Fixture::new().await;
    let event = f.event(1, "/nbtest").await;
    f.peer.uncertain.store(true, Ordering::SeqCst);
    assert_eq!(f.send(&event, "once", 456).await["status"], "failed");
    assert_eq!(f.send(&event, "once", 456).await["status"], "failed");
    assert_eq!(f.peer.writes.load(Ordering::SeqCst), 1);
    qkbot::catalog::set_enabled(
        &f.host.store,
        incoming(1, "").key.scope.clone(),
        "nb_echo".into(),
        false,
    )
    .await
    .unwrap();
    qkbot::catalog::set_enabled(
        &f.host.store,
        incoming(1, "").key.scope,
        "nb_echo".into(),
        true,
    )
    .await
    .unwrap();
    assert_eq!(f.send(&event, "new", 456).await["status"], "failed");
    assert_eq!(f.peer.writes.load(Ordering::SeqCst), 1);
    f.stop().await;
}
#[tokio::test]
async fn api_shape_media_upload_and_side_effect_ledger() {
    let mut f = Fixture::new().await;
    let event = f.event(1, "/nbtest image").await;
    let identity = event["qkbot"].clone();
    for mime in [
        "audio/mp4",
        "audio/aac",
        "audio/flac",
        "audio/amr",
        "audio/silk",
    ] {
        let uploaded = f
            .client
            .post(format!("{}/qkbot/media", f.base))
            .bearer_auth(TOKEN)
            .header("X-Qkbot-Task", identity["task_id"].as_str().unwrap())
            .header("X-Qkbot-Plugin", "nb_echo")
            .header(
                "X-Qkbot-Generation",
                identity["generation"].as_str().unwrap(),
            )
            .header("Content-Type", mime)
            .body("small-fixture-audio")
            .send()
            .await
            .unwrap();
        assert!(
            uploaded.status().is_success(),
            "{mime} must preserve upstream audio types"
        );
    }
    let quote = f
        .api(
            "get_message",
            json!({"message_scene":"group","peer_id":456,"message_seq":2,"qkbot":identity}),
        )
        .await;
    assert_eq!(quote["data"]["message"]["message_seq"], 2);
    let upload = f
        .client
        .post(format!("{}/qkbot/media", f.base))
        .bearer_auth(TOKEN)
        .header("X-Qkbot-Task", identity["task_id"].as_str().unwrap())
        .header("X-Qkbot-Plugin", "nb_echo")
        .header(
            "X-Qkbot-Generation",
            identity["generation"].as_str().unwrap(),
        )
        .header("Content-Type", "application/octet-stream")
        .body("fixture-file")
        .send()
        .await
        .unwrap();
    assert!(upload.status().is_success());
    let upload: Value = upload.json().await.unwrap();
    assert_eq!(
        f.client
            .get(upload["url"].as_str().unwrap())
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
        "fixture-file"
    );
    let mut call = identity;
    call["operation_id"] = json!("upload-once");
    let body =
        json!({"group_id":456,"file_name":"fixture.txt","file_uri":upload["url"],"qkbot":call});
    assert_eq!(
        f.api("upload_group_file", body.clone()).await["data"]["file_id"],
        "fixture-file"
    );
    assert_eq!(
        f.api("upload_group_file", body).await["data"]["file_id"],
        "fixture-file"
    );
    assert_eq!(f.peer.writes.load(Ordering::SeqCst), 1);
    let other = f.event(2, "/nbtest image").await;
    let mut other_id = other["qkbot"].clone();
    other_id["operation_id"] = json!("steal-media");
    let denied=f.api("send_group_message",json!({"group_id":456,"message":[{"type":"image","data":{"uri":upload["url"]}}],"qkbot":other_id})).await;
    assert_eq!(
        denied["status"], "failed",
        "an upload belongs to one task, even in the same chat"
    );
    f.host
        .access
        .set_enabled(incoming(1, "").key.scope, false)
        .await
        .unwrap();
    assert_eq!(
        f.client
            .get(upload["url"].as_str().unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    f.stop().await;
}
#[tokio::test]
async fn native_media_requires_send_binding_and_dies_on_plugin_revision_change() {
    use base64::Engine;
    let mut f = Fixture::new().await;
    f.event(1, "/nbtest").await;
    let bytes=base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6lVQAAAAASUVORK5CYII=").unwrap();
    let message = f.host.media.png(&f.host, bytes).await.unwrap();
    let uri = serde_json::to_value(&message).unwrap()["segments"][0]["data"]["uri"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(f.client.get(&uri).send().await.unwrap().status(), 404);
    f.host
        .send_plugin_reply(
            &incoming(1, "/nbtest"),
            "nb_echo",
            message,
            "native-image-fixture".into(),
        )
        .await
        .unwrap();
    assert_eq!(f.client.get(&uri).send().await.unwrap().status(), 200);
    for enabled in [false, true] {
        qkbot::catalog::set_enabled(
            &f.host.store,
            incoming(1, "").key.scope,
            "nb_echo".into(),
            enabled,
        )
        .await
        .unwrap();
        assert_eq!(f.client.get(&uri).send().await.unwrap().status(), 404);
    }
    f.stop().await;
}
#[tokio::test]
async fn bearer_only_and_short_lease_ownership() {
    let mut f = Fixture::with_ai(true).await;
    assert_eq!(
        f.client
            .get(format!("{}/qkbot/catalog?access_token={TOKEN}", f.base))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let event = f.event(1, "/nbtest prompt").await;
    let mut lease = event["qkbot"].clone();
    lease["seconds"] = json!(30);
    assert!(
        f.client
            .post(format!("{}/qkbot/lease", f.base))
            .bearer_auth(TOKEN)
            .json(&lease)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let sent = f.send(&event, "quoted-prompt", 456).await;
    let mut response = incoming(2, "ordinary answer");
    response.quote_seq = Some(sent["data"]["message_seq"].as_i64().unwrap());
    let follow = f.event_message(response).await;
    assert_eq!(follow["qkbot"]["plugin_id"], "nb_echo");
    assert_eq!(f.peer.models.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.host
            .store
            .call(|db| Ok(
                db.query_row("SELECT COUNT(*) FROM ai_conversations", [], |r| r
                    .get::<_, i64>(0))?
            ))
            .await
            .unwrap(),
        0
    );
    lease = follow["qkbot"].clone();
    lease["seconds"] = json!(0);
    assert!(
        f.client
            .post(format!("{}/qkbot/lease", f.base))
            .bearer_auth(TOKEN)
            .json(&lease)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(
        f.host
            .bridge
            .selected(&f.host, &incoming(3, "ordinary answer"))
            .await
            .unwrap(),
        None
    );
    f.stop().await;
}
