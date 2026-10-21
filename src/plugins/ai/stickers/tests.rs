use super::*;
use serde_json::json;

const FIXTURE: &str = r#"
PRAGMA foreign_keys=ON;
CREATE TABLE scope_access(scope TEXT PRIMARY KEY,enabled INTEGER,revision INTEGER);
CREATE TABLE scope_plugin_access(scope TEXT,plugin TEXT,enabled INTEGER,revision INTEGER,PRIMARY KEY(scope,plugin));
CREATE TABLE plugin_states(id TEXT PRIMARY KEY,epoch INTEGER,enabled INTEGER);
CREATE TABLE ai_conversations(id TEXT PRIMARY KEY,scope TEXT,epoch INTEGER);
CREATE TABLE messages(id INTEGER PRIMARY KEY AUTOINCREMENT,key TEXT UNIQUE,scope TEXT,conversation_id TEXT,epoch INTEGER,sender_id INTEGER,role TEXT,content TEXT,received_at INTEGER,access_revision INTEGER,revoked INTEGER DEFAULT 0);
CREATE TABLE ai_revoked_sources(source_key TEXT PRIMARY KEY,revoked_at INTEGER);
INSERT INTO plugin_states VALUES('ai',1,1);
INSERT INTO ai_conversations VALUES('conversation','1:group:2',1);
INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,access_revision) VALUES('1:group:2:3','1:group:2','conversation',1,7,'control','',100,0);
"#;

fn scope() -> Scope {
    Scope {
        bot_id: 1,
        scene: Scene::Group,
        peer_id: 2,
    }
}

fn fixture() -> (Connection, Job, Import) {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(FIXTURE).unwrap();
    db.execute_batch(MIGRATION).unwrap();
    db.execute("INSERT INTO ai_sticker_control VALUES(1,1,1,60)", [])
        .unwrap();
    let job = Job {
        key: MessageKey {
            scope: scope(),
            message_seq: 3,
        },
        sender: 7,
        resource: "resource".into(),
        automatic: true,
        stamp: stamp(&db, &scope(), 1).unwrap(),
        context: ContextGuard {
            conversation_id: "conversation".into(),
            epoch: 1,
        },
        received_at: 100,
    };
    let image = import(b"original");
    (db, job, image)
}

fn import(original: &[u8]) -> Import {
    let mut vector = vec![0.0; 64];
    vector[0] = 1.0;
    Import {
        hash: hash(original),
        mime: "image/gif".into(),
        original_bytes: original.len(),
        thumbnail_bytes: 5,
        thumbnail_hash: hash(b"thumb"),
        fingerprint: "space".into(),
        dimensions: 64,
        vector: encode_vector(&vector).unwrap(),
    }
}

fn candidate(db: &Connection) -> Candidate {
    db.query_row(
        "SELECT id,revision,hash FROM ai_stickers ORDER BY id LIMIT 1",
        [],
        |r| {
            Ok(Candidate {
                id: r.get(0)?,
                revision: r.get(1)?,
                hash: r.get(2)?,
                thumbnail_jpeg: vec![],
                score: 1.0,
            })
        },
    )
    .unwrap()
}

fn check(db: &Connection, value: &Candidate) -> Result<()> {
    check_candidate(db, &scope(), value.id, value.revision, &value.hash)
}

fn incoming(segments: serde_json::Value) -> Incoming {
    Incoming {
        key: MessageKey {
            scope: scope(),
            message_seq: 3,
        },
        sender_id: 7,
        sender_nickname: None,
        sender_card: None,
        text: String::new(),
        mentions: vec![],
        quote_seq: None,
        received_at: 100,
        raw_message: json!({"segments":segments}),
        notification: None,
    }
}

#[test]
fn only_top_level_explicit_stickers_are_automatically_queued() {
    let message = incoming(json!([
        {"type":"image","data":{"resource_id":"normal","sub_type":"normal"}},
        {"type":"image","data":{"resource_id":"missing"}},
        {"type":"forward","data":{"segments":[{"type":"image","data":{"resource_id":"nested","sub_type":"sticker"}}]}},
        {"type":"market_face","data":{"resource_id":"market"}},
        {"type":"text","data":{"text":"https://qpic.cn/sticker.gif"}},
        {"type":"image","data":{"resource_id":"bad\nvalue","sub_type":"sticker"}},
        {"type":"image","data":{"resource_id":"a","sub_type":"sticker"}},
        {"type":"image","data":{"resource_id":"a","sub_type":"sticker"}},
        {"type":"image","data":{"resource_id":"b","sub_type":"sticker"}},
        {"type":"image","data":{"resource_id":"c","sub_type":"sticker"}}
    ]));
    assert_eq!(resources(&message, true), vec!["a", "b"]);
    assert_eq!(resources(&message, false), vec!["normal", "missing"]);
}

#[test]
fn import_revalidates_every_source_and_permission_snapshot_atomically() {
    for mutation in [
        "UPDATE messages SET revoked=1",
        "UPDATE messages SET sender_id=8",
        "UPDATE messages SET scope='2:group:2'",
        "UPDATE messages SET received_at=101",
        "UPDATE messages SET access_revision=1",
        "UPDATE messages SET epoch=2",
        "DELETE FROM messages",
        "UPDATE ai_conversations SET epoch=2",
        "UPDATE plugin_states SET epoch=2",
        "UPDATE plugin_states SET enabled=0",
        "INSERT INTO scope_access VALUES('1:group:2',0,1)",
        "INSERT INTO scope_access VALUES('1:group:2',1,1)",
        "INSERT INTO scope_plugin_access VALUES('1:group:2','ai',0,1)",
        "INSERT INTO ai_sticker_scopes(scope,enabled,revision) VALUES('1:group:2',0,1)",
        "UPDATE ai_sticker_control SET revision=2",
        "INSERT INTO ai_revoked_sources VALUES('1:group:2:3',1)",
        "INSERT INTO ai_sticker_revoked VALUES('1:group:2:3',1)",
    ] {
        let (mut db, job, image) = fixture();
        db.execute(mutation, []).unwrap();
        assert!(
            commit_import(&mut db, &job, &image, &StickerConfig::default()).is_err(),
            "{mutation}"
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM ai_stickers", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "{mutation}"
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM ai_sticker_sources", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "{mutation}"
        );
    }
}

#[test]
fn persistent_provenance_survives_raw_retention_but_not_forgetting_or_revocation() {
    for mutation in [
        "INSERT INTO ai_revoked_sources VALUES('1:group:2:3',1)",
        "INSERT INTO ai_sticker_revoked VALUES('1:group:2:3',1)",
        "UPDATE ai_conversations SET epoch=2",
        "DELETE FROM ai_conversations",
    ] {
        let (mut db, job, image) = fixture();
        commit_import(&mut db, &job, &image, &StickerConfig::default()).unwrap();
        let candidate = candidate(&db);
        check(&db, &candidate).unwrap();
        db.execute("DELETE FROM messages", []).unwrap();
        check(&db, &candidate).unwrap();
        assert!(
            check_job(&db, &job).is_err(),
            "new imports still require their source"
        );
        db.execute(mutation, []).unwrap();
        assert!(check(&db, &candidate).is_err(), "{mutation}");
    }
}

#[test]
fn ordinary_settings_carry_sources_forward_only_with_an_existing_matching_raw_row() {
    let (mut db, job, image) = fixture();
    commit_import(&mut db, &job, &image, &StickerConfig::default()).unwrap();
    let carried = candidate(&db);
    db.execute("UPDATE ai_conversations SET epoch=2", [])
        .unwrap();
    assert!(check(&db, &carried).is_err());
    // This is the retained-message carry-forward performed by normal settings.
    db.execute("UPDATE messages SET epoch=2", []).unwrap();
    check(&db, &carried).unwrap();
    carry_forward_sources(&db).unwrap();
    assert_eq!(
        db.query_row("SELECT epoch FROM ai_sticker_sources", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    db.execute("DELETE FROM messages", []).unwrap();
    check(&db, &carried).unwrap();
    db.execute("UPDATE ai_conversations SET epoch=3", [])
        .unwrap();
    carry_forward_sources(&db).unwrap();
    assert!(check(&db, &carried).is_err());
    assert_eq!(
        db.query_row("SELECT epoch FROM ai_sticker_sources", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    for mutation in [
        "UPDATE messages SET scope='9:group:2',epoch=2",
        "UPDATE messages SET conversation_id='other',epoch=2",
        "UPDATE messages SET revoked=1,epoch=2",
        "INSERT INTO ai_revoked_sources VALUES('1:group:2:3',1)",
        "INSERT INTO ai_sticker_revoked VALUES('1:group:2:3',1)",
        "DELETE FROM messages",
    ] {
        let (mut db, job, image) = fixture();
        commit_import(&mut db, &job, &image, &StickerConfig::default()).unwrap();
        let candidate = candidate(&db);
        db.execute("UPDATE ai_conversations SET epoch=2", [])
            .unwrap();
        db.execute(mutation, []).unwrap();
        carry_forward_sources(&db).unwrap();
        assert!(check(&db, &candidate).is_err(), "{mutation}");
        assert_eq!(
            db.query_row("SELECT epoch FROM ai_sticker_sources", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1,
            "{mutation}"
        );
    }
}

#[test]
fn candidate_requires_exact_scope_hash_revision_and_a_live_source() {
    let (mut db, job, image) = fixture();
    commit_import(&mut db, &job, &image, &StickerConfig::default()).unwrap();
    let candidate = candidate(&db);
    check(&db, &candidate).unwrap();
    let other = Scope {
        bot_id: 2,
        ..scope()
    };
    assert!(
        check_candidate(
            &db,
            &other,
            candidate.id,
            candidate.revision,
            &candidate.hash
        )
        .is_err()
    );
    assert!(
        check_candidate(
            &db,
            &scope(),
            candidate.id,
            candidate.revision + 1,
            &candidate.hash
        )
        .is_err()
    );
    assert!(
        check_candidate(
            &db,
            &scope(),
            candidate.id,
            candidate.revision,
            &hash(b"another")
        )
        .is_err()
    );
    db.execute("UPDATE messages SET revoked=1", []).unwrap();
    assert!(check(&db, &candidate).is_err());
}

#[test]
fn identical_content_deduplicates_and_provenance_is_bounded_independently_of_retention() {
    let (mut db, mut job, image) = fixture();
    commit_import(&mut db, &job, &image, &StickerConfig::default()).unwrap();
    let original = candidate(&db);
    db.execute("DELETE FROM messages", []).unwrap();
    for sequence in 4..=40 {
        job.key.message_seq = sequence;
        db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,access_revision) VALUES(?1,'1:group:2','conversation',1,7,'user','',100,0)", [job.key.key()]).unwrap();
        commit_import(&mut db, &job, &image, &StickerConfig::default()).unwrap();
        db.execute("DELETE FROM messages", []).unwrap();
    }
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM ai_stickers", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM ai_sticker_sources", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        32
    );
    assert!(check(&db, &original).is_err());
    check(&db, &candidate(&db)).unwrap();
}

#[test]
fn cooldown_is_transactional_and_not_consumed_by_checks() {
    let (mut db, job, image) = fixture();
    commit_import(&mut db, &job, &image, &StickerConfig::default()).unwrap();
    let candidate = candidate(&db);
    check(&db, &candidate).unwrap();
    check(&db, &candidate).unwrap();
    {
        let tx = db.transaction().unwrap();
        mark_sent(
            &tx,
            &scope(),
            candidate.id,
            candidate.revision,
            &candidate.hash,
        )
        .unwrap();
        assert!(check(&tx, &candidate).is_err());
    }
    check(&db, &candidate).unwrap();
    mark_sent(
        &db,
        &scope(),
        candidate.id,
        candidate.revision,
        &candidate.hash,
    )
    .unwrap();
    assert!(check(&db, &candidate).is_err());
    db.execute("UPDATE ai_sticker_scopes SET last_sent_at=?1", [now() - 61])
        .unwrap();
    check(&db, &candidate).unwrap();
}

#[test]
fn capacity_and_invalid_vectors_do_not_leave_partial_metadata() {
    let (mut db, job, mut image) = fixture();
    let config = StickerConfig {
        max_per_group: 1,
        ..StickerConfig::default()
    };
    commit_import(&mut db, &job, &image, &config).unwrap();
    image.hash = hash(b"another");
    assert!(commit_import(&mut db, &job, &image, &config).is_err());
    image.vector.fill(0);
    assert!(commit_import(&mut db, &job, &image, &StickerConfig::default()).is_err());
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM ai_stickers", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM ai_sticker_sources", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn hourly_admission_is_bounded_and_rollback_safe() {
    let (mut db, _, _) = fixture();
    {
        let tx = db.transaction().unwrap();
        admit(&tx, 1).unwrap();
    }
    admit(&db, 1).unwrap();
    assert!(admit(&db, 1).is_err());
    db.execute("UPDATE ai_sticker_admissions SET at=?1", [now() - 3601])
        .unwrap();
    admit(&db, 1).unwrap();
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM ai_sticker_admissions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn cancelled_pending_work_releases_queue_slot() {
    let queue = Arc::new(Mutex::new(Queue::default()));
    let identity = ("source".into(), "resource".into());
    queue.lock().unwrap().pending.insert(identity.clone());
    let cloned = queue.clone();
    let (ready, started) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _pending = Pending {
            queue: &cloned,
            identity,
        };
        ready.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    started.await.unwrap();
    task.abort();
    let _ = task.await;
    assert!(queue.lock().unwrap().pending.is_empty());
}

#[tokio::test]
async fn original_gif_is_preserved_but_embedding_uses_only_first_frame_jpeg() {
    let mut original = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut original);
        for rgba in [[255, 0, 0, 255], [0, 0, 255, 255]] {
            encoder
                .encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
                    16,
                    16,
                    image::Rgba(rgba),
                )))
                .unwrap();
        }
    }
    let decoded = super::super::vision::decode(original.clone(), &CancellationToken::new())
        .await
        .unwrap();
    let jpeg = base64::engine::general_purpose::STANDARD
        .decode(decoded.strip_prefix("data:image/jpeg;base64,").unwrap())
        .unwrap();
    assert_eq!(
        image::guess_format(&jpeg).unwrap(),
        image::ImageFormat::Jpeg
    );
    let pixel = image::load_from_memory(&jpeg)
        .unwrap()
        .to_rgb8()
        .get_pixel(8, 8)
        .0;
    assert!(pixel[0] > 200 && pixel[1] < 30 && pixel[2] < 30);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("saved.original");
    write_blob(&path, &original).await.unwrap();
    write_blob(&path, &original).await.unwrap();
    assert_eq!(read_blob(&path, SOURCE_LIMIT).await.unwrap(), original);
    assert!(write_blob(&path, b"changed").await.is_err());
    assert_eq!(read_blob(&path, SOURCE_LIMIT).await.unwrap(), original);
    assert!(read_blob(&path, 1).await.is_err());
}

#[tokio::test]
async fn initialization_does_not_create_a_directory_and_cleanup_keeps_retained_sources() {
    let directory = tempfile::tempdir().unwrap();
    let files = directory.path().join("stickers");
    let store = Store::open(directory.path().join("test.db"), 32)
        .await
        .unwrap();
    store.call(|db| {
        db.execute_batch("CREATE TABLE ai_revoked_sources(source_key TEXT PRIMARY KEY,revoked_at INTEGER); INSERT INTO plugin_states VALUES('ai',1,1); INSERT INTO ai_conversations(id,scope,mode,epoch) VALUES('conversation','1:group:2','group',1); INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,access_revision) VALUES('1:group:2:3','1:group:2','conversation',1,7,'user','',100,0);")?; Ok(())
    }).await.unwrap();
    let library =
        StickerStore::new(store.clone(), StickerConfig::default(), files.clone()).unwrap();
    library.init().await.unwrap();
    library.cleanup().await.unwrap();
    assert!(!files.exists());
    let (_, job, image) = fixture();
    let content = image.hash.clone();
    store
        .call(move |db| commit_import(db, &job, &image, &StickerConfig::default()))
        .await
        .unwrap();
    let (original, thumbnail) = library.paths(&scope(), &content).unwrap();
    write_blob(&original, b"original").await.unwrap();
    write_blob(&thumbnail, b"thumb").await.unwrap();
    store
        .call(|db| {
            db.execute_batch("UPDATE ai_conversations SET epoch=2; UPDATE messages SET epoch=2;")?;
            Ok(())
        })
        .await
        .unwrap();
    library.cleanup().await.unwrap();
    assert_eq!(
        store
            .call(|db| Ok(
                db.query_row("SELECT epoch FROM ai_sticker_sources", [], |r| r
                    .get::<_, i64>(0))?
            ))
            .await
            .unwrap(),
        2
    );
    store
        .call(|db| {
            db.execute("DELETE FROM messages", [])?;
            Ok(())
        })
        .await
        .unwrap();
    library.cleanup().await.unwrap();
    assert!(original.exists());
    assert!(thumbnail.exists());
    library.revoke("1:group:2:3").await.unwrap();
    library.cleanup().await.unwrap();
    assert!(!original.exists());
    assert!(!thumbnail.exists());
    let rows = store
        .call(|db| {
            Ok(db.query_row("SELECT COUNT(*) FROM ai_stickers", [], |r| {
                r.get::<_, i64>(0)
            })?)
        })
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn reindex_reuses_retained_thumbnail_without_changing_provenance() {
    let (mut db, job, image) = fixture();
    let config = StickerConfig {
        max_per_group: 1,
        ..StickerConfig::default()
    };
    commit_import(&mut db, &job, &image, &config).unwrap();
    db.execute("DELETE FROM messages", []).unwrap();
    let before: (i64, String, i64) = db
        .query_row(
            "SELECT COUNT(*),MAX(source_key),MAX(epoch) FROM ai_sticker_sources",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert!(next_reindex(&db, "space", 64, 1).unwrap().is_none());
    let pending = next_reindex(&db, "new-space", 64, 1).unwrap().unwrap();
    // Reindexing is neither a new disk item nor a send; a full library and send
    // cooldown must not prevent a model change from being repaired.
    check_new_capacity(
        &db,
        &scope(),
        &image.hash,
        &config,
        image.original_bytes + image.thumbnail_bytes,
    )
    .unwrap();
    mark_sent(&db, &scope(), pending.id, pending.revision, &pending.hash).unwrap();
    let mut pending = pending;
    pending.stamp = stamp(&db, &scope(), 1).unwrap();
    commit_reindex(&mut db, &pending, "new-space", 64, &image.vector).unwrap();
    assert!(next_reindex(&db, "new-space", 64, 1).unwrap().is_none());
    let after: (i64, String, i64) = db
        .query_row(
            "SELECT COUNT(*),MAX(source_key),MAX(epoch) FROM ai_sticker_sources",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(before, after);
    assert!(commit_reindex(&mut db, &pending, "another-space", 64, &image.vector).is_err());
}

#[test]
fn reindex_has_retry_backoff_and_compare_and_swap_guards() {
    for mutation in [
        "UPDATE ai_stickers SET revision=revision+1",
        "UPDATE ai_stickers SET thumbnail_hash='changed'",
        "UPDATE ai_conversations SET epoch=2",
        "INSERT INTO ai_revoked_sources VALUES('1:group:2:3',1)",
        "UPDATE plugin_states SET epoch=2",
        "INSERT INTO scope_access VALUES('1:group:2',0,1)",
        "UPDATE ai_sticker_control SET revision=revision+1",
    ] {
        let (mut db, job, image) = fixture();
        commit_import(&mut db, &job, &image, &StickerConfig::default()).unwrap();
        let pending = next_reindex(&db, "new-space", 64, 1).unwrap().unwrap();
        db.execute(mutation, []).unwrap();
        assert!(
            commit_reindex(&mut db, &pending, "new-space", 64, &image.vector).is_err(),
            "{mutation}"
        );
        assert_eq!(
            db.query_row("SELECT fingerprint FROM ai_stickers", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "space"
        );
    }
    let (mut db, job, image) = fixture();
    commit_import(&mut db, &job, &image, &StickerConfig::default()).unwrap();
    db.execute("UPDATE ai_stickers SET retry_after=?1", [now() + 300])
        .unwrap();
    assert!(next_reindex(&db, "new-space", 64, 1).unwrap().is_none());
    db.execute("UPDATE ai_stickers SET retry_after=?1", [now() - 1])
        .unwrap();
    assert!(next_reindex(&db, "new-space", 64, 1).unwrap().is_some());
}

#[tokio::test]
async fn cleanup_cannot_delete_an_import_file_before_its_metadata_commit() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("test.db"), 32)
        .await
        .unwrap();
    store
        .call(|db| {
            db.execute_batch(
                "CREATE TABLE ai_revoked_sources(source_key TEXT PRIMARY KEY,revoked_at INTEGER);",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let library = StickerStore::new(
        store,
        StickerConfig::default(),
        directory.path().join("stickers"),
    )
    .unwrap();
    library.init().await.unwrap();
    let (path, _) = library.paths(&scope(), &hash(b"inflight")).unwrap();
    let worker = library.worker.lock().await;
    write_blob(&path, b"inflight").await.unwrap();
    library.cleanup().await.unwrap();
    assert!(path.exists());
    drop(worker);
    library.cleanup().await.unwrap();
    assert!(!path.exists());
}
