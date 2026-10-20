//! Runtime activation against a local Milky peer; never connects to a QQ account.
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{
        Path, State,
        ws::{Message, WebSocketUpgrade},
    },
    response::Response,
    routing::{get, post},
};
use qkbot::{
    admin::runtime::{Runtime, RuntimeStatus},
    config::Config,
    types::now,
};
use serde_json::{Value, json};
use tokio::{
    sync::{Notify, broadcast, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const BOT: i64 = 123;
const GROUP: i64 = 456;
const ACTOR: i64 = 789;
const POKE_REPLY: &str = "native-activation-poke-reply";

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("native activation fixture timed out")
}

struct Peer {
    events: broadcast::Sender<Value>,
    sends: mpsc::Sender<Value>,
    connections: AtomicUsize,
    sequence: AtomicI64,
    hold_member: AtomicBool,
    member_waiting: Notify,
    release_member: Notify,
    cancel: CancellationToken,
}

async fn event_socket(State(peer): State<Arc<Peer>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |mut socket| async move {
        let mut events = peer.events.subscribe();
        peer.connections.fetch_add(1, Ordering::SeqCst);
        loop {
            tokio::select! {
                _ = peer.cancel.cancelled() => break,
                event = events.recv() => {
                    let Ok(event) = event else { break };
                    if socket.send(Message::Text(event.to_string().into())).await.is_err() {
                        break;
                    }
                }
                frame = socket.recv() => match frame {
                    Some(Ok(Message::Ping(bytes))) => {
                        if socket.send(Message::Pong(bytes)).await.is_err() { break; }
                    }
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    _ => {}
                }
            }
        }
    })
}

async fn milky(
    State(peer): State<Arc<Peer>>,
    Path(api): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let data = match api.as_str() {
        "get_login_info" => json!({"uin":BOT,"nickname":"fixture bot"}),
        "get_group_member_info" => {
            assert_eq!(body["group_id"], GROUP);
            let user = body["user_id"].as_i64().unwrap();
            assert!([BOT, ACTOR].contains(&user));
            if peer.hold_member.swap(false, Ordering::SeqCst) {
                peer.member_waiting.notify_one();
                tokio::select! {
                    _ = peer.release_member.notified() => {},
                    _ = peer.cancel.cancelled() => {},
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {},
                }
            }
            json!({"member":{
                "user_id":user,"group_id":GROUP,"nickname":"fixture admin",
                "card":"","role":"admin","sex":"unknown","title":"",
                "level":0,"join_time":0,"last_sent_time":0
            }})
        }
        "send_group_message" => {
            assert_eq!(body["group_id"], GROUP);
            peer.sends.send(body).await.unwrap();
            json!({"message_seq":peer.sequence.fetch_add(1,Ordering::SeqCst),"time":now()})
        }
        _ => {
            // In particular, this test never shoots, mutes, or sends a nudge back.
            panic!("unexpected Milky operation in native activation test: {api}");
        }
    };
    Json(json!({"status":"ok","retcode":0,"data":data}))
}

fn group_message(sequence: i64, text: &str) -> Value {
    json!({
        "event_type":"message_receive","self_id":BOT,"time":now(),
        "data":{
            "message_scene":"group","peer_id":GROUP,"message_seq":sequence,
            "sender_id":ACTOR,"time":now(),
            "group_member":{"user_id":ACTOR,"group_id":GROUP,"nickname":"fixture admin","card":"","role":"admin"},
            "segments":[{"type":"text","data":{"text":text}}]
        }
    })
}

fn group_nudge() -> Value {
    json!({
        "event_type":"group_nudge","self_id":BOT,"time":now(),
        "data":{"group_id":GROUP,"sender_id":ACTOR,"receiver_id":BOT}
    })
}

struct Fixture {
    _directory: tempfile::TempDir,
    config: Config,
    runtime: Arc<Runtime>,
    peer: Arc<Peer>,
    sends: mpsc::Receiver<Value>,
    cancel: CancellationToken,
    worker: Option<JoinHandle<()>>,
    server: Option<JoinHandle<()>>,
}

impl Fixture {
    async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let (events, _) = broadcast::channel(16);
        let (send_tx, sends) = mpsc::channel(16);
        let peer = Arc::new(Peer {
            events,
            sends: send_tx,
            connections: AtomicUsize::new(0),
            sequence: AtomicI64::new(100_000),
            hold_member: AtomicBool::new(false),
            member_waiting: Notify::new(),
            release_member: Notify::new(),
            cancel: CancellationToken::new(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let milky_url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/event", get(event_socket))
            .route("/api/{api}", post(milky))
            .with_state(peer.clone());
        let stop = peer.cancel.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
                .unwrap();
        });
        let mut config = Config::default();
        assert!(
            !config
                .plugins
                .native
                .enabled
                .iter()
                .any(|id| { id == "nudge" || id == "mute_roulette" })
        );
        // Keep unrelated native observers and timers out of this regression.
        config.plugins.native.enabled.clear();
        config.plugins.ai.enabled = false;
        config.plugins.clock.enabled = false;
        config.plugins.reminder.enabled = false;
        config.app.database = directory
            .path()
            .join("test.sqlite")
            .to_string_lossy()
            .into();
        config.milky.url = milky_url;
        config.milky.bot_id = Some(BOT);
        config.milky.request_timeout_secs = 3;
        config.nonebot.enabled = false;
        config.nonebot.token.clear();
        assert!(config.nonebot.plugins.iter().all(|plugin| !plugin.enabled));
        config.nonebot.media_dir = directory.path().join("media").to_string_lossy().into();
        let media_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        config.nonebot.listen = media_listener.local_addr().unwrap().to_string();
        drop(media_listener);
        let resources = directory.path().join("poke");
        std::fs::create_dir(&resources).unwrap();
        std::fs::write(resources.join("poke.txt"), POKE_REPLY).unwrap();
        config.plugins.native.poke.resource_dir = resources.to_string_lossy().into();
        config.plugins.native.poke.send_text = true;
        config.plugins.native.poke.send_image = false;
        config.plugins.native.poke.send_record = false;
        config.plugins.native.poke.send_back = false;
        let cancel = CancellationToken::new();
        let (runtime, worker) = Runtime::start(config.clone(), "initial".into(), cancel.clone());
        let fixture = Self {
            _directory: directory,
            config,
            runtime,
            peer,
            sends,
            cancel,
            worker: Some(worker),
            server: Some(server),
        };
        fixture.running("initial", 1).await;
        fixture
    }

    async fn running(&self, revision: &str, connections: usize) -> RuntimeStatus {
        within(async {
            loop {
                let status = self.runtime.status().await;
                assert_ne!(status.phase, "error", "runtime failed: {}", status.message);
                if status.phase == "running"
                    && status.applied_revision.as_deref() == Some(revision)
                    && self.peer.connections.load(Ordering::SeqCst) >= connections
                {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
    }

    fn emit(&self, event: Value) {
        assert!(self.peer.events.send(event).unwrap() > 0);
    }

    async fn sent(&mut self) -> String {
        let body = within(self.sends.recv()).await.expect("send mock closed");
        body["message"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|segment| segment["data"]["text"].as_str())
            .collect()
    }

    async fn tables(&self) -> Vec<String> {
        self.runtime
            .data()
            .await
            .unwrap()
            .call(|db| {
                Ok(db
                    .prepare(
                        "SELECT name FROM sqlite_master WHERE type='table' AND name IN \
                         ('roulette_games','roulette_actions','roulette_moderation',\
                          'roulette_amnesty_pending','nudge_events','nudge_images') ORDER BY name",
                    )?
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?)
            })
            .await
            .unwrap()
    }

    async fn stop(mut self) {
        self.cancel.cancel();
        within(self.worker.take().unwrap()).await.unwrap();
        self.peer.cancel.cancel();
        within(self.server.take().unwrap()).await.unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.peer.cancel.cancel();
        self.peer.release_member.notify_one();
    }
}

#[tokio::test]
async fn rejected_bridge_apply_preserves_runtime_then_native_activation_and_disable_are_real() {
    let mut fixture = Fixture::start().await;
    let before = fixture.runtime.status().await;
    assert_eq!(before.loaded_plugins, ["host"]);
    assert!(fixture.tables().await.is_empty());

    // This is the saved-config failure: no external plugin is selected, but the
    // bridge itself is on without its independent credential. Native switches
    // must not be reported as loaded when the apply preflight rejects it.
    let mut enabled = fixture.config.clone();
    enabled.plugins.native.enabled = vec!["nudge".into(), "mute_roulette".into()];
    enabled.nonebot.enabled = true;
    enabled.validate().unwrap(); // Saving this draft is permitted.
    let error = fixture
        .runtime
        .apply(enabled.clone(), "invalid-bridge".into())
        .unwrap_err();
    assert!(error.to_string().contains("NoneBot"));
    let unchanged = fixture.running("initial", 1).await;
    assert_eq!(unchanged.runtime_generation, before.runtime_generation);
    assert_eq!(unchanged.loaded_plugins, ["host"]);
    assert!(fixture.tables().await.is_empty());
    assert!(
        fixture
            .runtime
            .config()
            .await
            .plugins
            .native
            .enabled
            .is_empty()
    );

    enabled.nonebot.enabled = false;
    fixture
        .runtime
        .apply(enabled.clone(), "native-enabled".into())
        .unwrap();
    let status = fixture.running("native-enabled", 2).await;
    assert_eq!(status.loaded_plugins, ["host", "mute_roulette", "nudge"]);
    assert_eq!(
        fixture.runtime.loaded_plugins().await,
        status.loaded_plugins
    );
    assert_eq!(
        fixture.tables().await.len(),
        6,
        "both plugin init hooks ran"
    );

    let start = group_message(1, "自由轮盘");
    fixture.emit(start.clone());
    assert!(fixture.sent().await.contains("游戏开始！"));
    let poke = group_nudge();
    fixture.emit(poke.clone());
    assert_eq!(fixture.sent().await, POKE_REPLY);

    // Hold an already admitted handler at a real member lookup. Applying a
    // disabled configuration cancels the host before this late answer arrives.
    fixture.peer.hold_member.store(true, Ordering::SeqCst);
    fixture.emit(group_message(2, "开启自由轮盘"));
    within(fixture.peer.member_waiting.notified()).await;
    let mut disabled = enabled;
    disabled.plugins.native.enabled.clear();
    fixture
        .runtime
        .apply(disabled, "native-disabled".into())
        .unwrap();
    within(async {
        loop {
            let status = fixture.runtime.status().await;
            if status.phase == "stopping" && status.loaded_plugins.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    fixture.peer.release_member.notify_one();
    let status = fixture.running("native-disabled", 3).await;
    assert_eq!(status.loaded_plugins, ["host"]);
    assert!(
        fixture.sends.try_recv().is_err(),
        "late handler must not send"
    );

    // Protocol replay after restart cannot re-run the previous successful work.
    fixture.emit(start);
    fixture.emit(poke);
    fixture.emit(group_message(3, "自由轮盘"));
    let disabled_reply = fixture.sent().await;
    assert!(!disabled_reply.contains("游戏开始！"));
    assert!(!disabled_reply.contains("自由轮盘已开启"));
    assert!(!disabled_reply.contains(POKE_REPLY));
    let counts = fixture
        .runtime
        .data()
        .await
        .unwrap()
        .call(|db| {
            Ok((
                db.query_row("SELECT COUNT(*) FROM nudge_events", [], |row| {
                    row.get::<_, i64>(0)
                })?,
                db.query_row(
                    "SELECT COUNT(*) FROM outbox WHERE origin_plugin IN ('mute_roulette','nudge') AND state='confirmed'",
                    [],
                    |row| row.get::<_, i64>(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(
        counts,
        (1, 2),
        "only the two original native replies committed"
    );
    assert_eq!(
        fixture.tables().await.len(),
        6,
        "disable preserves plugin data"
    );
    fixture.stop().await;
}
