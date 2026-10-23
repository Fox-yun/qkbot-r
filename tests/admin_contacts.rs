//! Contact switches work independently of model setup and remain account scoped.
use std::{
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use axum::{Json, Router, extract::State, routing::post};
use qkbot::{
    access::AccessStore,
    admin::{self, AdminState, auth::AuthService, runtime::Runtime, settings::ConfigStore},
    config::Config,
    store::Store,
    types::{Scene, Scope},
};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const SETUP: &str = "contacts-integration-setup-code-long-enough";

#[tokio::test]
async fn contact_http_authentication_defaults_isolation_and_persistence() {
    let account = Arc::new(AtomicI64::new(101));
    let upstream = Router::new()
        .route("/api/get_login_info", post(|State(id): State<Arc<AtomicI64>>| async move {
            Json(json!({"status":"ok","retcode":0,"data":{"uin":id.load(Ordering::SeqCst)}}))
        }))
        .route("/api/get_group_list", post(|| async {
            Json(json!({"status":"ok","retcode":0,"data":{"groups":[{"group_id":202,"group_name":"测试群","remark":"群备注"}]}}))
        }))
        .route("/api/get_friend_list", post(|| async {
            Json(json!({"status":"ok","retcode":0,"data":{"friends":[{"user_id":202,"nickname":"好友","remark":"好友备注"}]}}))
        }))
        .with_state(account.clone());
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", upstream_listener.local_addr().unwrap());
    let upstream_cancel = CancellationToken::new();
    let upstream_stop = upstream_cancel.clone();
    let upstream_task = tokio::spawn(async move {
        axum::serve(upstream_listener, upstream)
            .with_graceful_shutdown(upstream_stop.cancelled_owned())
            .await
            .unwrap();
    });

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let database = directory.path().join("bot.sqlite3");
    let mut config = Config::default();
    config.app.database = database.to_string_lossy().into_owned();
    config.web.token = SETUP.into();
    config.milky.url = upstream_url;
    config.milky.request_timeout_secs = 1;
    // The default model is unconfigured; contact management must still work.
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let settings = Arc::new(ConfigStore::open(&path).await.unwrap());
    let (config, revision) = settings.effective_with_revision().await.unwrap();
    let cancel = CancellationToken::new();
    let (runtime, worker) = Runtime::start(config, revision, cancel.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        while runtime.data().await.is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let auth = AuthService::open(&path, SETUP, false).await.unwrap();
    let state = AdminState::new(settings, runtime, auth);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server_stop = cancel.clone();
    let server = tokio::spawn(async move {
        admin::serve(listener, state, server_stop).await.unwrap();
    });
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let list_url = format!("{url}/api/qq/contacts");
    let toggle_url = format!("{url}/api/qq/contacts/toggle");
    let auto_reply_url = format!("{url}/api/qq/contacts/auto-reply");

    assert_eq!(
        client.get(&list_url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let setup = client
        .post(format!("{url}/api/auth/setup"))
        .json(&json!({"code":SETUP,"password":"contact fixture password"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let cookie = setup.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf = setup.json::<Value>().await.unwrap()["csrf_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let get_list = || client.get(&list_url).header("cookie", &cookie);
    let toggle = |bot: &str, scene: &str, peer: &str, enabled: bool| {
        client
            .post(&toggle_url)
            .header("cookie", &cookie)
            .header("x-csrf-token", &csrf)
            .json(&json!({"bot_id":bot,"scene":scene,"peer_id":peer,"enabled":enabled}))
    };
    let auto_reply = |bot: &str, scene: &str, peer: &str, enabled: bool| {
        client
            .post(&auto_reply_url)
            .header("cookie", &cookie)
            .header("x-csrf-token", &csrf)
            .json(&json!({"bot_id":bot,"scene":scene,"peer_id":peer,"enabled":enabled}))
    };

    let list: Value = get_list()
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["bot_id"], "101");
    assert_eq!(list["groups"][0]["id"], "202");
    assert_eq!(list["groups"][0]["enabled"], true);
    assert_eq!(list["friends"][0]["enabled"], true);
    assert_eq!(list["groups"][0]["auto_reply_enabled"], false);
    assert_eq!(list["friends"][0]["auto_reply_enabled"], false);
    assert_eq!(
        client
            .post(&auto_reply_url)
            .json(&json!({"bot_id":"101","scene":"group","peer_id":"202","enabled":true}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(&auto_reply_url)
            .header("cookie", &cookie)
            .json(&json!({"bot_id":"101","scene":"group","peer_id":"202","enabled":true}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert!(
        auto_reply("101", "group", "202", true)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let list: Value = get_list().send().await.unwrap().json().await.unwrap();
    assert_eq!(list["groups"][0]["auto_reply_enabled"], true);
    assert_eq!(list["friends"][0]["auto_reply_enabled"], false);
    assert_eq!(list["groups"][0]["enabled"], true);
    assert_eq!(
        auto_reply("101", "group", "999", true)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        auto_reply("303", "group", "202", true)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert!(
        auto_reply("101", "friend", "202", true)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(
        client
            .post(&toggle_url)
            .header("cookie", &cookie)
            .json(&json!({"bot_id":"101","scene":"group","peer_id":"202","enabled":false}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );

    let changed: Value = toggle("101", "group", "202", false)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(changed["enabled"], false);
    let list: Value = get_list().send().await.unwrap().json().await.unwrap();
    assert_eq!(list["groups"][0]["enabled"], false);
    assert_eq!(
        list["friends"][0]["enabled"], true,
        "same number in private chat is independent"
    );
    assert!(
        toggle("101", "group", "999", true)
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
    );
    assert!(
        toggle("101", "group", "0", false)
            .send()
            .await
            .unwrap()
            .status()
            .is_client_error()
    );
    assert_eq!(
        toggle("303", "group", "202", true)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );

    account.store(303, Ordering::SeqCst);
    assert_eq!(
        toggle("101", "group", "202", true)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let list: Value = get_list()
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["bot_id"], "303");
    assert_eq!(list["groups"][0]["enabled"], true);
    assert_eq!(list["groups"][0]["auto_reply_enabled"], false);
    assert_eq!(list["friends"][0]["auto_reply_enabled"], false);
    account.store(101, Ordering::SeqCst);
    let list: Value = get_list()
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["groups"][0]["enabled"], false);

    account.store(0, Ordering::SeqCst);
    assert!(
        auto_reply("101", "group", "202", false)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert!(
        !auto_reply("101", "group", "202", true)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert!(
        toggle("101", "group", "202", false)
            .send()
            .await
            .unwrap()
            .status()
            .is_success(),
        "an authenticated administrator can keep a bound scope disabled while offline"
    );
    assert!(
        !toggle("101", "group", "202", true)
            .send()
            .await
            .unwrap()
            .status()
            .is_success(),
        "enabling still requires verified membership and account"
    );

    cancel.cancel();
    server.await.unwrap();
    worker.await.unwrap();
    upstream_cancel.cancel();
    upstream_task.await.unwrap();
    let reopened = Store::open(&database, 16).await.unwrap();
    let access = AccessStore::new(reopened.clone());
    assert!(
        !access
            .auto_reply_enabled(&Scope {
                bot_id: 101,
                scene: Scene::Group,
                peer_id: 202
            })
            .await
            .unwrap()
    );
    assert!(
        access
            .auto_reply_enabled(&Scope {
                bot_id: 101,
                scene: Scene::Friend,
                peer_id: 202
            })
            .await
            .unwrap()
    );
    assert!(
        !access
            .auto_reply_enabled(&Scope {
                bot_id: 303,
                scene: Scene::Friend,
                peer_id: 202
            })
            .await
            .unwrap()
    );
    assert!(
        !access
            .enabled(&Scope {
                bot_id: 101,
                scene: Scene::Group,
                peer_id: 202
            })
            .await
            .unwrap()
    );
    assert!(
        access
            .enabled(&Scope {
                bot_id: 303,
                scene: Scene::Group,
                peer_id: 202
            })
            .await
            .unwrap()
    );
    reopened.stop().await.unwrap();
}
