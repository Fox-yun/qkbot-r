use qkbot::{
    config::{Config, CrossScenePolicy, PersonalConfig},
    host::Host,
    plugins::ai::{
        context,
        memory::{MemoryBatch, lifecycle::LearningUpdate},
        personal::{self, Action, Mutation},
    },
    types::{ContextGuard, Scene, Scope, now},
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::sync::Arc;

async fn setup() -> Arc<Host> {
    let mut c = Config::default();
    c.app.database = ":memory:".into();
    c.llm.model = "synthetic".into();
    c.llm.chat_completions_url = "http://127.0.0.1:9/v1/chat/completions".into();
    c.plugins.native.enabled.clear();
    Host::create(c).await.unwrap()
}
fn scope(scene: Scene, peer: i64) -> Scope {
    Scope {
        bot_id: 1,
        scene,
        peer_id: peer,
    }
}
fn seed(
    db: &Connection,
    scene: Scene,
    peer: i64,
    user: i64,
    id: i64,
    text: &str,
    direct: bool,
) -> anyhow::Result<ContextGuard> {
    let scope = scope(scene, peer).key();
    let conv = if scene == Scene::Friend {
        format!("{scope}:v0:user:{user}")
    } else {
        format!("{scope}:v0:group")
    };
    db.execute(
        "INSERT OR IGNORE INTO ai_conversations(id,scope,mode,owner_id) VALUES(?1,?2,?3,?4)",
        params![
            conv,
            scope,
            if scene == Scene::Friend {
                "user"
            } else {
                "group"
            },
            if scene == Scene::Friend {
                Some(user)
            } else {
                None
            }
        ],
    )?;
    db.execute("INSERT INTO messages(id,key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,learnable) VALUES(?1,?2,?3,?4,1,?5,'user',?6,?7,?1,1)",params![id,format!("{scope}:{id}"),scope,conv,user,text,now()])?;
    context::continuity::register_incoming(db, &format!("{scope}:{id}"), direct, true)?;
    Ok(ContextGuard {
        conversation_id: conv,
        epoch: 1,
    })
}
fn candidate(key: &str, kind: &str, value: &str, quote: &str) -> Value {
    json!({"subject_id":7,"target_id":1,"kind":kind,"value":value,"evidence":{"source_key":key,"quote":quote},"ttl_days":null})
}
async fn batch(
    host: &Host,
    scene: Scene,
    peer: i64,
    id: i64,
    text: &'static str,
    direct: bool,
) -> MemoryBatch {
    let guard = host
        .store
        .call(move |db| seed(db, scene, peer, 7, id, text, direct))
        .await
        .unwrap();
    host.memory.prepare_batch(&guard, id, true).await.unwrap()
}
async fn commit(host: &Host, batch: MemoryBatch, relations: Vec<Value>) {
    let update: LearningUpdate =
        serde_json::from_value(json!({"summary":"","changes":[],"relationship_changes":relations}))
            .unwrap();
    host.memory
        .commit_learning_batch(batch, update)
        .await
        .unwrap();
}

#[tokio::test]
async fn sourced_preferences_obey_person_bot_character_and_audience() {
    let host = setup().await;
    let b = batch(&host, Scene::Friend, 7, 1, "在私聊里请叫我阿云", true).await;
    commit(
        &host,
        b,
        vec![candidate("1:friend:7:1", "address", "阿云", "请叫我阿云")],
    )
    .await;
    host.store
        .call(|db| {
            let c = PersonalConfig::default();
            let private = scope(Scene::Friend, 7);
            let group = scope(Scene::Group, 2);
            let value =
                personal::recall(db, &private, 7, &c, CrossScenePolicy::PrivateFollowup, None)?
                    .unwrap();
            assert!(value.text.contains("阿云"));
            value.dependency.validate(db)?;
            assert!(
                personal::recall(db, &group, 7, &c, CrossScenePolicy::PrivateFollowup, None)?
                    .is_none()
            );
            assert!(personal::recall(db, &group, 7, &c, CrossScenePolicy::Shared, None)?.is_some());
            assert!(
                personal::recall(db, &private, 8, &c, CrossScenePolicy::Shared, None)?.is_none()
            );
            assert!(
                personal::recall(
                    db,
                    &Scope {
                        bot_id: 2,
                        ..private.clone()
                    },
                    7,
                    &c,
                    CrossScenePolicy::Shared,
                    None
                )?
                .is_none()
            );
            assert!(
                personal::recall(
                    db,
                    &private,
                    7,
                    &PersonalConfig {
                        character_id: "other".into(),
                        ..c
                    },
                    CrossScenePolicy::Shared,
                    None
                )?
                .is_none()
            );
            assert!(!value.dependency.allows_audience(
                db,
                CrossScenePolicy::PrivateFollowup,
                &group,
                7
            )?);
            Ok(())
        })
        .await
        .unwrap();
    let other = PersonalConfig {
        character_id: "other".into(),
        ..PersonalConfig::default()
    };
    personal::configure(&host.store, &other).await.unwrap();
    personal::configure(&host.store, &PersonalConfig::default())
        .await
        .unwrap();
    host.store
        .call(|db| {
            assert!(
                personal::recall(
                    db,
                    &scope(Scene::Friend, 7),
                    7,
                    &PersonalConfig::default(),
                    CrossScenePolicy::PrivateFollowup,
                    None
                )?
                .is_some()
            );
            Ok(())
        })
        .await
        .unwrap();
    host.store.stop().await.unwrap();
}

#[tokio::test]
async fn invalid_relationship_candidate_preserves_valid_siblings_and_facts() {
    let host = setup().await;
    let b = batch(&host, Scene::Group, 2, 1, "请先说结论，我喜欢白茶", true).await;
    let mut impostor = candidate("1:group:2:1", "address", "别人的称呼", "请先说结论");
    impostor["subject_id"] = json!(8);
    let mut target = candidate("1:group:2:1", "boundary", "对旁人的评价", "请先说结论");
    target["target_id"] = json!(8);
    let update=serde_json::from_value(json!({"summary":"","changes":[{"op":"upsert","subject_id":7,"kind":"preference","text":"喜欢白茶","evidence":[{"source_key":"1:group:2:1","quote":"我喜欢白茶"}],"ttl_days":null}],"relationship_changes":[impostor,target,candidate("1:group:2:1","style","先说结论","请先说结论")]})).unwrap();
    host.memory.commit_learning_batch(b, update).await.unwrap();
    host.store
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT count(*) FROM ai_relationship_events", [], |r| r
                    .get::<_, i64>(0))?,
                1
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM ai_memories", [], |r| r
                    .get::<_, i64>(0))?,
                1
            );
            assert_eq!(
                db.query_row(
                    "SELECT rejected_count FROM ai_memory_batch_results",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                2
            );
            Ok(())
        })
        .await
        .unwrap();
    let b = batch(&host, Scene::Group, 2, 2, "请叫我另一个名字", false).await;
    commit(
        &host,
        b,
        vec![candidate(
            "1:group:2:2",
            "address",
            "另一个名字",
            "请叫我另一个名字",
        )],
    )
    .await;
    host.store
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT count(*) FROM ai_relationship_events", [], |r| r
                    .get::<_, i64>(0))?,
                1
            );
            Ok(())
        })
        .await
        .unwrap();
    host.store.stop().await.unwrap();
}

#[tokio::test]
async fn corrections_are_idempotent_independent_and_invalidate_derived_context() {
    let host = setup().await;
    let b = batch(&host, Scene::Group, 2, 1, "请叫我阿云", true).await;
    commit(
        &host,
        b,
        vec![candidate("1:group:2:1", "address", "阿云", "请叫我阿云")],
    )
    .await;
    host.store
        .call(|db| {
            let context = personal::recall(
                db,
                &scope(Scene::Friend, 7),
                7,
                &PersonalConfig::default(),
                CrossScenePolicy::PrivateFollowup,
                None,
            )?
            .unwrap();
            let revision: i64 = db.query_row(
                "SELECT memory_revision FROM ai_conversations LIMIT 1",
                [],
                |r| r.get(0),
            )?;
            let request = Mutation {
                bot_id: "1".into(),
                user_id: "7".into(),
                character_id: "default".into(),
                revision: 1,
                request_id: "correction".into(),
                action: Action::Correct {
                    id: 1,
                    version: 1,
                    value: "云同学".into(),
                },
            };
            let result = personal::mutate(db, 1, &request, "admin", "session")?;
            assert_eq!(
                result,
                personal::mutate(db, 1, &request, "admin", "session")?
            );
            assert!(context.dependency.validate(db).is_err());
            assert_eq!(
                db.query_row(
                    "SELECT memory_revision FROM ai_conversations LIMIT 1",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                revision
            );
            assert!(
                personal::recall(
                    db,
                    &scope(Scene::Friend, 7),
                    7,
                    &PersonalConfig::default(),
                    CrossScenePolicy::PrivateFollowup,
                    None
                )?
                .unwrap()
                .text
                .contains("云同学")
            );
            assert!(personal::mutate(db, 2, &request, "admin", "session").is_err());
            let stale = Mutation {
                request_id: "stale".into(),
                ..request
            };
            assert!(personal::mutate(db, 1, &stale, "admin", "session").is_err());
            Ok(())
        })
        .await
        .unwrap();
    host.store.stop().await.unwrap();
}

#[tokio::test]
async fn revoked_or_disabled_sources_remove_preferences_even_after_raw_retention() {
    let host = setup().await;
    let b = batch(&host, Scene::Group, 2, 1, "不要拿我的工作开玩笑", true).await;
    commit(
        &host,
        b,
        vec![candidate(
            "1:group:2:1",
            "boundary",
            "不要拿工作开玩笑",
            "不要拿我的工作开玩笑",
        )],
    )
    .await;
    host.store
        .call(|db| {
            let cfg = PersonalConfig::default();
            let target = scope(Scene::Friend, 7);
            db.execute("DELETE FROM messages", [])?;
            let context = personal::recall(
                db,
                &target,
                7,
                &cfg,
                CrossScenePolicy::PrivateFollowup,
                None,
            )?
            .unwrap();
            db.execute(
                "UPDATE ai_conversations SET settings='{\"memory_read\":false}'",
                [],
            )?;
            assert!(
                personal::recall(
                    db,
                    &target,
                    7,
                    &cfg,
                    CrossScenePolicy::PrivateFollowup,
                    None
                )?
                .is_none()
            );
            assert!(context.dependency.validate(db).is_err());
            db.execute("UPDATE ai_conversations SET settings='{}'", [])?;
            db.execute("INSERT INTO ai_revoked_sources VALUES('1:group:2:1',1)", [])?;
            context::continuity::invalidate_sources(db, &["1:group:2:1".into()])?;
            assert!(
                personal::recall(
                    db,
                    &target,
                    7,
                    &cfg,
                    CrossScenePolicy::PrivateFollowup,
                    None
                )?
                .is_none()
            );
            assert_eq!(
                db.query_row("SELECT value||quote FROM ai_relationship_events", [], |r| r
                    .get::<_, String>(0))?,
                ""
            );
            Ok(())
        })
        .await
        .unwrap();
    host.store.stop().await.unwrap();
}

#[tokio::test]
async fn reset_blocks_frozen_relationship_proposals() {
    let host = setup().await;
    let b = batch(&host, Scene::Group, 2, 1, "请叫我阿云", true).await;
    host.store
        .call(|db| {
            personal::mutate(
                db,
                1,
                &Mutation {
                    bot_id: "1".into(),
                    user_id: "7".into(),
                    character_id: "default".into(),
                    revision: 0,
                    request_id: "reset".into(),
                    action: Action::Reset,
                },
                "admin",
                "session",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    commit(
        &host,
        b,
        vec![candidate("1:group:2:1", "address", "阿云", "请叫我阿云")],
    )
    .await;
    host.store
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT count(*) FROM ai_relationship_events", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            assert_eq!(
                db.query_row("SELECT reason FROM ai_memory_candidate_results", [], |r| {
                    r.get::<_, String>(0)
                })?,
                "relationship_reset"
            );
            Ok(())
        })
        .await
        .unwrap();
    host.store.stop().await.unwrap();
}

#[tokio::test]
async fn interaction_days_are_capped_and_never_modify_fact_revisions() {
    let host = setup().await;
    host.store
        .call(|db| {
            seed(db, Scene::Group, 2, 7, 1, "你好", true)?;
            seed(db, Scene::Group, 2, 7, 2, "你好", true)?;
            let revision: i64 = db.query_row(
                "SELECT memory_revision FROM ai_conversations LIMIT 1",
                [],
                |r| r.get(0),
            )?;
            personal::record_interaction(db, "1:group:2:1", "1:group:2:100")?;
        assert_eq!(db.query_row("SELECT count(*) FROM ai_relationship_events",[],|r|r.get::<_,i64>(0))?,0);
        db.execute("INSERT INTO ai_recent_turns(bot_id,actor_id,request_position,request_key,reply_key,completed_at,completed_position,dependencies) VALUES(1,7,1,'1:group:2:1','1:group:2:100',1,3,'{}'),(1,7,2,'1:group:2:2','1:group:2:101',1,4,'{}')",[])?;
        personal::record_interaction(db, "1:group:2:1", "1:group:2:100")?;
            personal::record_interaction(db, "1:group:2:2", "1:group:2:101")?;
            assert_eq!(
                db.query_row("SELECT count(*) FROM ai_relationship_events", [], |r| r
                    .get::<_, i64>(0))?,
                1
            );
            assert_eq!(
                db.query_row(
                    "SELECT memory_revision FROM ai_conversations LIMIT 1",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                revision
            );
            let value = personal::recall(
                db,
                &scope(Scene::Friend, 7),
                7,
                &PersonalConfig::default(),
                CrossScenePolicy::PrivateFollowup, None
            )?
            .unwrap();
            assert!(value.text.contains("少量直接交流"));
            let tiny = PersonalConfig {
                injection_budget: 10,
                ..PersonalConfig::default()
            };
            assert!(
                personal::recall(
                    db,
                    &scope(Scene::Friend, 7),
                    7,
                    &tiny,
                    CrossScenePolicy::PrivateFollowup, None
                )?
                .is_none()
            );
            Ok(())
        })
        .await
        .unwrap();
    host.store.stop().await.unwrap();
}
