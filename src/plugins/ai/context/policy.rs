//! Audience checks happen before material reaches the model. Subject identity
//! alone never grants access to another person's private conversation.
use crate::{
    config::CrossScenePolicy,
    types::{Scene, Scope},
};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

/// Retire old summary baselines when adopting a restricted policy: older
/// consolidators could see facts whose audience was not recorded on summaries.
/// Keep original human messages, traceable replies and authoritative knowledge.
pub async fn configure(store: &crate::store::Store, policy: CrossScenePolicy) -> Result<()> {
    store.call(move |db| {
        let tx=db.transaction()?;
        let encoded=serde_json::to_string(&policy)?;
        let previous:Option<String>=tx.query_row("SELECT policy FROM ai_context_audience WHERE id=1",[],|r|r.get(0)).optional()?;
        if previous.as_ref()!=Some(&encoded) && policy!=CrossScenePolicy::Shared {
            tx.execute("INSERT OR IGNORE INTO ai_context_invalidated(source_key,invalidated_at)
                SELECT m.key,?1 FROM messages m WHERE m.role='assistant'
                AND NOT EXISTS(SELECT 1 FROM ai_context_reply_dependencies d WHERE d.reply_key=m.key)",[crate::types::now()])?;
            tx.execute("UPDATE ai_conversations SET summary='',compacted_until=0,memory_revision=memory_revision+1 WHERE summary!=''",[])?;
            tx.execute("DELETE FROM ai_context_summary_dependencies",[])?;
        }
        tx.execute("INSERT INTO ai_context_audience(id,policy) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET policy=excluded.policy",[encoded])?;
        tx.commit()?;
        Ok(())
    }).await
}

pub(crate) fn configured(db: &Connection) -> Result<Option<CrossScenePolicy>> {
    let installed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='ai_context_audience')",
        [],
        |r| r.get(0),
    )?;
    if !installed {
        return Ok(None);
    }
    let value: Option<String> = db
        .query_row(
            "SELECT policy FROM ai_context_audience WHERE id=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
}

pub fn allows_scope(policy: CrossScenePolicy, source: &Scope, target: &Scope, actor: i64) -> bool {
    if source.bot_id != target.bot_id || actor <= 0 || actor == target.bot_id {
        return false;
    }
    if source.scene == Scene::Friend && source.peer_id != actor {
        return false;
    }
    if target.scene == Scene::Friend && target.peer_id != actor {
        return false;
    }
    source == target
        || match policy {
            CrossScenePolicy::SameScope => false,
            CrossScenePolicy::PrivateFollowup => target.scene == Scene::Friend,
            CrossScenePolicy::Shared => true,
        }
}

pub fn allows_scope_key(
    policy: CrossScenePolicy,
    source: &str,
    target: &Scope,
    actor: i64,
) -> bool {
    super::continuity::parse_scope(source)
        .is_ok_and(|scope| allows_scope(policy, &scope, target, actor))
}

pub fn allows_sources(
    db: &Connection,
    policy: CrossScenePolicy,
    keys: &[String],
    target: &Scope,
    actor: i64,
) -> Result<bool> {
    for key in keys {
        // Source keys survive normal raw-message expiry. Opaque legacy evidence
        // has no inferred global visibility; an available original row is needed.
        let scope = key
            .rsplit_once(':')
            .and_then(|(scope, _)| super::continuity::parse_scope(scope).ok());
        let scope = match scope {
            Some(scope) => Some(scope),
            None => db
                .query_row("SELECT scope FROM messages WHERE key=?1", [key], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?
                .and_then(|s| super::continuity::parse_scope(&s).ok()),
        };
        if !scope.is_some_and(|s| allows_scope(policy, &s, target, actor)) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audience_matrix_never_grants_another_persons_private_history() {
        let group = Scope {
            bot_id: 1,
            scene: Scene::Group,
            peer_id: 10,
        };
        let other = Scope {
            peer_id: 20,
            ..group.clone()
        };
        let private = Scope {
            scene: Scene::Friend,
            peer_id: 7,
            ..group.clone()
        };
        let foreign = Scope {
            peer_id: 8,
            ..private.clone()
        };
        for policy in [
            CrossScenePolicy::SameScope,
            CrossScenePolicy::PrivateFollowup,
            CrossScenePolicy::Shared,
        ] {
            assert!(allows_scope(policy, &group, &group, 7));
            assert!(allows_scope(policy, &private, &private, 7));
            assert!(!allows_scope(policy, &foreign, &private, 7));
            assert!(!allows_scope(policy, &foreign, &group, 7));
            assert!(!allows_scope(policy, &group, &foreign, 7));
            assert!(!allows_scope(
                policy,
                &Scope {
                    bot_id: 2,
                    ..group.clone()
                },
                &group,
                7
            ));
            assert_eq!(
                allows_scope(policy, &private, &group, 7),
                policy == CrossScenePolicy::Shared
            );
            assert_eq!(
                allows_scope(policy, &group, &other, 7),
                policy == CrossScenePolicy::Shared
            );
            assert_eq!(
                allows_scope(policy, &group, &private, 7),
                policy != CrossScenePolicy::SameScope
            );
        }
    }

    #[tokio::test]
    async fn restricted_policy_retires_untraceable_legacy_caches_only_once() {
        let store = crate::store::Store::open(":memory:", 32).await.unwrap();
        crate::plugins::ai::memory::MemoryStore::new(store.clone(), 256)
            .init()
            .await
            .unwrap();
        store.call(|db| {
            db.execute("INSERT INTO ai_conversations(id,scope,mode,summary,compacted_until,memory_processed_until) VALUES('legacy','1:group:10','group','旧摘要',5,5)",[])?;
            db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position) VALUES('1:group:10:1','1:group:10','legacy',1,7,'user','原始证据',1,1),('1:group:10:2','1:group:10','legacy',1,1,'assistant','旧回答',1,1)",[])?;
            Ok(())
        }).await.unwrap();
        configure(&store, CrossScenePolicy::PrivateFollowup)
            .await
            .unwrap();
        configure(&store, CrossScenePolicy::PrivateFollowup)
            .await
            .unwrap();
        store.call(|db| {
            assert_eq!(db.query_row("SELECT summary,compacted_until,memory_processed_until,memory_revision FROM ai_conversations WHERE id='legacy'",[],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?)))?,(String::new(),0,5,1));
            assert!(super::super::reply_is_valid(db,"1:group:10:1")?);
            assert!(!super::super::reply_is_valid(db,"1:group:10:2")?);
            assert_eq!(db.query_row("SELECT count(*) FROM messages WHERE content!=''",[],|r|r.get::<_,i64>(0))?,2);
            Ok(())
        }).await.unwrap();
        store.stop().await.unwrap();
    }
}
