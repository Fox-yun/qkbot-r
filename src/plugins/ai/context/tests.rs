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

fn policy_state(db: &Connection, id: &str) -> Result<(i64, String, i64, i64, i64)> {
    Ok(db.query_row(
        "SELECT epoch,summary,compacted_until,memory_processed_until,memory_revision FROM ai_conversations WHERE id=?1",
        [id],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
    )?)
}

#[tokio::test]
async fn first_policy_install_preserves_allowed_history_and_expires_legacy_restrictions() {
    let (store, _) = database().await;
    store.call(|db| {
        db.execute("UPDATE ai_conversations SET summary='allowed',compacted_until=10,memory_processed_until=10 WHERE id=?1",[CONVERSATION])?;
        for (id, settings) in [("read-disabled", "{\"memory_read\":false}"), ("paused", "{\"paused\":true}")] {
            db.execute("INSERT INTO ai_conversations(id,scope,mode,settings,summary,compacted_until,memory_processed_until) VALUES(?1,?2,'group',?3,'legacy',10,10)",params![id,SOURCE_SCOPE,settings])?;
        }
        Ok(())
    }).await.unwrap();
    configure_policy(&store, &AiSettings::default())
        .await
        .unwrap();
    configure_policy(&store, &AiSettings::default())
        .await
        .unwrap();
    store
        .call(|db| {
            assert_eq!(
                policy_state(db, CONVERSATION)?,
                (1, "allowed".into(), 10, 10, 1)
            );
            for id in ["read-disabled", "paused"] {
                assert_eq!(policy_state(db, id)?, (2, String::new(), 0, 0, 1));
            }
            let rows: i64 = db.query_row("SELECT count(*) FROM ai_context_policy", [], |row| {
                row.get(0)
            })?;
            assert_eq!(rows, 1);
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn default_policy_changes_expire_only_effective_conversations_and_never_revive_outputs() {
    let (store, _) = database().await;
    learning::init(&store).await.unwrap();
    store.call(|db| {
        let source=message(db,1,"user",1)?;
        let original=message(db,2,"assistant",1)?;
        save_reply_guard(db,&original,&ContextDependencyGuard::default())?;
        db.execute("INSERT INTO ai_conversations(id,scope,mode,settings,summary) VALUES('explicit','1:group:30','group','{\"memory_read\":true,\"paused\":false}','derived summary')",[])?;
        db.execute("INSERT INTO messages(id,key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,origin_plugin) VALUES(3,'1:group:30:3','1:group:30','explicit',1,1,'assistant','derived',?1,3,'ai')",[now()])?;
        save_reply_guard(db,"1:group:30:3",&ContextDependencyGuard::default())?;
        db.execute("INSERT INTO ai_context_dependencies(derived_key,source_key,bot_id) VALUES('1:group:30:3',?1,1)",[&original])?;
        let mut summary=ContextDependencyGuard::default();
        summary.include_sources(db,&[original])?;
        db.execute("INSERT INTO ai_context_summary_dependencies VALUES('explicit',1,?1)",[serde_json::to_string(&summary)?])?;
        db.execute("UPDATE ai_conversations SET summary='original summary',compacted_until=2,memory_processed_until=2 WHERE id=?1",[CONVERSATION])?;
        db.execute("INSERT INTO ai_memory_provenance(memory_id,source_key,quote) VALUES(1,?1,'kept evidence')",[&source])?;
        db.execute("INSERT INTO ai_learning_notes(id,bot_id,kind,text,scope,locked,updated_at) VALUES(1,1,'experience','kept note','shared',1,1)",[])?;
        db.execute("INSERT INTO ai_learning_sources(note_id,source_key,detail) VALUES(1,?1,'kept evidence')",[source])?;
        Ok(())
    }).await.unwrap();
    let mut defaults = AiSettings::default();
    configure_policy(&store, &defaults).await.unwrap();
    defaults.memory_read = false;
    configure_policy(&store, &defaults).await.unwrap();
    store.call(|db| {
        assert_eq!(policy_state(db,CONVERSATION)?,(2,String::new(),0,0,2));
        assert_eq!(policy_state(db,"explicit")?,(1,"derived summary".into(),0,0,0));
        assert!(!reply_is_valid(db,&format!("{SOURCE_SCOPE}:2"))?);
        assert!(!reply_is_valid(db,"1:group:30:3")?);
        assert!(summary_guard(db,&ContextGuard{conversation_id:"explicit".into(),epoch:1}).is_err());
        let mut knowledge=ContextDependencyGuard::default();
        knowledge.include_memories(db,&[1])?;
        knowledge.validate(db)?;
        let note:(String,i64,bool,bool,bool)=db.query_row("SELECT text,version,locked,deleted,invalidated FROM ai_learning_notes WHERE id=1",[],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))?;
        assert_eq!(note,("kept note".into(),1,true,false,false));
        let memory:(String,i64,bool)=db.query_row("SELECT text,version,locked FROM ai_memories WHERE id=1",[],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
        assert_eq!(memory,("管理员补充的资料".into(),1,true));
        let raw:(i64,i64)=db.query_row("SELECT count(*),sum(epoch=1) FROM messages",[],|row| Ok((row.get(0)?,row.get(1)?)))?;
        assert_eq!(raw,(3,3));
        let users_invalidated:i64=db.query_row("SELECT count(*) FROM ai_context_invalidated i JOIN messages m ON m.key=i.source_key WHERE m.role='user'",[],|row| row.get(0))?;
        assert_eq!(users_invalidated,0);
        Ok(())
    }).await.unwrap();
    defaults.memory_read = true;
    configure_policy(&store, &defaults).await.unwrap();
    configure_policy(&store, &defaults).await.unwrap();
    defaults.input_budget *= 2;
    defaults.persona = "a different persona".into();
    configure_policy(&store, &defaults).await.unwrap();
    store
        .call(|db| {
            assert_eq!(policy_state(db, CONVERSATION)?, (3, String::new(), 0, 0, 3));
            assert_eq!(policy_state(db, "explicit")?.0, 1);
            assert!(!reply_is_valid(db, &format!("{SOURCE_SCOPE}:2"))?);
            assert!(!reply_is_valid(db, "1:group:30:3")?);
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn first_policy_install_honors_new_defaults_and_explicit_overrides() {
    for defaults in [
        AiSettings {
            memory_read: false,
            ..AiSettings::default()
        },
        AiSettings {
            paused: true,
            ..AiSettings::default()
        },
    ] {
        let (store, _) = database().await;
        store.call(|db| {
            db.execute("INSERT INTO ai_conversations(id,scope,mode,settings) VALUES('explicit','1:group:30','group','{\"memory_read\":true,\"paused\":false}')",[])?;
            Ok(())
        }).await.unwrap();
        configure_policy(&store, &defaults).await.unwrap();
        configure_policy(&store, &defaults).await.unwrap();
        store
            .call(|db| {
                assert_eq!(policy_state(db, CONVERSATION)?.0, 2);
                assert_eq!(policy_state(db, "explicit")?.0, 1);
                Ok(())
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn invalid_policy_data_rolls_back_all_epoch_and_default_changes() {
    let (store, _) = database().await;
    store.call(|db| {
        db.execute("INSERT INTO ai_conversations(id,scope,mode,settings) VALUES('z-invalid','1:group:30','group','{\"memory_read\":\"false\"}')",[])?;
        let defaults=AiSettings{memory_read:false,..AiSettings::default()};
        assert!(configure_policy_in_transaction(db,&defaults).is_err());
        assert_eq!(policy_state(db,CONVERSATION)?.0,1);
        let rows:i64=db.query_row("SELECT count(*) FROM ai_context_policy",[],|row| row.get(0))?;
        assert_eq!(rows,0);
        Ok(())
    }).await.unwrap();
}

#[tokio::test]
async fn group_public_recall_only_uses_a_members_facts_from_this_group() {
    let (store, _memories) = database().await;
    store
        .call(|db| {
            let other = "1:group:21:v0:group";
            let private = "1:friend:8:v0:user:8";
            db.execute("INSERT INTO ai_conversations(id,scope,mode,epoch) VALUES(?1,'1:group:21','group',1),(?2,'1:friend:8','user',1)", params![other, private])?;
            let insert = |id: i64, conversation: &str, scope: &str, seq: i64, text: &str, origin: &str| -> Result<()> {
                let key = format!("{scope}:{seq}");
                db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,learnable) VALUES(?1,?2,?3,1,8,'user',?4,?5,1)", params![key, scope, conversation, text, now()])?;
                let evidence = serde_json::to_string(&serde_json::json!([{"source_key":key,"quote":text}]))?;
                db.execute("INSERT INTO ai_memories(id,conversation_id,subject_id,kind,text,evidence,created_at,updated_at,origin) VALUES(?1,?2,8,'preference',?3,?4,?5,?5,?6)", params![id, conversation, text, evidence, now(), origin])?;
                db.execute("INSERT INTO ai_memory_provenance(memory_id,source_key,quote) VALUES(?1,?2,?3)", params![id, key, text])?;
                db.execute("INSERT INTO ai_memory_search(memory_id,terms) VALUES(?1,?2)", params![id, memory::lexical_terms(text).join(" ")])?;
                Ok(())
            };
            insert(10, CONVERSATION, SOURCE_SCOPE, 100, "喜欢吃火锅", "auto")?;
            insert(11, other, "1:group:21", 100, "喜欢吃火锅底料", "auto")?;
            insert(12, private, "1:friend:8", 100, "喜欢吃火锅丸子", "auto")?;
            insert(13, CONVERSATION, SOURCE_SCOPE, 101, "喜欢吃火锅蘸料", "manual")?;
            let audience = Scope { bot_id: 1, scene: Scene::Group, peer_id: 20 };
            let found = group_public(db, &conversation(), &audience, &[8], "火锅")?;
            let texts: Vec<&str> = found.iter().map(|k| k.text.as_str()).collect();
            assert_eq!(texts.len(), 1, "{texts:?}");
            assert!(texts[0].contains("群成员 8 在本群公开说过") && texts[0].ends_with("喜欢吃火锅"));
            assert!(matches!(found[0].dependency, KnowledgeDependency::Memory(MemoryDependency { id: 10, .. })));
            Ok(())
        })
        .await
        .unwrap();
    store.stop().await.unwrap();
}
