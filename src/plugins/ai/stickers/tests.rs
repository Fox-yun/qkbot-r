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
    db.execute_batch(ADMIN_MIGRATION).unwrap();
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

fn admin_actor() -> admin::AdminActor {
    admin::AdminActor {
        admin_id: "administrator".into(),
        session_id: "session-a".into(),
    }
}

#[test]
fn admin_delete_blocks_old_imports_and_reindex_but_allows_a_new_received_source() {
    let (mut db, mut job, image) = fixture();
    commit_import(&mut db, &job, &image, &StickerConfig::default()).unwrap();
    let item = candidate(&db);
    let pending = next_reindex(&db, "new-space", 64, 1).unwrap().unwrap();
    admin::delete_entry(
        &mut db,
        &admin_actor(),
        "delete",
        &scope().key(),
        item.id,
        item.revision,
    )
    .unwrap();
    assert!(commit_import(&mut db, &job, &image, &StickerConfig::default()).is_err());
    assert!(commit_reindex(&mut db, &pending, "new-space", 64, &image.vector).is_err());
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM ai_sticker_sources", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    // Same timestamp deliberately: the reception ID, not time precision, is the boundary.
    db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,access_revision) VALUES('1:group:2:4','1:group:2','conversation',1,7,'user','',100,0)", []).unwrap();
    job.key.message_seq = 4;
    commit_import(&mut db, &job, &image, &StickerConfig::default()).unwrap();
    let restored = candidate(&db);
    assert_ne!(restored.id, item.id);
    assert!(check(&db, &item).is_err());
    check(&db, &restored).unwrap();
}

#[test]
fn admin_delete_is_scoped_revision_checked_and_session_bound_idempotent() {
    let (mut db, job, image) = fixture();
    commit_import(&mut db, &job, &image, &StickerConfig::default()).unwrap();
    let item = candidate(&db);
    let actor = admin_actor();
    let wrong_scope = admin::delete_entry(
        &mut db,
        &actor,
        "scope",
        "1:group:99",
        item.id,
        item.revision,
    )
    .unwrap_err();
    assert!(matches!(
        wrong_scope.downcast_ref::<admin::AdminError>(),
        Some(admin::AdminError::NotFound(_))
    ));
    let stale = admin::delete_entry(
        &mut db,
        &actor,
        "stale",
        &scope().key(),
        item.id,
        item.revision + 1,
    )
    .unwrap_err();
    assert!(matches!(
        stale.downcast_ref::<admin::AdminError>(),
        Some(admin::AdminError::Conflict(_))
    ));
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM ai_sticker_deletions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let result = admin::delete_entry(
        &mut db,
        &actor,
        "same-id",
        &scope().key(),
        item.id,
        item.revision,
    )
    .unwrap();
    assert_eq!(
        admin::delete_entry(
            &mut db,
            &actor,
            "same-id",
            &scope().key(),
            item.id,
            item.revision
        )
        .unwrap(),
        result
    );
    let changed_payload =
        admin::request_reindex(&mut db, &actor, "same-id", &scope().key(), None, None).unwrap_err();
    assert!(matches!(
        changed_payload.downcast_ref::<admin::AdminError>(),
        Some(admin::AdminError::Conflict(_))
    ));
    for actor in [
        admin::AdminActor {
            session_id: "session-b".into(),
            ..actor.clone()
        },
        admin::AdminActor {
            admin_id: "other-administrator".into(),
            ..actor.clone()
        },
    ] {
        let err = admin::delete_entry(
            &mut db,
            &actor,
            "same-id",
            &scope().key(),
            item.id,
            item.revision,
        )
        .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<admin::AdminError>(),
            Some(admin::AdminError::NotFound(_))
        ));
    }
}

#[test]
fn admin_forced_reindex_keeps_vector_and_new_requests_supersede_inflight_work() {
    let (mut db, job, image) = fixture();
    commit_import(&mut db, &job, &image, &StickerConfig::default()).unwrap();
    let item = candidate(&db);
    let actor = admin_actor();
    assert!(next_reindex(&db, "space", 64, 1).unwrap().is_none());
    admin::request_reindex(
        &mut db,
        &actor,
        "first",
        &scope().key(),
        Some(item.id),
        Some(item.revision),
    )
    .unwrap();
    let pending = next_reindex(&db, "space", 64, 1).unwrap().unwrap();
    assert_eq!(
        db.query_row("SELECT vector FROM ai_stickers", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        image.vector
    );
    admin::request_reindex(
        &mut db,
        &actor,
        "second",
        &scope().key(),
        Some(item.id),
        Some(pending.revision),
    )
    .unwrap();
    assert!(commit_reindex(&mut db, &pending, "space", 64, &image.vector).is_err());
    let latest = next_reindex(&db, "space", 64, 1).unwrap().unwrap();
    commit_reindex(&mut db, &latest, "space", 64, &image.vector).unwrap();
    assert!(next_reindex(&db, "space", 64, 1).unwrap().is_none());
    assert!(
        !db.query_row("SELECT reindex_requested FROM ai_stickers", [], |r| r
            .get::<_, bool>(0))
            .unwrap()
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM ai_sticker_sources", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn admin_group_reindex_replay_does_not_enqueue_new_images_and_resets_backoff() {
    let (mut db, job, image) = fixture();
    commit_import(&mut db, &job, &image, &StickerConfig::default()).unwrap();
    db.execute("UPDATE ai_stickers SET retry_after=?1", [now() + 300])
        .unwrap();
    let actor = admin_actor();
    let first =
        admin::request_reindex(&mut db, &actor, "group", &scope().key(), None, None).unwrap();
    assert_eq!(first["affected"], 1);
    assert!(next_reindex(&db, "space", 64, 1).unwrap().is_some());
    commit_import(&mut db, &job, &import(b"second"), &StickerConfig::default()).unwrap();
    assert_eq!(
        admin::request_reindex(&mut db, &actor, "group", &scope().key(), None, None).unwrap(),
        first
    );
    assert_eq!(
        db.query_row("SELECT SUM(reindex_requested) FROM ai_stickers", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap(),
        1
    );
    let other = admin::request_reindex(&mut db, &actor, "other", "2:group:2", None, None).unwrap();
    assert_eq!(other["affected"], 0);
}

async fn admin_fixture() -> (tempfile::TempDir, Store, StickerStore, admin::StickerAdmin) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db"), 32).await.unwrap();
    store.call(|db| {
        db.execute_batch("CREATE TABLE ai_revoked_sources(source_key TEXT PRIMARY KEY,revoked_at INTEGER); INSERT INTO plugin_states VALUES('ai',1,1); INSERT INTO ai_conversations(id,scope,mode,epoch) VALUES('conversation','1:group:2','group',1); INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,access_revision) VALUES('1:group:2:3','1:group:2','conversation',1,7,'user','',100,0);")?;
        Ok(())
    }).await.unwrap();
    let library = StickerStore::new(
        store.clone(),
        StickerConfig::default(),
        dir.path().join("stickers"),
    )
    .unwrap();
    library.init().await.unwrap();
    let (_, job, image) = fixture();
    store
        .call(move |db| commit_import(db, &job, &image, &StickerConfig::default()))
        .await
        .unwrap();
    let (original, thumb) = library.paths(&scope(), &hash(b"original")).unwrap();
    write_blob(&original, b"original").await.unwrap();
    write_blob(&thumb, b"thumb").await.unwrap();
    let admin = admin::StickerAdmin::new(
        store.clone(),
        dir.path().join("stickers"),
        Some(admin::IndexTarget {
            fingerprint: "space".into(),
            model: "synthetic-model".into(),
            dimensions: 64,
            enabled: false,
        }),
    )
    .unwrap();
    (dir, store, library, admin)
}

#[tokio::test]
async fn admin_offline_browsing_checks_files_and_deletion_leaves_files_for_worker_gc() {
    let (_dir, store, library, admin) = admin_fixture().await;
    store
        .call(|db| {
            db.execute_batch(
                "UPDATE ai_sticker_control SET enabled=0; UPDATE plugin_states SET enabled=0;",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let groups = admin.groups(1).await.unwrap();
    assert_eq!(groups["groups"][0]["count"], 1);
    assert_eq!(groups["index"]["enabled"], false);
    let items = admin.items(&scope().key(), 1).await.unwrap();
    let item = &items["items"][0];
    let id = item["id"].as_i64().unwrap();
    let revision = item["revision"].as_i64().unwrap();
    assert_eq!(item["index_status"], "ready");
    assert_eq!(item["source_available"], true);
    for private in ["hash", "thumbnail_hash", "vector", "path", "fingerprint"] {
        assert!(item.get(private).is_none());
    }
    assert_eq!(
        admin
            .image(&scope().key(), id, None, admin::AssetKind::Preview)
            .await
            .unwrap()
            .bytes,
        b"thumb"
    );
    assert_eq!(
        admin
            .image(
                &scope().key(),
                id,
                Some(revision),
                admin::AssetKind::Original
            )
            .await
            .unwrap()
            .bytes,
        b"original"
    );
    assert!(
        admin
            .image("1:group:99", id, None, admin::AssetKind::Original)
            .await
            .is_err()
    );
    assert!(
        admin
            .image(
                &scope().key(),
                id,
                Some(revision + 1),
                admin::AssetKind::Original
            )
            .await
            .is_err()
    );
    let (original, thumb) = library.paths(&scope(), &hash(b"original")).unwrap();
    tokio::fs::write(&thumb, b"wrong").await.unwrap();
    assert!(
        admin
            .image(&scope().key(), id, None, admin::AssetKind::Preview)
            .await
            .is_err()
    );
    admin
        .delete(&admin_actor(), "delete", &scope().key(), id, revision)
        .await
        .unwrap();
    assert!(original.exists() && thumb.exists());
    assert!(
        admin
            .image(&scope().key(), id, None, admin::AssetKind::Original)
            .await
            .is_err()
    );
    library.cleanup().await.unwrap();
    assert!(!original.exists() && !thumb.exists());
    store.stop().await.unwrap();
}

#[tokio::test]
async fn admin_queue_survives_store_reopen_and_lists_pending_or_failed_without_network() {
    let (dir, store, _library, admin) = admin_fixture().await;
    let value = admin
        .reindex(&admin_actor(), "queue", &scope().key(), None, None)
        .await
        .unwrap();
    assert_eq!(value["affected"], 1);
    let pending = admin.items(&scope().key(), 1).await.unwrap();
    assert_eq!(pending["items"][0]["index_status"], "pending");
    store.stop().await.unwrap();
    let reopened = Store::open(dir.path().join("test.db"), 32).await.unwrap();
    init_store(&reopened).await.unwrap();
    let api =
        admin::StickerAdmin::new(reopened.clone(), dir.path().join("stickers"), None).unwrap();
    assert_eq!(
        api.items(&scope().key(), 1).await.unwrap()["items"][0]["index_status"],
        "pending"
    );
    assert_eq!(
        api.reindex(&admin_actor(), "queue", &scope().key(), None, None)
            .await
            .unwrap(),
        value
    );
    reopened
        .call(|db| {
            db.execute("UPDATE ai_stickers SET retry_after=?1", [now() + 300])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        api.items(&scope().key(), 1).await.unwrap()["items"][0]["index_status"],
        "failed"
    );
    assert_eq!(api.groups(1).await.unwrap()["groups"][0]["failed"], 1);
    reopened.stop().await.unwrap();
}

#[tokio::test]
async fn admin_pagination_validates_inputs_and_revoked_assets_are_not_readable() {
    let (_dir, store, _library, admin) = admin_fixture().await;
    let (_, job, _) = fixture();
    store
        .call(move |db| {
            for i in 0..20 {
                commit_import(
                    db,
                    &job,
                    &import(format!("extra-{i}").as_bytes()),
                    &StickerConfig::default(),
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let first = admin.items(&scope().key(), 1).await.unwrap();
    let second = admin.items(&scope().key(), 2).await.unwrap();
    assert_eq!(first["total"], 21);
    assert_eq!(first["total_pages"], 2);
    assert_eq!(first["items"].as_array().unwrap().len(), 20);
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert!(admin.groups(0).await.is_err());
    for bad in ["1:friend:2", "01:group:2", "1:group:0", "../1:group:2"] {
        assert!(admin.items(bad, 1).await.is_err());
    }
    assert!(
        admin
            .reindex(&admin_actor(), "bad", &scope().key(), Some(1), None)
            .await
            .is_err()
    );
    store
        .call(|db| {
            db.execute("INSERT INTO ai_sticker_revoked VALUES('1:group:2:3',1)", [])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        admin.items(&scope().key(), 2).await.unwrap()["items"][0]["source_available"],
        false
    );
    assert!(
        admin
            .image(
                &scope().key(),
                second["items"][0]["id"].as_i64().unwrap(),
                None,
                admin::AssetKind::Original
            )
            .await
            .is_err()
    );
    store.stop().await.unwrap();
}

#[tokio::test]
async fn admin_v2_migration_preserves_v1_vector_and_does_not_enable_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db"), 32).await.unwrap();
    store.migrate("ai_stickers", 1, MIGRATION).await.unwrap();
    store.call(|db| {
        db.execute("INSERT INTO ai_sticker_control VALUES(1,0,7,120)",[])?;
        let image=import(b"legacy");
        db.execute("INSERT INTO ai_stickers(scope,hash,mime,original_bytes,thumbnail_bytes,thumbnail_hash,fingerprint,dimensions,vector,created_at,updated_at) VALUES('1:group:2',?1,'image/gif',6,5,?2,'space',64,?3,1,1)",params![image.hash,image.thumbnail_hash,image.vector])?;
        Ok(())
    }).await.unwrap();
    init_store(&store).await.unwrap();
    init_store(&store).await.unwrap();
    store
        .call(|db| {
            assert_eq!(
                db.query_row(
                    "SELECT enabled,revision,cooldown_secs FROM ai_sticker_control",
                    [],
                    |r| Ok((
                        r.get::<_, bool>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?
                    ))
                )?,
                (false, 7, 120)
            );
            assert_eq!(
                db.query_row(
                    "SELECT vector,reindex_requested FROM ai_stickers",
                    [],
                    |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, bool>(1)?))
                )?,
                (import(b"legacy").vector, false)
            );
            assert_eq!(
                db.query_row(
                    "SELECT COUNT(*) FROM schema_migrations WHERE plugin_id='ai_stickers'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                2
            );
            Ok(())
        })
        .await
        .unwrap();
    store.stop().await.unwrap();
}
