//! Profile validation and fallback recall use only synthetic in-memory records.
use qkbot::{
    config::Config,
    host::Host,
    plugins::ai::memory::{BatchUpdate, Evidence, MemoryChange, MemoryKind, lexical_terms},
    types::{ContextGuard, now},
};
use rusqlite::{Connection, params};
use std::sync::Arc;

async fn setup() -> Arc<Host> {
    let mut config = Config::default();
    config.app.database = ":memory:".into();
    config.plugins.ai.enabled = false;
    config.plugins.native.enabled.clear();
    Host::create(config).await.unwrap()
}

fn guard(id: &str) -> ContextGuard {
    ContextGuard {
        conversation_id: id.into(),
        epoch: 1,
    }
}

fn seed_conversation(db: &Connection, id: &str, scope: &str) -> anyhow::Result<()> {
    db.execute(
        "INSERT INTO ai_conversations(id,scope,mode,summary) VALUES(?1,?2,'group','保留原摘要')",
        params![id, scope],
    )?;
    Ok(())
}

fn seed_sources(db: &Connection, id: &str) -> anyhow::Result<()> {
    seed_conversation(db, id, "1:group:2")?;
    for (position, actor, role, text) in [
        (1, 7, "user", "先给结论，解释放后面。"),
        (2, 7, "user", "结论先说，接着补依据。"),
        (3, 8, "user", "我也希望先看到结论。"),
        (4, 1, "assistant", "好的，先给结论。"),
    ] {
        db.execute(
            "INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,learnable)
             VALUES(?1,'1:group:2',?2,1,?3,?4,?5,?6,?7,?8)",
            params![format!("{id}:{position}"), id, actor, role, text, now(), position, role == "user"],
        )?;
    }
    Ok(())
}

fn evidence(id: &str, position: i64, quote: &str) -> Evidence {
    Evidence {
        source_key: format!("{id}:{position}"),
        quote: quote.into(),
    }
}

fn observation(kind: MemoryKind, evidence: Vec<Evidence>, ttl_days: Option<u16>) -> MemoryChange {
    MemoryChange::Upsert {
        id: None,
        subject_id: 7,
        kind,
        text: "聊天观察：两次交流中都要求先给结论，再补说明。".into(),
        evidence,
        ttl_days,
    }
}

#[tokio::test]
async fn two_distinct_human_messages_allow_observations_at_both_ttl_boundaries() {
    let host = setup().await;
    for days in [7, 30] {
        let id = format!("valid-{days}");
        let seed_id = id.clone();
        host.store
            .call(move |db| seed_sources(db, &seed_id))
            .await
            .unwrap();
        let context = guard(&id);
        let batch = host.memory.prepare_batch(&context, 2, true).await.unwrap();
        assert_eq!(batch.sources.len(), 2);
        assert!(batch.sources.iter().all(|source| source.memory_eligible));
        let original_evidence = vec![evidence(&id, 1, "先给结论"), evidence(&id, 2, "结论先说")];
        let before = now();
        host.memory
            .commit_batch(
                batch,
                BatchUpdate {
                    summary: "用户7两次要求先给结论。".into(),
                    changes: vec![observation(
                        MemoryKind::Preference,
                        original_evidence.clone(),
                        Some(days),
                    )],
                },
            )
            .await
            .unwrap();
        let after = now();
        let recalled = host
            .memory
            .retrieve(&context, 7, "", 5, 1000)
            .await
            .unwrap();
        assert_eq!(recalled.len(), 1);
        assert_eq!(recalled[0].kind, MemoryKind::Preference);
        assert_eq!(recalled[0].subject_id, 7);
        assert_eq!(recalled[0].evidence, original_evidence);
        let ttl = i64::from(days) * 86_400;
        assert!((before + ttl..=after + ttl).contains(&recalled[0].expires_at.unwrap()));
        host.store.call(move |db| {
            let state: (i64, i64, String) = db.query_row(
                "SELECT compacted_until,memory_processed_until,summary FROM ai_conversations WHERE id=?1",
                [&id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
            )?;
            assert_eq!(state, (2, 2, "用户7两次要求先给结论。".into()));
            assert_eq!(db.query_row("SELECT COUNT(*) FROM ai_memories WHERE conversation_id=?1", [&id], |row| row.get::<_, i64>(0))?, 1);
            Ok(())
        }).await.unwrap();
    }
    host.store.stop().await.unwrap();
}

#[tokio::test]
async fn invalid_observations_reject_the_whole_batch_without_advancing_cursors() {
    let host = setup().await;
    host.store
        .call(|db| seed_sources(db, "invalid"))
        .await
        .unwrap();
    let context = guard("invalid");
    let first = evidence("invalid", 1, "先给结论");
    let second = evidence("invalid", 2, "结论先说");
    let both = vec![first.clone(), second];
    let cases = [
        (
            "one source",
            MemoryKind::Preference,
            vec![first.clone()],
            Some(7),
        ),
        (
            "duplicate source",
            MemoryKind::Preference,
            vec![first.clone(), first.clone()],
            Some(7),
        ),
        ("permanent", MemoryKind::Preference, both.clone(), None),
        ("too short", MemoryKind::Preference, both.clone(), Some(6)),
        ("too long", MemoryKind::Preference, both.clone(), Some(31)),
        ("wrong kind", MemoryKind::Fact, both, Some(7)),
        (
            "another human",
            MemoryKind::Preference,
            vec![first.clone(), evidence("invalid", 3, "先看到结论")],
            Some(7),
        ),
        (
            "assistant source",
            MemoryKind::Preference,
            vec![first.clone(), evidence("invalid", 4, "先给结论")],
            Some(7),
        ),
    ];
    for (label, kind, sources, ttl) in cases {
        let batch = host.memory.prepare_batch(&context, 4, true).await.unwrap();
        let error = host
            .memory
            .commit_batch(
                batch,
                BatchUpdate {
                    summary: "不能落库的新摘要".into(),
                    changes: vec![
                        MemoryChange::Upsert {
                            id: None,
                            subject_id: 7,
                            kind: MemoryKind::Preference,
                            text: "希望回答先讲结论。".into(),
                            evidence: vec![first.clone()],
                            ttl_days: None,
                        },
                        observation(kind, sources, ttl),
                    ],
                },
            )
            .await
            .expect_err(label);
        assert!(
            error.to_string().contains("chat observations")
                || error
                    .to_string()
                    .contains("memory evidence must quote its human subject exactly"),
            "{label}: {error}"
        );
        host.store.call(move |db| {
            let state: (i64, i64, i64, String) = db.query_row(
                "SELECT compacted_until,memory_processed_until,memory_revision,summary FROM ai_conversations WHERE id='invalid'",
                [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
            )?;
            assert_eq!(state, (0, 0, 0, "保留原摘要".into()), "{label}");
            for table in ["ai_memories", "ai_memory_search", "ai_memory_provenance"] {
                assert_eq!(db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get::<_, i64>(0))?, 0, "{label}: {table}");
            }
            Ok(())
        }).await.unwrap();
    }
    host.store.stop().await.unwrap();
}

fn seed_background(
    db: &Connection,
    conversation: &str,
    subject: i64,
    text: &str,
    expires: Option<i64>,
    active: bool,
) -> anyhow::Result<i64> {
    db.execute(
        "INSERT INTO ai_memories(conversation_id,subject_id,kind,text,evidence,created_at,updated_at,expires_at,active,origin)
         VALUES(?1,?2,'fact',?3,'[]',?4,?4,?5,?6,'auto')",
        params![conversation, subject, text, now(), expires, active],
    )?;
    let id = db.last_insert_rowid();
    db.execute(
        "INSERT INTO ai_memory_search(memory_id,terms) VALUES(?1,?2)",
        params![id, lexical_terms(text).join(" ")],
    )?;
    Ok(id)
}

#[tokio::test]
async fn background_fact_fallback_is_scoped_and_excludes_expired_or_ordinary_facts() {
    let host = setup().await;
    let (own, foreign, other, expired) = host.store.call(|db| {
        seed_conversation(db, "main", "1:group:2")?;
        seed_conversation(db, "other", "1:group:3")?;
        let own = seed_background(db, "main", 7, "人物背景：自述在学习机械设计。", None, true)?;
        let foreign = seed_background(db, "main", 8, "人物背景：自述从事园艺工作。", None, true)?;
        let other = seed_background(db, "other", 7, "人物背景：另一个会话中的背景。", None, true)?;
        let expired = seed_background(db, "main", 7, "人物背景：已过期的旧背景。", Some(now() - 1), true)?;
        seed_background(db, "main", 7, "人物背景：不活跃的旧背景。", None, false)?;
        seed_background(db, "main", 7, "普通事实，不应因没有匹配词就自动召回。", None, true)?;
        assert_eq!(db.query_row("SELECT COUNT(*) FROM ai_memory_search WHERE ai_memory_search MATCH 'zzzxqvnohit'", [], |row| row.get::<_, i64>(0))?, 0);
        Ok((own, foreign, other, expired))
    }).await.unwrap();
    for query in ["zzzxqvnohit", ""] {
        let recalled = host
            .memory
            .retrieve(&guard("main"), 7, query, 10, 10_000)
            .await
            .unwrap();
        assert_eq!(
            recalled.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![own]
        );
        assert_eq!(recalled[0].kind, MemoryKind::Fact);
    }
    for (conversation, subject, expected) in [("main", 8, foreign), ("other", 7, other)] {
        let recalled = host
            .memory
            .retrieve(&guard(conversation), subject, "", 10, 10_000)
            .await
            .unwrap();
        assert_eq!(
            recalled.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![expected]
        );
    }
    assert!(
        host.memory
            .retrieve(&guard("main"), 9, "", 10, 10_000)
            .await
            .unwrap()
            .is_empty()
    );
    host.store
        .call(move |db| {
            assert_eq!(
                db.query_row(
                    "SELECT COUNT(*) FROM ai_memory_search WHERE memory_id=?1",
                    [expired],
                    |row| row.get::<_, i64>(0)
                )?,
                0
            );
            assert_eq!(
                db.query_row(
                    "SELECT COUNT(*) FROM ai_memories WHERE id=?1",
                    [expired],
                    |row| row.get::<_, i64>(0)
                )?,
                1
            );
            Ok(())
        })
        .await
        .unwrap();
    host.store.stop().await.unwrap();
}
