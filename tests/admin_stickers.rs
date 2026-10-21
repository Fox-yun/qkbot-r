//! Real HTTP against a stopped bot and a temporary saved sticker library.
use std::{path::PathBuf, sync::Arc, time::Duration};

use qkbot::{
    admin::{self, AdminState, auth::AuthService, runtime::Runtime, settings::ConfigStore},
    config::Config,
    embedding::{Embedding, encode_vector},
    store::Store,
    types::now,
};
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use rusqlite::params;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const SETUP: &str = "sticker-library-synthetic-setup-code-long-enough";
const SCOPE: &str = "123:group:456";
const OTHER_SCOPE: &str = "123:group:457";

struct Panel {
    directory: tempfile::TempDir,
    database: PathBuf,
    url: String,
    client: Client,
    cookie: String,
    csrf: String,
    store: Store,
    cancel: CancellationToken,
    worker: Option<JoinHandle<()>>,
    server: Option<JoinHandle<()>>,
}

impl Panel {
    async fn start() -> Self {
        Self::start_with_relative_database(false).await
    }

    async fn start_with_relative_database(relative: bool) -> Self {
        let cwd = std::env::current_dir().unwrap();
        let directory = if relative {
            std::fs::create_dir_all(cwd.join("target")).unwrap();
            tempfile::tempdir_in(cwd.join("target")).unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let database = directory.path().join("stickers.sqlite3");
        let path = directory.path().join("config.toml");
        let mut config = Config::default();
        config.app.database = database.to_string_lossy().into_owned();
        config.web.token = SETUP.into();
        config.embedding.dimensions = 64;
        config.embedding.api_key = "never-expose-synthetic-embedding-key".into();
        // Unconfigured LLM prevents the bot from starting or making QQ requests.
        // The shared database and library administration must still initialize.
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        let settings = Arc::new(ConfigStore::open(&path).await.unwrap());
        let (mut config, revision) = settings.effective_with_revision().await.unwrap();
        if relative {
            // Runtime also accepts an already-loaded relative database path;
            // it must resolve exactly as Store::open does, from the process cwd.
            config.app.database = database
                .strip_prefix(&cwd)
                .unwrap()
                .to_string_lossy()
                .into_owned();
        }
        let cancel = CancellationToken::new();
        let (runtime, worker) = Runtime::start(config, revision, cancel.clone());
        let store = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(store) = runtime.data().await {
                    break store;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let auth = AuthService::open(&path, SETUP, false).await.unwrap();
        let state = AdminState::new(settings, runtime.clone(), auth);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = cancel.clone();
        let server = tokio::spawn(async move {
            admin::serve(listener, state, stop).await.unwrap();
        });
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let setup = client
            .post(format!("{url}/api/auth/setup"))
            .json(&json!({"code":SETUP,"password":"synthetic sticker admin password"}))
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
        let panel = Self {
            directory,
            database,
            url,
            client,
            cookie,
            csrf,
            store,
            cancel,
            worker: Some(worker),
            server: Some(server),
        };
        panel
            .request(Method::POST, "/api/runtime/stop")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while runtime.status().await.phase != "stopped" {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        panel
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .header("x-csrf-token", &self.csrf)
    }

    async fn get(&self, path: &str) -> Value {
        let response = self.request(Method::GET, path).send().await.unwrap();
        if response.status() != StatusCode::OK {
            panic!(
                "{path}: {} {}",
                response.status(),
                response.text().await.unwrap_or_default()
            );
        }
        response.json().await.unwrap()
    }

    async fn seed(&self, scope: &str, count: usize) -> Vec<i64> {
        let scope = scope.to_owned();
        let conversation = format!("{scope}:v0:group");
        let mut embedding = Config::default().embedding;
        embedding.dimensions = 64;
        let fingerprint = Embedding::new(embedding).unwrap().fingerprint().to_owned();
        let mut vector = vec![0.0; 64];
        vector[0] = 1.0;
        let vector = encode_vector(&vector).unwrap();
        let mut assets = Vec::new();
        let directory = self
            .directory
            .path()
            .join("stickers")
            .join(format!("{:x}", Sha256::digest(scope.as_bytes())));
        std::fs::create_dir_all(&directory).unwrap();
        for index in 0..count {
            let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                8,
                6,
                image::Rgb([index as u8, 90, 140]),
            ));
            let mut original = std::io::Cursor::new(Vec::new());
            image
                .write_to(&mut original, image::ImageFormat::Png)
                .unwrap();
            let original = original.into_inner();
            let mut thumbnail = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut thumbnail, 85)
                .encode_image(&image)
                .unwrap();
            let hash = format!("{:x}", Sha256::digest(&original));
            let thumbnail_hash = format!("{:x}", Sha256::digest(&thumbnail));
            std::fs::write(directory.join(format!("{hash}.original")), &original).unwrap();
            std::fs::write(directory.join(format!("{hash}.jpg")), &thumbnail).unwrap();
            assets.push((hash, thumbnail_hash, original.len(), thumbnail.len()));
        }
        self.store.call(move |db| {
            db.execute("INSERT OR IGNORE INTO ai_conversations(id,scope,mode,summary) VALUES(?1,?2,'group','必须保留的原摘要')",params![conversation,scope])?;
            let mut ids = Vec::new();
            for (index,(hash,thumbnail_hash,original_bytes,thumbnail_bytes)) in assets.into_iter().enumerate() {
                let source = format!("{scope}:{}",index+100);
                db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at) VALUES(?1,?2,?3,1,789,'control','合成图片来源',?4)",params![source,scope,conversation,now()])?;
                db.execute("INSERT INTO ai_stickers(scope,hash,mime,original_bytes,thumbnail_bytes,thumbnail_hash,fingerprint,dimensions,vector,created_at,updated_at) VALUES(?1,?2,'image/png',?3,?4,?5,?6,64,?7,?8,?8)",params![scope,hash,original_bytes,thumbnail_bytes,thumbnail_hash,fingerprint,vector,now()])?;
                let id = db.last_insert_rowid(); ids.push(id);
                db.execute("INSERT INTO ai_sticker_sources(sticker_id,source_key,scope,conversation_id,epoch) VALUES(?1,?2,?3,?4,1)",params![id,source,scope,conversation])?;
            }
            Ok(ids)
        }).await.unwrap()
    }

    async fn stop(&mut self) {
        self.cancel.cancel();
        if let Some(server) = self.server.take() {
            tokio::time::timeout(Duration::from_secs(10), server)
                .await
                .unwrap()
                .unwrap();
        }
        if let Some(worker) = self.worker.take() {
            tokio::time::timeout(Duration::from_secs(10), worker)
                .await
                .unwrap()
                .unwrap();
        }
    }
}

#[tokio::test]
async fn sticker_http_auth_scope_pagination_and_bounded_images_work_without_a_running_bot() {
    let mut panel = Panel::start().await;
    let ids = panel.seed(SCOPE, 23).await;
    panel.seed(OTHER_SCOPE, 2).await;
    panel.seed("124:group:456", 1).await;
    for path in [
        "/api/stickers/groups".to_owned(),
        format!("/api/stickers/items?scope={SCOPE}"),
        format!("/api/stickers/items/{}/image?scope={SCOPE}", ids[0]),
    ] {
        assert_eq!(
            panel
                .client
                .get(format!("{}{path}", panel.url))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let groups = panel.get("/api/stickers/groups").await;
    assert_eq!(groups["groups"].as_array().unwrap().len(), 3);
    assert_eq!(groups["page"], 1);
    let group = groups["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["scope"] == SCOPE)
        .unwrap();
    assert_eq!(group["count"], 23);
    assert!(group["bytes"].as_u64().unwrap() > 0);
    let first = panel
        .get(&format!("/api/stickers/items?scope={SCOPE}&page=1"))
        .await;
    let second = panel
        .get(&format!("/api/stickers/items?scope={SCOPE}&page=2"))
        .await;
    assert_eq!(first["total"], 23);
    assert_eq!(first["total_pages"], 2);
    assert_eq!(first["items"].as_array().unwrap().len(), 20);
    assert_eq!(second["items"].as_array().unwrap().len(), 3);
    assert_eq!(first["running"], false);
    assert_eq!(first["index"]["enabled"], false);
    let mut listed = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["items"].as_array().unwrap())
        .map(|item| {
            assert_eq!(item["index_status"], "ready");
            for hidden in ["hash", "vector", "fingerprint", "local_path", "source_key"] {
                assert!(item.get(hidden).is_none(), "{hidden} must not be exposed");
            }
            item["id"].as_i64().unwrap()
        })
        .collect::<Vec<_>>();
    listed.sort_unstable();
    assert_eq!(listed, ids);
    let serialized = format!("{groups}{first}{second}");
    assert!(!serialized.contains("never-expose"));
    assert!(!serialized.contains("sha256:"));
    assert!(!serialized.contains(panel.directory.path().to_string_lossy().as_ref()));
    for (variant, mime) in [("thumbnail", "image/jpeg"), ("original", "image/png")] {
        let response = panel
            .request(
                Method::GET,
                &format!(
                    "/api/stickers/items/{}/image?scope={SCOPE}&revision=1&variant={variant}",
                    ids[0]
                ),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], mime);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let bytes = response.bytes().await.unwrap();
        assert!(bytes.len() < 8192);
        let image = image::load_from_memory(&bytes).unwrap();
        assert_eq!((image.width(), image.height()), (8, 6));
    }
    for (query, status) in [
        (
            format!("scope={OTHER_SCOPE}&variant=original"),
            StatusCode::NOT_FOUND,
        ),
        (format!("scope={SCOPE}&revision=99"), StatusCode::CONFLICT),
        (
            format!("scope={SCOPE}&variant=https://example.invalid/file"),
            StatusCode::BAD_REQUEST,
        ),
        (
            format!("scope={SCOPE}&url=https://example.invalid/file"),
            StatusCode::BAD_REQUEST,
        ),
        ("scope=../../private".into(), StatusCode::BAD_REQUEST),
    ] {
        let response = panel
            .request(
                Method::GET,
                &format!("/api/stickers/items/{}/image?{query}", ids[0]),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert!(
            !response
                .text()
                .await
                .unwrap()
                .contains(panel.directory.path().to_string_lossy().as_ref())
        );
    }
    assert_eq!(
        panel
            .request(
                Method::GET,
                &format!("/api/stickers/items?scope={SCOPE}&page=0")
            )
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let oversized = ids[0];
    panel
        .store
        .call(move |db| {
            db.execute(
                "UPDATE ai_stickers SET original_bytes=9*1024*1024 WHERE id=?1",
                [oversized],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        panel
            .request(
                Method::GET,
                &format!("/api/stickers/items/{oversized}/image?scope={SCOPE}&variant=original")
            )
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT,
        "invalid oversized metadata must not produce an image response"
    );
    panel.stop().await;
}

#[tokio::test]
async fn sticker_http_relative_runtime_database_uses_the_same_library_directory() {
    let mut panel = Panel::start_with_relative_database(true).await;
    let id = panel.seed(SCOPE, 1).await[0];
    let items = panel
        .get(&format!("/api/stickers/items?scope={SCOPE}"))
        .await;
    assert_eq!(items["items"][0]["id"], id);
    let response = panel
        .request(
            Method::GET,
            &format!("/api/stickers/items/{id}/image?scope={SCOPE}&variant=thumbnail"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "image/jpeg");
    assert!(image::load_from_memory(&response.bytes().await.unwrap()).is_ok());
    panel.stop().await;
}

#[tokio::test]
async fn sticker_http_mutations_require_csrf_are_idempotent_and_persist_offline() {
    let mut panel = Panel::start().await;
    let ids = panel.seed(SCOPE, 3).await;
    let other = panel.seed(OTHER_SCOPE, 1).await[0];
    panel.store.call(|db| {
        db.execute("INSERT INTO ai_memories(conversation_id,subject_id,kind,text,evidence,created_at,updated_at,origin) VALUES('123:group:456:v0:group',789,'fact','必须保留的长期记忆','[]',1,1,'manual')",[])?;
        db.execute("INSERT INTO ai_group_personas(scope,persona) VALUES('123:group:456','必须保留的人设')",[])?;
        Ok(())
    }).await.unwrap();
    let rebuild =
        json!({"request_id":"sticker-rebuild-one","scope":SCOPE,"id":ids[0],"revision":1});
    let mut spoofed = rebuild.clone();
    spoofed["admin_id"] = json!("untrusted-request-actor");
    assert_eq!(
        panel
            .request(Method::POST, "/api/stickers/reindex")
            .json(&spoofed)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        panel
            .client
            .post(format!("{}/api/stickers/reindex", panel.url))
            .header("cookie", &panel.cookie)
            .json(&rebuild)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        panel
            .request(Method::POST, "/api/stickers/reindex")
            .header("origin", "https://example.invalid")
            .json(&rebuild)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let queued = panel
        .request(Method::POST, "/api/stickers/reindex")
        .json(&rebuild)
        .send()
        .await
        .unwrap();
    assert_eq!(queued.status(), StatusCode::OK);
    let queued: Value = queued.json().await.unwrap();
    assert_eq!(queued["request_id"], rebuild["request_id"]);
    assert_eq!(queued["scope"], SCOPE);
    assert_eq!(queued["action"], "reindex");
    assert_eq!(queued["affected"], 1);
    assert!(
        queued["receipt"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    let repeated: Value = panel
        .request(Method::POST, "/api/stickers/reindex")
        .json(&rebuild)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(repeated, queued);
    for body in [
        json!({"request_id":"sticker-rebuild-one","scope":SCOPE,"id":ids[1],"revision":1}),
        json!({"request_id":"sticker-stale-rebuild","scope":SCOPE,"id":ids[0],"revision":1}),
    ] {
        assert_eq!(
            panel
                .request(Method::POST, "/api/stickers/reindex")
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
    }
    assert_eq!(
        panel
            .request(Method::POST, "/api/stickers/delete")
            .json(&json!({"request_id":"wrong-scope","scope":OTHER_SCOPE,"id":ids[1],"revision":1}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let delete = json!({"request_id":"sticker-delete-one","scope":SCOPE,"id":ids[1],"revision":1});
    assert_eq!(
        panel
            .client
            .post(format!("{}/api/stickers/delete", panel.url))
            .header("cookie", &panel.cookie)
            .json(&delete)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let deleted: Value = panel
        .request(Method::POST, "/api/stickers/delete")
        .json(&delete)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(deleted["action"], "delete");
    assert_eq!(deleted["affected"], 1);
    let repeated: Value = panel
        .request(Method::POST, "/api/stickers/delete")
        .json(&delete)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        repeated, deleted,
        "retrying deletion returns its original receipt"
    );
    assert_eq!(
        panel
            .request(
                Method::GET,
                &format!("/api/stickers/items/{}/image?scope={SCOPE}", ids[1])
            )
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let group_rebuild = json!({"request_id":"sticker-rebuild-group","scope":SCOPE});
    let response: Value = panel
        .request(Method::POST, "/api/stickers/reindex")
        .json(&group_rebuild)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["affected"], 2);
    let items = panel
        .get(&format!("/api/stickers/items?scope={SCOPE}"))
        .await;
    assert_eq!(items["total"], 2);
    assert!(
        items["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["index_status"] == "pending")
    );
    panel.stop().await;
    let db = rusqlite::Connection::open_with_flags(
        &panel.database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let pending: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM ai_stickers WHERE scope=?1 AND reindex_requested=1",
            [SCOPE],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        pending, 2,
        "rebuild work survives shutdown without a running model"
    );
    assert_eq!(
        db.query_row(
            "SELECT SUM(length(vector)) FROM ai_stickers WHERE scope=?1",
            [SCOPE],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        2 * 64 * 4,
        "queuing a rebuild preserves the old vectors until replacement succeeds"
    );
    assert_eq!(
        db.query_row(
            "SELECT reindex_requested FROM ai_stickers WHERE id=?1",
            [other],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM outbox", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT text FROM ai_memories", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "必须保留的长期记忆"
    );
    assert_eq!(
        db.query_row(
            "SELECT summary FROM ai_conversations WHERE scope=?1",
            [SCOPE],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "必须保留的原摘要"
    );
    assert_eq!(
        db.query_row(
            "SELECT persona FROM ai_group_personas WHERE scope=?1",
            [SCOPE],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "必须保留的人设"
    );
}
