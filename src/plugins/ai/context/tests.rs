use super::*;
use crate::plugins::ai::memory::{BatchUpdate, MemoryStore};

const CONVERSATION: &str = "1:group:20:v0:group";
const SOURCE_SCOPE: &str = "1:group:20";

fn conversation() -> ContextGuard {
    ContextGuard {
        conversation_id: CONVERSATION.into(),
        epoch: 1,
    }
}

async fn database() -> (Store, MemoryStore) {
    let store = Store::open(":memory:", 32).await.unwrap();
    let memories = MemoryStore::new(store.clone(), 256);
    memories.init().await.unwrap();
    store.call(|db| {
        db.execute("INSERT INTO ai_conversations(id,scope,mode,epoch) VALUES(?1,?2,'group',1)", params![CONVERSATION,SOURCE_SCOPE])?;
        db.execute("INSERT INTO ai_memories(id,conversation_id,subject_id,kind,text,evidence,created_at,updated_at,origin,locked,version) VALUES(1,?1,7,'fact','管理员补充的资料','[]',?2,?2,'manual',1,1)", params![CONVERSATION,now()])?;
        Ok(())
    }).await.unwrap();
    (store, memories)
}

fn memory_guard() -> ContextDependencyGuard {
    ContextDependencyGuard {
        memories: vec![MemoryDependency { id: 1, version: 1 }],
        ..ContextDependencyGuard::default()
    }
}

fn message(db: &Connection, id: i64, role: &str, position: i64) -> Result<String> {
    let key = format!("{SOURCE_SCOPE}:{id}");
    let sender = if role == "assistant" { 1 } else { 7 };
    let origin = if role == "assistant" {
        Some("ai")
    } else {
        None
    };
    db.execute("INSERT INTO messages(id,key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,origin_plugin,learnable) VALUES(?1,?2,?3,?4,1,?5,?6,?7,?8,?9,?10,0)",
        params![id,key,SOURCE_SCOPE,CONVERSATION,sender,role,format!("synthetic {role} {id}"),now(),position,origin])?;
    Ok(key)
}

fn save_reply_guard(db: &Connection, key: &str, guard: &ContextDependencyGuard) -> Result<()> {
    guard.validate(db)?;
    db.execute(
        "INSERT INTO ai_context_reply_dependencies(reply_key,dependencies) VALUES(?1,?2)",
        params![key, serde_json::to_string(guard)?],
    )?;
    Ok(())
}

async fn first_summary(store: &Store, memories: &MemoryStore) {
    store
        .call(|db| {
            message(db, 1, "user", 1)?;
            let reply = message(db, 2, "assistant", 1)?;
            save_reply_guard(db, &reply, &memory_guard())
        })
        .await
        .unwrap();
    let batch = memories
        .prepare_batch(&conversation(), 1, false)
        .await
        .unwrap();
    memories
        .commit_batch(
            batch,
            BatchUpdate {
                summary: "首轮包含人工资料的摘要".into(),
                changes: vec![],
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn manual_memory_edit_invalidates_original_inherited_reply_and_summary() {
    let (store, memories) = database().await;
    first_summary(&store, &memories).await;
    store
        .call(|db| {
            let first = format!("{SOURCE_SCOPE}:2");
            let second = message(db, 3, "assistant", 1)?;
            let mut inherited = ContextDependencyGuard::default();
            inherited.include_sources(db, std::slice::from_ref(&first))?;
            inherited.include_sources(db, std::slice::from_ref(&first))?;
            assert_eq!(
                inherited.memories.len(),
                1,
                "flattened repeated sources must not duplicate memory dependencies"
            );
            save_reply_guard(db, &second, &inherited)?;
            assert!(reply_is_valid(db, &first)?);
            assert!(reply_is_valid(db, &second)?);
            assert_eq!(summary_guard(db, &conversation())?.memories.len(), 1);

            db.execute(
                "UPDATE ai_memories SET text='修正后的人工资料',version=version+1 WHERE id=1",
                [],
            )?;
            assert!(!reply_is_valid(db, &first)?);
            assert!(!reply_is_valid(db, &second)?);
            assert!(inherited.validate(db).is_err());
            assert!(summary_guard(db, &conversation()).is_err());
            let mut fresh = ContextDependencyGuard::default();
            assert!(fresh.include_sources(db, &[second]).is_err());
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn disabled_expired_and_deleted_knowledge_invalidates_saved_reply_dependencies() {
    for mutation in [
        "UPDATE ai_memories SET active=0 WHERE id=1",
        "UPDATE ai_memories SET expires_at=1 WHERE id=1",
        "DELETE FROM ai_memories WHERE id=1",
    ] {
        let (store, _) = database().await;
        store
            .call(move |db| {
                let reply = message(db, 1, "assistant", 1)?;
                save_reply_guard(db, &reply, &memory_guard())?;
                assert!(reply_is_valid(db, &reply)?);
                db.execute(mutation, [])?;
                assert!(
                    !reply_is_valid(db, &reply)?,
                    "accepted invalid knowledge after {mutation}"
                );
                assert!(memory_guard().validate(db).is_err());
                Ok(())
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn source_tombstones_invalidate_transitively_after_original_message_retention() {
    let (store, _) = database().await;
    store
        .call(|db| {
            let source = message(db, 1, "user", 1)?;
            let reply = message(db, 2, "assistant", 1)?;
            let derived = message(db, 3, "assistant", 1)?;
            let mut original = memory_guard();
            original.include_sources(db, std::slice::from_ref(&source))?;
            save_reply_guard(db, &reply, &original)?;
            let mut inherited = ContextDependencyGuard::default();
            inherited.include_sources(db, std::slice::from_ref(&reply))?;
            save_reply_guard(db, &derived, &inherited)?;

            db.execute("DELETE FROM messages WHERE key=?1", [&source])?;
            assert!(
                reply_is_valid(db, &reply)?,
                "ordinary retention is not permanent revocation"
            );
            assert!(reply_is_valid(db, &derived)?);
            db.execute(
                "INSERT INTO ai_revoked_sources(source_key,revoked_at) VALUES(?1,?2)",
                params![source, now()],
            )?;
            assert!(!reply_is_valid(db, &reply)?);
            assert!(!reply_is_valid(db, &derived)?);
            assert!(inherited.validate(db).is_err());
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn summary_keeps_old_dependencies_across_incremental_compaction_and_retention() {
    let (store, memories) = database().await;
    first_summary(&store, &memories).await;
    store
        .call(|db| {
            message(db, 3, "user", 3)?;
            Ok(())
        })
        .await
        .unwrap();
    let batch = memories
        .prepare_batch(&conversation(), 3, false)
        .await
        .unwrap();
    assert_eq!(batch.previous_summary, "首轮包含人工资料的摘要");
    assert!(batch.sources.iter().all(|source| source.role == "user"));
    memories
        .commit_batch(
            batch,
            BatchUpdate {
                summary: "沿用首轮资料并加入后续话题".into(),
                changes: vec![],
            },
        )
        .await
        .unwrap();
    store
        .call(|db| {
            db.execute("DELETE FROM messages WHERE id<=2", [])?;
            maintain(db)?;
            let inherited = summary_guard(db, &conversation())?;
            assert_eq!(inherited.memories.len(), 1);
            assert!(
                inherited
                    .continuity
                    .source_keys()
                    .contains(&format!("{SOURCE_SCOPE}:1"))
            );
            db.execute("UPDATE ai_memories SET version=version+1 WHERE id=1", [])?;
            assert!(summary_guard(db, &conversation()).is_err());
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn settings_epoch_change_preserving_summary_does_not_drop_its_dependencies() {
    let (store, memories) = database().await;
    first_summary(&store, &memories).await;
    store
        .call(|db| {
            db.execute(
                "UPDATE ai_conversations SET epoch=epoch+1 WHERE id=?1",
                [CONVERSATION],
            )?;
            let changed = ContextGuard {
                conversation_id: CONVERSATION.into(),
                epoch: 2,
            };
            assert_eq!(summary_guard(db, &changed)?.memories.len(), 1);
            db.execute("UPDATE ai_memories SET active=0 WHERE id=1", [])?;
            assert!(summary_guard(db, &changed).is_err());
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn summary_dependency_capture_rejects_knowledge_changed_after_batch_freeze() {
    let (store, memories) = database().await;
    store
        .call(|db| {
            message(db, 1, "user", 1)?;
            let reply = message(db, 2, "assistant", 1)?;
            save_reply_guard(db, &reply, &memory_guard())
        })
        .await
        .unwrap();
    let batch = memories
        .prepare_batch(&conversation(), 1, false)
        .await
        .unwrap();
    assert!(
        batch
            .sources
            .iter()
            .any(|source| source.role == "assistant")
    );
    store
        .call(move |db| {
            db.execute("UPDATE ai_memories SET version=version+1 WHERE id=1", [])?;
            assert!(record_summary(db, &batch).is_err());
            let saved: i64 = db.query_row(
                "SELECT count(*) FROM ai_context_summary_dependencies",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(
                saved, 0,
                "a rejected snapshot must not create summary metadata"
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn summary_only_uses_visible_summary_eligible_sources() {
    let (store, memories) = database().await;
    store
        .call(|db| {
            message(db, 1, "user", 1)?;
            message(db, 2, "user", 2)?;
            db.execute(
                "UPDATE messages SET origin_plugin='memory_only',learnable=1 WHERE id=2",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let batch = memories
        .prepare_batch(&conversation(), 2, false)
        .await
        .unwrap();
    assert!(batch.sources.iter().any(|source| !source.context_visible));
    memories
        .commit_batch(
            batch,
            BatchUpdate {
                summary: "只总结可见对话".into(),
                changes: vec![],
            },
        )
        .await
        .unwrap();
    store
        .call(|db| {
            let captured = summary_guard(db, &conversation())?;
            assert!(
                captured
                    .continuity
                    .source_keys()
                    .contains(&format!("{SOURCE_SCOPE}:1"))
            );
            assert!(
                !captured
                    .continuity
                    .source_keys()
                    .contains(&format!("{SOURCE_SCOPE}:2"))
            );
            db.execute(
                "INSERT INTO ai_revoked_sources(source_key,revoked_at) VALUES(?1,?2)",
                params![format!("{SOURCE_SCOPE}:2"), now()],
            )?;
            assert!(summary_guard(db, &conversation()).is_ok());
            db.execute(
                "INSERT INTO ai_revoked_sources(source_key,revoked_at) VALUES(?1,?2)",
                params![format!("{SOURCE_SCOPE}:1"), now()],
            )?;
            assert!(summary_guard(db, &conversation()).is_err());
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn summary_repair_preserves_the_original_knowledge_dependencies() {
    let (store, memories) = database().await;
    first_summary(&store, &memories).await;
    store
        .call(|db| {
            db.execute(
                "UPDATE ai_conversations SET summary=?1 WHERE id=?2",
                params!["很长的摘要".repeat(400), CONVERSATION],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let repair = memories
        .summary_repair(&conversation(), 8000)
        .await
        .unwrap()
        .unwrap();
    memories
        .commit_summary_repair(repair, "压缩后的摘要".into())
        .await
        .unwrap();
    store
        .call(|db| {
            assert_eq!(summary_guard(db, &conversation())?.memories.len(), 1);
            db.execute("DELETE FROM ai_memories WHERE id=1", [])?;
            assert!(summary_guard(db, &conversation()).is_err());
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn invalid_summary_rebuilds_retained_prefix_without_relearning_processed_facts() {
    let (store, memories) = database().await;
    first_summary(&store, &memories).await;
    store.call(|db| {
        for position in 2..=55 {
            message(db, position + 1, "user", position)?;
        }
        db.execute("UPDATE messages SET learnable=1 WHERE role='user'", [])?;
        db.execute("UPDATE ai_conversations SET compacted_until=55,memory_processed_until=55 WHERE id=?1", [CONVERSATION])?;
        db.execute("UPDATE ai_memories SET version=version+1 WHERE id=1", [])?;
        Ok(())
    }).await.unwrap();
    let batch = memories
        .prepare_batch_budgeted(&conversation(), 55, true, true, 6000)
        .await
        .unwrap();
    assert_eq!(
        batch.previous_compacted_until, 55,
        "retain the stored CAS cursor"
    );
    assert_eq!(batch.previous_memory_processed_until, 55);
    assert!(
        batch.through_position < 55,
        "exercise a bounded partial rebuild"
    );
    assert!(
        batch.summary_enabled,
        "budget fitting must use the rebuild cursor"
    );
    assert!(!batch.memory_write);
    assert!(batch.previous_summary.is_empty());
    assert_eq!(batch.sources.first().unwrap().inbound_position, 1);
    assert!(
        batch
            .sources
            .iter()
            .all(|s| s.role == "user" && s.summary_eligible && !s.memory_eligible)
    );
    let through = batch.through_position;
    memories
        .commit_batch(
            batch,
            BatchUpdate {
                summary: "从仍保留的原始发言重新整理".into(),
                changes: vec![],
            },
        )
        .await
        .unwrap();
    store
        .call(move |db| {
            let cursors: (i64, i64) = db.query_row(
                "SELECT compacted_until,memory_processed_until FROM ai_conversations WHERE id=?1",
                [CONVERSATION],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!(cursors, (through, 55));
            assert!(summary_guard(db, &conversation()).is_ok());
            Ok(())
        })
        .await
        .unwrap();
    let next = memories
        .prepare_batch(&conversation(), 55, true)
        .await
        .unwrap();
    assert_eq!(next.previous_summary, "从仍保留的原始发言重新整理");
    assert!(!next.memory_write);
    assert!(
        next.sources
            .iter()
            .all(|s| s.inbound_position > through && !s.memory_eligible)
    );
}

#[tokio::test]
async fn batch_freezes_model_dependencies_and_excludes_them_from_prompt_payload() {
    let (store, memories) = database().await;
    store
        .call(|db| {
            message(db, 1, "user", 1)?;
            let reply = message(db, 2, "assistant", 1)?;
            save_reply_guard(db, &reply, &memory_guard())
        })
        .await
        .unwrap();
    let batch = memories
        .prepare_batch(&conversation(), 1, false)
        .await
        .unwrap();
    let encoded = serde_json::to_value(&batch).unwrap();
    assert!(encoded.get("dependencies").is_none());
    assert!(encoded.get("summary_dependencies").is_none());
    let frozen = batch.dependencies.clone();
    store
        .call(move |db| {
            frozen.validate(db)?;
            db.execute("UPDATE ai_memories SET version=version+1 WHERE id=1", [])?;
            assert!(frozen.validate(db).is_err());
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        memories
            .commit_batch(
                batch,
                BatchUpdate {
                    summary: "拒绝过期结果".into(),
                    changes: vec![],
                }
            )
            .await
            .is_err()
    );
    store
        .call(|db| {
            let summary: String = db.query_row(
                "SELECT summary FROM ai_conversations WHERE id=?1",
                [CONVERSATION],
                |r| r.get(0),
            )?;
            assert!(summary.is_empty());
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn allowed_memory_versions_are_part_of_the_frozen_background_request() {
    let (store, memories) = database().await;
    store
        .call(|db| {
            message(db, 1, "user", 1)?;
            db.execute("UPDATE messages SET learnable=1", [])?;
            db.execute(
                "UPDATE ai_memories SET origin='auto',locked=0 WHERE id=1",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let batch = memories
        .prepare_batch(&conversation(), 1, true)
        .await
        .unwrap();
    assert_eq!(batch.allowed_memories.len(), 1);
    let frozen = batch.dependencies.clone();
    store
        .call(move |db| {
            frozen.validate(db)?;
            db.execute("UPDATE ai_memories SET version=version+1 WHERE id=1", [])?;
            assert!(frozen.validate(db).is_err());
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        memories
            .commit_batch(
                batch,
                BatchUpdate {
                    summary: "不得使用被修改的旧知识".into(),
                    changes: vec![],
                }
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn invalid_summary_is_never_repaired_and_frozen_repairs_reject_dependency_changes() {
    let (store, memories) = database().await;
    first_summary(&store, &memories).await;
    store
        .call(|db| {
            db.execute(
                "UPDATE ai_conversations SET summary=?1 WHERE id=?2",
                params!["过长的原始摘要".repeat(300), CONVERSATION],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let repair = memories
        .summary_repair(&conversation(), 8000)
        .await
        .unwrap()
        .unwrap();
    let frozen = repair.dependencies.clone();
    store
        .call(move |db| {
            frozen.validate(db)?;
            db.execute("UPDATE ai_memories SET version=version+1 WHERE id=1", [])?;
            assert!(frozen.validate(db).is_err());
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        memories
            .summary_repair(&conversation(), 8000)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        memories
            .commit_summary_repair(repair, "不得提交的压缩结果".into())
            .await
            .is_err()
    );
    store
        .call(|db| {
            let summary: String = db.query_row(
                "SELECT summary FROM ai_conversations WHERE id=?1",
                [CONVERSATION],
                |r| r.get(0),
            )?;
            assert!(summary.starts_with("过长的原始摘要"));
            Ok(())
        })
        .await
        .unwrap();
}

#[test]
fn old_databases_without_context_tables_accept_legacy_replies_but_keep_tombstones() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE messages(key TEXT PRIMARY KEY,revoked INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE ai_revoked_sources(source_key TEXT PRIMARY KEY,revoked_at INTEGER NOT NULL);",
    )
    .unwrap();
    assert!(reply_is_valid(&db, "legacy-reply").unwrap());
    summary_guard(&db, &conversation())
        .unwrap()
        .validate(&db)
        .unwrap();
    let mut guard = ContextDependencyGuard::default();
    guard
        .include_sources(&db, &["legacy-source".into()])
        .unwrap();
    guard.validate(&db).unwrap();
    db.execute(
        "INSERT INTO ai_revoked_sources VALUES('legacy-source',1)",
        [],
    )
    .unwrap();
    assert!(guard.validate(&db).is_err());
}

#[tokio::test]
async fn malformed_saved_dependencies_fail_closed() {
    let (store, _) = database().await;
    store
        .call(|db| {
            let reply = message(db, 1, "assistant", 1)?;
            db.execute(
                "INSERT INTO ai_context_reply_dependencies VALUES(?1,'invalid-json')",
                [&reply],
            )?;
            assert!(!reply_is_valid(db, &reply)?);
            db.execute(
                "INSERT INTO ai_context_summary_dependencies VALUES(?1,1,'invalid-json')",
                [CONVERSATION],
            )?;
            assert!(summary_guard(db, &conversation()).is_err());
            Ok(())
        })
        .await
        .unwrap();
}
