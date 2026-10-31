use super::*;
use crate::{
    config::Config,
    host::Host,
    plugins::ai::{context::continuity, memory::lifecycle::LearningUpdate},
    types::{ContextGuard, Scene, Scope},
};
use serde_json::json;
use std::sync::Arc;

async fn host_with(edit: impl FnOnce(&mut Config)) -> Arc<Host> {
    let mut c = Config::default();
    c.app.database = ":memory:".into();
    c.llm.model = "synthetic".into();
    c.llm.chat_completions_url = "http://127.0.0.1:9/v1/chat/completions".into();
    c.plugins.native.enabled.clear();
    edit(&mut c);
    Host::create(c).await.unwrap()
}

fn scope(scene: Scene, peer: i64) -> Scope {
    Scope {
        bot_id: 1,
        scene,
        peer_id: peer,
    }
}

fn conversation(db: &Connection, scope: &Scope, owner: Option<i64>) -> Result<ContextGuard> {
    let key = scope.key();
    let id = match owner {
        Some(user) => format!("{key}:v0:user:{user}"),
        None => format!("{key}:v0:group"),
    };
    db.execute(
        "INSERT OR IGNORE INTO ai_conversations(id,scope,mode,owner_id) VALUES(?1,?2,?3,?4)",
        params![
            id,
            key,
            if owner.is_some() { "user" } else { "group" },
            owner
        ],
    )?;
    Ok(ContextGuard {
        conversation_id: id,
        epoch: 1,
    })
}

/// A human message (direct when asked) at an explicit time and position.
#[allow(clippy::too_many_arguments)]
fn human(
    db: &Connection,
    scope: &Scope,
    guard: &ContextGuard,
    id: i64,
    user: i64,
    text: &str,
    at: i64,
    direct: bool,
) -> Result<String> {
    let key = format!("{}:{id}", scope.key());
    db.execute("INSERT INTO messages(id,key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,learnable) VALUES(?1,?2,?3,?4,1,?5,'user',?6,?7,?1,1)",
        params![id, key, scope.key(), guard.conversation_id, user, text, at])?;
    continuity::register_incoming(db, &key, direct, true)?;
    Ok(key)
}

fn reply(
    db: &Connection,
    scope: &Scope,
    guard: &ContextGuard,
    id: i64,
    trigger: i64,
    text: &str,
    at: i64,
) -> Result<String> {
    let key = format!("{}:{id}", scope.key());
    db.execute("INSERT INTO messages(id,key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,origin_plugin,learnable) VALUES(?1,?2,?3,?4,1,1,'assistant',?5,?6,?7,'ai',0)",
        params![id, key, scope.key(), guard.conversation_id, text, at, trigger])?;
    Ok(key)
}

fn quote(db: &Connection, key: &str, target: &str) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO ai_context_relations(source_key,reply_to_key,mentioned_ids) VALUES(?1,?2,'[]')",
        params![key, target],
    )?;
    Ok(())
}

fn recall_texts(
    db: &Connection,
    target: &Scope,
    actor: i64,
    policy: CrossScenePolicy,
) -> Result<Vec<String>> {
    let domain = memory::scope::resolve(
        db,
        &conversation(db, target, (target.scene == Scene::Friend).then_some(actor))?
            .conversation_id,
    )?;
    Ok(recall(
        db,
        &RecallRequest {
            scope: target,
            actor,
            policy,
            domain: &domain,
            query: "",
            budget: 8000,
            reference: now(),
            personal: true,
        },
    )?
    .into_iter()
    .map(|r| r.text)
    .collect())
}

fn update(knowledge: Value) -> LearningUpdate {
    serde_json::from_value(json!({"summary":"","changes":[],"knowledge_changes":knowledge}))
        .unwrap()
}

#[tokio::test]
async fn episodes_and_statements_need_their_own_kind_of_evidence() {
    let host = host_with(|_| {}).await;
    let friend = scope(Scene::Friend, 7);
    let seed = friend.clone();
    let guard = host
        .store
        .call(move |db| {
            let friend = seed;
            let guard = conversation(db, &friend, Some(7))?;
            human(
                db,
                &friend,
                &guard,
                1,
                7,
                "下周三我要考驾照科目二",
                now(),
                true,
            )?;
            reply(
                db,
                &friend,
                &guard,
                2,
                1,
                "加油！考完告诉我结果，我请你喝虚拟奶茶。我自己最喜欢下雨天。",
                now(),
            )?;
            Ok(guard)
        })
        .await
        .unwrap();
    let batch = host.memory.prepare_batch(&guard, 1, true).await.unwrap();
    let policy = batch
        .knowledge
        .clone()
        .expect("confirmed reply enables extraction");
    assert_eq!(policy.bot_source_keys, vec!["1:friend:7:2".to_owned()]);
    host.memory
        .commit_learning_batch(
            batch,
            update(json!([
                {"kind":"episode","subject":"user","subject_id":7,"text":"机器人答应用户考完科目二后请喝虚拟奶茶","evidence":[{"source_key":"1:friend:7:1","quote":"考驾照科目二"},{"source_key":"1:friend:7:2","quote":"考完告诉我结果"}],"ttl_days":60,"importance":4},
                {"kind":"self","subject":"bot","subject_id":1,"text":"机器人说过自己最喜欢下雨天","evidence":[{"source_key":"1:friend:7:2","quote":"我自己最喜欢下雨天"}],"ttl_days":null},
                // No bot reply: not a shared experience.
                {"kind":"episode","subject":"user","subject_id":7,"text":"用户要考驾照","evidence":[{"source_key":"1:friend:7:1","quote":"考驾照"}],"ttl_days":null},
                // A person's words cannot be the bot's statement.
                {"kind":"self","subject":"bot","subject_id":1,"text":"机器人要考驾照","evidence":[{"source_key":"1:friend:7:1","quote":"考驾照"}],"ttl_days":null},
                // Another person cannot be the subject of this exchange.
                {"kind":"episode","subject":"user","subject_id":8,"text":"答应别人请喝奶茶","evidence":[{"source_key":"1:friend:7:1","quote":"科目二"},{"source_key":"1:friend:7:2","quote":"虚拟奶茶"}],"ttl_days":null},
                // Quotes must be verbatim.
                {"kind":"self","subject":"bot","subject_id":1,"text":"机器人讨厌晴天","evidence":[{"source_key":"1:friend:7:2","quote":"我讨厌晴天"}],"ttl_days":null}
            ])),
        )
        .await
        .unwrap();
    host.store
        .call(move |db| {
            let stored: Vec<(String, String, String)> = db
                .prepare("SELECT kind,text,(SELECT group_concat(role) FROM ai_knowledge_sources s WHERE s.knowledge_id=k.id) FROM ai_knowledge k ORDER BY id")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert_eq!(stored.len(), 2, "{stored:?}");
            assert_eq!(stored[0].0, "episode");
            assert!(stored[0].2.contains("evidence") && stored[0].2.contains("reply"));
            assert_eq!(stored[1].0, "self");
            let rejected: i64 = db.query_row("SELECT count(*) FROM ai_memory_candidate_results WHERE status='rejected' AND reason LIKE 'knowledge_%'", [], |r| r.get(0))?;
            assert_eq!(rejected, 4);
            // The person and this private chat see both.
            let texts = recall_texts(db, &friend, 7, CrossScenePolicy::PrivateFollowup)?;
            assert!(texts.iter().any(|t| t.contains("虚拟奶茶") && t.contains("共同经历")));
            assert!(texts.iter().any(|t| t.contains("下雨天") && t.contains("你说过的话")));
            // Another person's private chat sees neither.
            let other = scope(Scene::Friend, 8);
            assert!(recall_texts(db, &other, 8, CrossScenePolicy::Shared)?.is_empty());
            // Revoking the person's message removes the shared episode only.
            continuity::invalidate_sources(db, &["1:friend:7:1".into()])?;
            let texts = recall_texts(db, &friend, 7, CrossScenePolicy::PrivateFollowup)?;
            assert!(!texts.iter().any(|t| t.contains("虚拟奶茶")));
            assert!(texts.iter().any(|t| t.contains("下雨天")));
            Ok(())
        })
        .await
        .unwrap();
    host.store.stop().await.unwrap();
}

#[tokio::test]
async fn disabled_kinds_are_not_offered_to_extraction() {
    let host = host_with(|c| {
        c.plugins.ai.knowledge.episodes = false;
        c.plugins.ai.knowledge.self_memory = false;
    })
    .await;
    let friend = scope(Scene::Friend, 7);
    let guard = host
        .store
        .call(move |db| {
            let guard = conversation(db, &friend, Some(7))?;
            human(db, &friend, &guard, 1, 7, "你好", now(), true)?;
            reply(db, &friend, &guard, 2, 1, "你好呀", now())?;
            Ok(guard)
        })
        .await
        .unwrap();
    let batch = host.memory.prepare_batch(&guard, 1, true).await.unwrap();
    assert!(batch.knowledge.is_none());
    host.store.stop().await.unwrap();
}

#[tokio::test]
async fn fact_importance_and_reinforcement_stay_outside_memory_revisions() {
    let host = host_with(|_| {}).await;
    let friend = scope(Scene::Friend, 7);
    let guard = host
        .store
        .call(move |db| {
            let guard = conversation(db, &friend, Some(7))?;
            human(db, &friend, &guard, 1, 7, "我在杭州工作", now(), true)?;
            Ok(guard)
        })
        .await
        .unwrap();
    let batch = host.memory.prepare_batch(&guard, 1, true).await.unwrap();
    let first: LearningUpdate = serde_json::from_value(json!({"summary":"","changes":[{"op":"upsert","id":null,"subject_id":7,"kind":"fact","text":"在杭州工作","evidence":[{"source_key":"1:friend:7:1","quote":"我在杭州工作"}],"ttl_days":null,"importance":5}]})).unwrap();
    host.memory
        .commit_learning_batch(batch, first)
        .await
        .unwrap();
    let guard2 = guard.clone();
    let (id, revision) = host
        .store
        .call(move |db| {
            let id: i64 = db.query_row("SELECT id FROM ai_memories", [], |r| r.get(0))?;
            assert_eq!(
                stats::read(db, &format!("memory:{id}"))?.unwrap()["importance"],
                5
            );
            stats::record_recall(db, &[id], &[])?;
            let revision: i64 = db.query_row(
                "SELECT memory_revision FROM ai_conversations WHERE id=?1",
                [&guard2.conversation_id],
                |r| r.get(0),
            )?;
            human(
                db,
                &scope(Scene::Friend, 7),
                &guard2,
                2,
                7,
                "我还在杭州上班",
                now(),
                true,
            )?;
            Ok((id, revision))
        })
        .await
        .unwrap();
    // Recording use did not advance the memory domain revision.
    let batch = host.memory.prepare_batch(&guard, 2, true).await.unwrap();
    assert_eq!(batch.memory_revision, revision);
    let again: LearningUpdate = serde_json::from_value(json!({"summary":"","changes":[{"op":"upsert","id":id,"subject_id":7,"kind":"fact","text":"在杭州工作","evidence":[{"source_key":"1:friend:7:2","quote":"我还在杭州上班"}],"ttl_days":null}]})).unwrap();
    host.memory
        .commit_learning_batch(batch, again)
        .await
        .unwrap();
    host.store
        .call(move |db| {
            let stats = stats::read(db, &format!("memory:{id}"))?.unwrap();
            assert_eq!(stats["reinforced_count"], 1);
            assert_eq!(stats["recall_count"], 1);
            assert_eq!(stats["importance"], 5);
            let boosts = stats::memory_boosts(db, &[id])?;
            assert!(boosts[&id] > 0.0);
            Ok(())
        })
        .await
        .unwrap();
    host.store.stop().await.unwrap();
}

/// Three direct replies in a group, with reactions, past the outcome window.
fn seed_group(db: &Connection) -> Result<(Scope, Vec<String>)> {
    let group = scope(Scene::Group, 50);
    let guard = conversation(db, &group, None)?;
    let start = now() - 3600;
    let mut replies = Vec::new();
    for (i, (ask, answer, reactions)) in [
        (
            "小鱼讲个冷笑话",
            "企鹅为什么不怕冷？因为它穿着燕尾服。",
            vec![(8, "哈哈哈燕尾服笑死"), (9, "哈哈这个冷笑话可以")],
        ),
        (
            "小鱼再来一个",
            "鱼为什么不说话？因为它在水里冒泡泡。",
            vec![(8, "哈哈泡泡梗又来了"), (9, "泡泡梗笑死我了")],
        ),
        (
            "小鱼解释一下量子力学",
            "量子力学是研究微观粒子运动规律的物理学分支，涉及波粒二象性、不确定性原理……",
            vec![(7, "太长了，说简单点")],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let base = (i as i64) * 10 + 1;
        let at = start + (i as i64) * 600;
        human(db, &group, &guard, base, 7, ask, at, true)?;
        let key = reply(db, &group, &guard, base + 1, base, answer, at + 5)?;
        for (j, (user, text)) in reactions.into_iter().enumerate() {
            let reaction = human(
                db,
                &group,
                &guard,
                base + 2 + j as i64,
                user,
                text,
                at + 20 + j as i64,
                true,
            )?;
            quote(db, &reaction, &key)?;
        }
        replies.push(key);
    }
    Ok((group, replies))
}

#[tokio::test]
async fn outcomes_count_quotes_corrections_and_drive_participation_hints() {
    let host = host_with(|_| {}).await;
    host.store
        .call(|db| {
            let (group, replies) = seed_group(db)?;
            assert_eq!(outcomes::evaluate(db, 1, 300)?, 3);
            assert_eq!(outcomes::evaluate(db, 1, 300)?, 0, "each reply is evaluated once");
            let row = |key: &str| -> Result<(i64, i64, bool, bool)> {
                Ok(db.query_row("SELECT responders,quoted,corrected,positive FROM ai_reply_outcomes WHERE reply_key=?1", [key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?)
            };
            assert_eq!(row(&replies[0])?, (2, 2, false, true));
            assert_eq!(row(&replies[2])?, (1, 1, true, false));
            // Direct replies are not automatic participation.
            assert_eq!(outcomes::participation(db, &group.key(), 0)?.replies, 0);
            db.execute("UPDATE ai_reply_outcomes SET automatic=1", [])?;
            let participation = outcomes::participation(db, &group.key(), 0)?;
            assert_eq!(participation, outcomes::Participation { replies: 3, engaged: 3, corrected: 1 });
            // Revoking a reaction removes the outcome built on it.
            invalidate_sources(db, &["1:group:50:3".into()])?;
            assert_eq!(outcomes::participation(db, &group.key(), 0)?.replies, 2);
            Ok(())
        })
        .await
        .unwrap();
    host.store.stop().await.unwrap();
}

#[tokio::test]
async fn interaction_notes_need_quoted_reactions_from_enough_people() {
    let host = host_with(|c| c.plugins.ai.knowledge.behavior = true).await;
    host.store
        .call(|db| {
            let (group, replies) = seed_group(db)?;
            // A long reaction is excerpted for the model but hashed in full.
            db.execute(
                "UPDATE messages SET content=content||?1 WHERE key='1:group:50:13'",
                ["啊".repeat(600)],
            )?;
            outcomes::evaluate(db, 1, 300)?;
            let batch = behavior::testing::prepare_batch(db, 1)?.expect("enough outcomes");
            assert_eq!(batch["scope"], group.key());
            assert_eq!(batch["episodes"].as_array().unwrap().len(), 3);
            let (applied, rejected) = behavior::testing::commit_json(db, 1, json!({"changes":[
                {"op":"add","kind":"joke","text":"本群喜欢“泡泡”这类冷笑话梗，可以偶尔接上","confidence":0.8,
                 "evidence":[{"source_key":"1:group:50:13","quote":"泡泡梗又来了"},{"source_key":"1:group:50:14","quote":"泡泡梗笑死我了"}],
                 "reply_keys":[replies[1].clone()],"ttl_days":60},
                {"op":"add","kind":"avoid","text":"解释知识时别太长，先给简单版本","confidence":0.7,
                 "evidence":[{"source_key":"1:group:50:23","quote":"太长了"},{"source_key":"1:group:50:21","quote":"解释一下量子力学"}],
                 "reply_keys":[replies[2].clone()],"ttl_days":30},
                // One quote is not enough support.
                {"op":"add","kind":"style","text":"多用燕尾服梗","confidence":0.9,
                 "evidence":[{"source_key":"1:group:50:3","quote":"燕尾服"}],"reply_keys":[replies[0].clone()]},
                // A group norm cannot rest on one person.
                {"op":"add","kind":"norm","text":"本群都讨厌长回复","confidence":0.9,
                 "evidence":[{"source_key":"1:group:50:23","quote":"太长了"},{"source_key":"1:group:50:21","quote":"量子力学"}],"reply_keys":[replies[2].clone()]}
            ]}), false)?;
            assert_eq!((applied, rejected), (2, 2));
            let texts = recall_texts(db, &group, 9, CrossScenePolicy::PrivateFollowup)?;
            assert!(texts.iter().any(|t| t.contains("本群的梗") && t.contains("泡泡")));
            assert!(texts.iter().any(|t| t.contains("本群应避免")));
            // Another group sees nothing unless sharing is explicit.
            let other = scope(Scene::Group, 51);
            assert!(recall_texts(db, &other, 9, CrossScenePolicy::PrivateFollowup)?.is_empty());
            assert_eq!(recall_texts(db, &other, 9, CrossScenePolicy::Shared)?.len(), 2);
            // Revoking a quoted reaction deletes the note built on it.
            continuity::invalidate_sources(db, &["1:group:50:14".into()])?;
            let texts = recall_texts(db, &group, 9, CrossScenePolicy::PrivateFollowup)?;
            assert!(!texts.iter().any(|t| t.contains("泡泡")));
            Ok(())
        })
        .await
        .unwrap();
    host.store.stop().await.unwrap();
}

#[tokio::test]
async fn reviewed_notes_wait_for_an_administrator_and_mutations_are_idempotent() {
    let host = host_with(|c| c.plugins.ai.knowledge.behavior = true).await;
    host.store
        .call(|db| {
            let (group, replies) = seed_group(db)?;
            outcomes::evaluate(db, 1, 300)?;
            behavior::testing::commit_json(db, 1, json!({"changes":[
                {"op":"add","kind":"joke","text":"本群喜欢冷笑话","confidence":0.8,
                 "evidence":[{"source_key":"1:group:50:3","quote":"燕尾服笑死"},{"source_key":"1:group:50:4","quote":"冷笑话可以"}],
                 "reply_keys":[replies[0].clone()]}
            ]}), true)?;
            assert!(recall_texts(db, &group, 9, CrossScenePolicy::PrivateFollowup)?.is_empty());
            let item: (i64, i64) = db.query_row("SELECT id,version FROM ai_knowledge WHERE status='pending'", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let request = Mutation { bot_id: "1".into(), id: item.0, version: item.1, request_id: "r1".into(), action: Action::Approve };
            let first = mutate(db, 1, &request, "admin", "s")?;
            let replay = mutate(db, 1, &request, "admin", "s")?;
            assert_eq!(first, replay);
            assert_eq!(recall_texts(db, &group, 9, CrossScenePolicy::PrivateFollowup)?.len(), 1);
            // A stale version is refused.
            let stale = Mutation { request_id: "r2".into(), action: Action::Reject, ..request };
            assert!(mutate(db, 1, &stale, "admin", "s").is_err());
            let snapshot = snapshot(db, 1, &Filter::default())?;
            assert_eq!(snapshot["items"].as_array().unwrap().len(), 1);
            assert!(snapshot["outcomes"]["periods"].is_array());
            Ok(())
        })
        .await
        .unwrap();
    host.store.stop().await.unwrap();
}

#[tokio::test]
async fn impressions_follow_the_exact_fact_versions_and_their_audience() {
    let host = host_with(|c| c.plugins.ai.knowledge.impressions = true).await;
    let friend = scope(Scene::Friend, 7);
    let seed = friend.clone();
    let guard = host
        .store
        .call(move |db| {
            let friend = seed;
            let guard = conversation(db, &friend, Some(7))?;
            for (i, text) in [
                "我最近在备考教资",
                "我每天晚上跑步",
                "我喜欢猫",
                "考试压力好大",
                "周末想去爬山",
            ]
            .iter()
            .enumerate()
            {
                human(db, &friend, &guard, i as i64 + 1, 7, text, now(), true)?;
            }
            Ok(guard)
        })
        .await
        .unwrap();
    let batch = host.memory.prepare_batch(&guard, 5, true).await.unwrap();
    let changes: Vec<Value> = ["我最近在备考教资", "我每天晚上跑步", "我喜欢猫", "考试压力好大", "周末想去爬山"]
        .iter()
        .enumerate()
        .map(|(i, text)| json!({"op":"upsert","id":null,"subject_id":7,"kind":"event","text":text,"evidence":[{"source_key":format!("1:friend:7:{}",i+1),"quote":text}],"ttl_days":null}))
        .collect();
    let update: LearningUpdate =
        serde_json::from_value(json!({"summary":"","changes":changes})).unwrap();
    host.memory
        .commit_learning_batch(batch, update)
        .await
        .unwrap();
    host.store
        .call(move |db| {
            let ids: Vec<i64> = db.prepare("SELECT id FROM ai_memories ORDER BY id")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let stored = reflection::testing::refresh(db, 1, json!({"impressions":[
                {"text":"最近在备考，压力不小，可能喜欢轻松一点的鼓励","memory_ids":[ids[0],ids[3]]},
                {"text":"只凭一条记忆","memory_ids":[ids[1]]},
                {"text":"引用不存在的记忆","memory_ids":[ids[0], 9999]}
            ]}))?;
            assert_eq!(stored, 1);
            let texts = recall_texts(db, &friend, 7, CrossScenePolicy::PrivateFollowup)?;
            assert!(texts.iter().any(|t| t.contains("人物印象") && t.contains("备考")));
            // Private facts do not reach a group, even for the same person.
            assert!(recall_texts(db, &scope(Scene::Group, 50), 7, CrossScenePolicy::PrivateFollowup)?.is_empty());
            // Editing a fact behind it retires the impression.
            db.execute("UPDATE ai_memories SET version=version+1 WHERE id=?1", [ids[3]])?;
            assert!(!recall_texts(db, &friend, 7, CrossScenePolicy::PrivateFollowup)?.iter().any(|t| t.contains("备考")));
            // Forgetting removes it entirely.
            let memories: Vec<memory::Memory> = vec![memory::get_memory(db, ids[0])?.unwrap()];
            memory::revoke_memories(db, &memories)?;
            assert_eq!(db.query_row("SELECT count(*) FROM ai_knowledge", [], |r| r.get::<_, i64>(0))?, 0);
            Ok(())
        })
        .await
        .unwrap();
    host.store.stop().await.unwrap();
}

#[test]
fn bonus_reranking_keeps_order_without_statistics() {
    let db = Connection::open_in_memory().unwrap();
    let items = vec![3, 1, 2];
    assert_eq!(stats::rerank(&db, items.clone(), |i| *i).unwrap(), items);
}
