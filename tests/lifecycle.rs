//! Regression coverage for shared-store shutdown and guarded model admission.
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{Json, routing::post};
use qkbot::{
    budget::Budget,
    config::{Config, Limits},
    host::{Host, Router},
    llm::{Llm, LlmConfig},
    store::Store,
    types::ContextGuard,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("lifecycle regression timed out")
}

#[tokio::test]
async fn shared_store_shutdown_waits_for_a_full_queue_and_can_start_again() {
    const CAPACITY: usize = 8;
    let store = Store::open(":memory:", CAPACITY).await.unwrap();
    let mut config = Config::from_toml(include_str!("../config.example.toml")).unwrap();
    config.app.database = ":memory:".into();
    config.limits.database_queue = CAPACITY;
    config.plugins.ai.enabled = false;
    config.milky.url = "http://127.0.0.1:1".into();
    let host = Host::create_with_store(config.clone(), store.clone())
        .await
        .unwrap();
    let initial_epoch = host.epochs["clock"];
    store
        .call(move |db| {
            db.execute_batch("CREATE TABLE retained_data(value INTEGER NOT NULL);")?;
            db.execute(
                "INSERT INTO outbox(id,scope,origin_plugin,plugin_epoch,body,state,created_at,updated_at) VALUES('pending','1:group:2','clock',?1,'hello','prepared',1,1)",
                [initial_epoch],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let router = Router::new(host);

    // Hold the actual SQLite worker, then enqueue exactly its capacity. Polling
    // each future once makes the full-queue condition deterministic without sleeps.
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let blocking_store = store.clone();
    let blocker = tokio::spawn(async move {
        blocking_store
            .call(move |_| {
                let _ = entered.send(());
                blocked.recv_timeout(Duration::from_secs(10))?;
                Ok(())
            })
            .await
    });
    within(started).await.unwrap();
    let mut queued = Vec::new();
    for value in 0..CAPACITY {
        let mut operation = Box::pin(store.call(move |db| {
            db.execute(
                "INSERT INTO retained_data(value) VALUES(?1)",
                [value as i64],
            )?;
            Ok(())
        }));
        assert!(futures_util::poll!(&mut operation).is_pending());
        queued.push(operation);
    }
    assert!(store.call(|_| Ok(())).await.is_err(), "queue must be full");

    let mut shutdown = Box::pin(router.shutdown());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), shutdown.as_mut())
            .await
            .is_err(),
        "shutdown must wait for capacity instead of treating backpressure as failure"
    );
    release.send(()).unwrap();
    within(blocker).await.unwrap().unwrap();
    for operation in queued {
        within(operation).await.unwrap();
    }
    within(shutdown).await.unwrap();

    let (rows, state, enabled): (i64, String, i64) = store
        .call(|db| {
            Ok((
                db.query_row("SELECT count(*) FROM retained_data", [], |r| r.get(0))?,
                db.query_row("SELECT state FROM outbox WHERE id='pending'", [], |r| {
                    r.get(0)
                })?,
                db.query_row(
                    "SELECT count(*) FROM plugin_states WHERE enabled=1",
                    [],
                    |r| r.get(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(rows, CAPACITY as i64);
    assert_eq!(state, "failed");
    assert_eq!(enabled, 0);

    // The shared service remains alive after stop and supports a fresh runtime.
    let restarted = Host::create_with_store(config, store.clone())
        .await
        .unwrap();
    assert!(restarted.epochs["clock"] > initial_epoch);
    within(Router::new(restarted).shutdown()).await.unwrap();
    assert_eq!(
        store
            .call(
                |db| Ok(db.query_row("SELECT count(*) FROM retained_data", [], |r| r
                    .get::<_, i64>(0))?)
            )
            .await
            .unwrap(),
        CAPACITY as i64
    );
    store.stop().await.unwrap();
}

#[tokio::test]
async fn stale_background_context_or_plugin_never_reaches_http_or_spends_quota() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let requests = Arc::new(AtomicUsize::new(0));
    let hits = requests.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "model":"mock",
                    "choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"summary"}}],
                    "usage":{"prompt_tokens":4,"completion_tokens":1}
                }))
            }
        }),
    );
    let stop = CancellationToken::new();
    let server_stop = stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(server_stop.cancelled_owned())
            .await
            .unwrap();
    });

    let store = Store::open(":memory:", 8).await.unwrap();
    store.activate_plugins(vec!["ai".into()]).await.unwrap();
    let old_plugin_epoch = store.epoch("ai").await.unwrap();
    store
        .call(|db| {
            db.execute(
                "INSERT INTO ai_conversations(id,scope,mode) VALUES('c','1:group:2','group')",
                [],
            )?;
            // A prepared background batch held epoch 1 before this reset committed.
            db.execute("UPDATE ai_conversations SET epoch=epoch+1 WHERE id='c'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let budget = Budget::new(
        store.clone(),
        Limits {
            memory_requests_per_hour: 1,
            ..Default::default()
        },
    );
    budget.init().await.unwrap();
    let llm = Llm::new(
        LlmConfig {
            chat_completions_url: endpoint,
            model: "mock".into(),
            request_timeout_secs: 2,
            ..Default::default()
        },
        reqwest::Client::builder().no_proxy().build().unwrap(),
    )
    .unwrap()
    .with_budget(budget.clone());
    let cancel = CancellationToken::new();
    let input = vec![json!({"role":"user","content":"old private batch"})];
    let old_guard = ContextGuard {
        conversation_id: "c".into(),
        epoch: 1,
    };
    assert!(
        within(llm.complete_background_admitted(
            input.clone(),
            32,
            &cancel,
            old_guard,
            old_plugin_epoch
        ))
        .await
        .is_err()
    );

    let current_guard = ContextGuard {
        conversation_id: "c".into(),
        epoch: 2,
    };
    store.activate_plugins(vec!["ai".into()]).await.unwrap();
    let current_plugin_epoch = store.epoch("ai").await.unwrap();
    assert!(
        within(llm.complete_background_admitted(
            input.clone(),
            32,
            &cancel,
            current_guard.clone(),
            old_plugin_epoch
        ))
        .await
        .is_err()
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    let usage = budget.snapshot().await.unwrap();
    assert_eq!(usage["used"]["foreground"], 0);
    assert_eq!(usage["used"]["background"], 0);

    assert!(
        within(llm.complete_background_admitted(
            input.clone(),
            32,
            &cancel,
            current_guard.clone(),
            current_plugin_epoch
        ))
        .await
        .unwrap()
        .is_some()
    );
    let usage = budget.snapshot().await.unwrap();
    assert_eq!(usage["used"]["foreground"], 0);
    assert_eq!(usage["used"]["background"], 1);
    assert!(
        within(llm.complete_background_admitted(
            input,
            32,
            &cancel,
            current_guard,
            current_plugin_epoch
        ))
        .await
        .is_err(),
        "the successful current-generation request must consume the separate background limit"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    stop.cancel();
    within(server).await.unwrap();
    store.stop().await.unwrap();
}
