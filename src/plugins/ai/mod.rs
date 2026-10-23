pub(crate) mod auto_reply;
pub mod chat;
pub mod learning;
pub mod memory;
pub mod memory_rebuild;
pub(crate) mod reply;
pub mod semantic;
mod sticker_reply;
pub mod stickers;
pub(crate) mod trigger;
pub(crate) mod vision;

use crate::{
    config::AiSettings,
    host::Host,
    types::{ContextGuard, Incoming, Scene, Scope},
};
use anyhow::{Result, ensure};
use rusqlite::params;

#[derive(Clone, Debug)]
pub struct Conversation {
    pub guard: ContextGuard,
    pub settings: AiSettings,
    pub scope: Scope,
    pub owner_id: Option<i64>,
}

pub async fn conversation(host: &Host, msg: &Incoming) -> Result<Conversation> {
    let scope = msg.key.scope.clone();
    let actor = msg.sender_id;
    let defaults = host.config.plugins.ai.defaults.clone();
    host.store.call(move|c| {
        crate::access::require_enabled(c,&scope.key())?;
        let (mode,revision)=scope_mode(c,&scope,&defaults.mode)?;
        let owner=if scope.scene==Scene::Friend||mode=="user"{Some(actor)}else{None};
        let id=match owner {Some(user)=>format!("{}:v{revision}:user:{user}",scope.key()),None=>format!("{}:v{revision}:group",scope.key())};
        c.execute("INSERT OR IGNORE INTO ai_conversations(id,scope,mode,owner_id,settings) VALUES(?1,?2,?3,?4,COALESCE((SELECT json_object('persona',persona) FROM ai_group_personas WHERE scope=?2),'{}'))",params![id,scope.key(),mode,owner])?;
        let (epoch,settings):(i64,String)=c.query_row("SELECT epoch,settings FROM ai_conversations WHERE id=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        // Stored overrides can narrow, never widen, the configured tool allow list.
        let mut settings=resolve_settings(&defaults,&settings)?;settings.mode=mode;
        settings.allowed_tools.retain(|name|defaults.allowed_tools.contains(name));
        Ok(Conversation{guard:ContextGuard{conversation_id:id,epoch},settings,scope,owner_id:owner})
    }).await
}

/// The configured mode seeds a new group. Subsequent mode changes use the
/// Web administration, which increments the revision instead of reviving old IDs.
fn scope_mode(db: &rusqlite::Connection, scope: &Scope, default: &str) -> Result<(String, i64)> {
    if scope.scene == Scene::Friend {
        return Ok(("user".into(), 0));
    }
    db.execute(
        "INSERT OR IGNORE INTO ai_scope_modes(scope,mode,revision) VALUES(?1,?2,0)",
        params![scope.key(), default],
    )?;
    Ok(db.query_row(
        "SELECT mode,revision FROM ai_scope_modes WHERE scope=?1",
        [scope.key()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

pub async fn change_settings(
    host: &Host,
    conversation: &Conversation,
    new: AiSettings,
) -> Result<ContextGuard> {
    host.config.plugins.ai.validate_settings(&new)?;
    let g = conversation.guard.clone();
    let defaults = serde_json::to_value(&host.config.plugins.ai.defaults)?;
    let mut values = serde_json::to_value(&new)?;
    values
        .as_object_mut()
        .unwrap()
        .retain(|k, v| k != "mode" && (k == "persona" || defaults.get(k) != Some(v)));
    let scope = conversation.scope.key();
    host.store
        .call(move |c| {
            let tx = c.transaction()?;
            let seeded: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM ai_group_personas WHERE scope=?1)",
                [&scope],
                |row| row.get(0),
            )?;
            if !seeded && values["persona"] == defaults["persona"] {
                values.as_object_mut().unwrap().remove("persona");
            }
            let json = serde_json::to_string(&values)?;
            let n = tx.execute(
                "UPDATE ai_conversations SET settings=?3,epoch=epoch+1 WHERE id=?1 AND epoch=?2",
                params![g.conversation_id, g.epoch, json],
            )?;
            ensure!(n == 1, "会话设置已变化，请重试");
            // Carry forward current context for ordinary setting changes; old tasks still expire.
            tx.execute(
                "UPDATE messages SET epoch=?3 WHERE conversation_id=?1 AND epoch=?2",
                params![g.conversation_id, g.epoch, g.epoch + 1],
            )?;
            tx.commit()?;
            Ok(ContextGuard {
                conversation_id: g.conversation_id,
                epoch: g.epoch + 1,
            })
        })
        .await
}

pub async fn reset(host: &Host, conversation: &Conversation) -> Result<ContextGuard> {
    let g = conversation.guard.clone();
    host.store.call(move|c| {
        let n=c.execute("UPDATE ai_conversations SET epoch=epoch+1,summary='',compacted_until=0,memory_processed_until=0 WHERE id=?1 AND epoch=?2",params![g.conversation_id,g.epoch])?;
        ensure!(n==1,"会话已变化");Ok(ContextGuard{conversation_id:g.conversation_id,epoch:g.epoch+1})
    }).await
}

pub async fn switch_mode(host: &Host, scope: &Scope, mode: &str) -> Result<()> {
    ensure!(
        mode == "user" || mode == "group",
        "模式只能是 group 或 user"
    );
    let scope = scope.key();
    let mode = mode.to_owned();
    host.store.call(move|c| {
        let tx=c.transaction()?;
        tx.execute("INSERT INTO ai_scope_modes(scope,mode,revision) VALUES(?1,?2,1) ON CONFLICT(scope) DO UPDATE SET mode=excluded.mode,revision=revision+1",params![scope,mode])?;
        tx.execute("UPDATE ai_conversations SET epoch=epoch+1,summary='',compacted_until=0,memory_processed_until=0 WHERE scope=?1",[scope])?;
        tx.commit()?;Ok(())
    }).await
}

pub fn resolve_settings(defaults: &AiSettings, overrides: &str) -> Result<AiSettings> {
    let mut value = serde_json::to_value(defaults)?;
    let overrides: serde_json::Value = serde_json::from_str(overrides)?;
    ensure!(overrides.is_object(), "会话设置格式无效");
    for (k, v) in overrides.as_object().unwrap() {
        value[k] = v.clone();
    }
    let mut settings: AiSettings = serde_json::from_value(value)?;
    settings
        .allowed_tools
        .retain(|n| defaults.allowed_tools.contains(n));
    crate::config::validate_settings(&settings)?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changing_config_default_does_not_revive_an_old_scope_mode() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE ai_scope_modes(scope TEXT PRIMARY KEY,mode TEXT NOT NULL,revision INTEGER NOT NULL);").unwrap();
        let scope = Scope {
            bot_id: 1,
            scene: Scene::Group,
            peer_id: 2,
        };
        assert_eq!(
            scope_mode(&db, &scope, "group").unwrap(),
            ("group".into(), 0)
        );
        assert_eq!(
            scope_mode(&db, &scope, "user").unwrap(),
            ("group".into(), 0)
        );
        db.execute(
            "UPDATE ai_scope_modes SET mode='user',revision=1 WHERE scope=?1",
            [scope.key()],
        )
        .unwrap();
        assert_eq!(
            scope_mode(&db, &scope, "group").unwrap(),
            ("user".into(), 1)
        );
    }

    fn persona_message(bot_id: i64, scene: Scene, peer_id: i64, sender_id: i64) -> Incoming {
        Incoming {
            key: crate::types::MessageKey {
                scope: Scope {
                    bot_id,
                    scene,
                    peer_id,
                },
                message_seq: 1,
            },
            sender_id,
            sender_nickname: None,
            sender_card: None,
            text: "你好".into(),
            mentions: Vec::new(),
            quote_seq: None,
            received_at: crate::types::now(),
            raw_message: serde_json::Value::Null,
            notification: None,
        }
    }

    async fn persona_host(database: &str) -> std::sync::Arc<Host> {
        let mut config = crate::config::Config::default();
        config.app.database = database.into();
        config.plugins.ai.enabled = false;
        config.plugins.native.enabled.clear();
        Host::create(config).await.unwrap()
    }

    #[tokio::test]
    async fn group_persona_seeds_shared_and_new_member_contexts_without_crossing_accounts() {
        let host = persona_host(":memory:").await;
        let friend = persona_message(123, Scene::Friend, 456, 456);
        let foreign = persona_message(124, Scene::Group, 456, 7);
        let friend_before = conversation(&host, &friend).await.unwrap();
        let foreign_before = conversation(&host, &foreign).await.unwrap();
        host.store
            .call(|db| {
                db.execute_batch(
                    "INSERT INTO ai_group_personas(scope,persona) VALUES
                    ('123:group:456','理性搭档'),('123:group:789','温柔学长');
                 INSERT INTO ai_scope_modes(scope,mode,revision) VALUES('123:group:789','user',0);",
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let shared = persona_message(123, Scene::Group, 456, 7);
        let first = conversation(&host, &shared).await.unwrap();
        let second = conversation(&host, &persona_message(123, Scene::Group, 456, 8))
            .await
            .unwrap();
        assert_eq!(first.settings.persona, "理性搭档");
        assert_eq!(first.owner_id, None);
        assert_eq!(first.guard.conversation_id, second.guard.conversation_id);

        let member_a = conversation(&host, &persona_message(123, Scene::Group, 789, 7))
            .await
            .unwrap();
        let member_b = conversation(&host, &persona_message(123, Scene::Group, 789, 8))
            .await
            .unwrap();
        assert_eq!(member_a.settings.persona, "温柔学长");
        assert_eq!(member_b.settings.persona, "温柔学长");
        assert_eq!(member_a.owner_id, Some(7));
        assert_eq!(member_b.owner_id, Some(8));
        assert_ne!(
            member_a.guard.conversation_id,
            member_b.guard.conversation_id
        );

        let friend_after = conversation(&host, &friend).await.unwrap();
        let foreign_after = conversation(&host, &foreign).await.unwrap();
        assert_eq!(
            friend_after.settings.persona,
            friend_before.settings.persona
        );
        assert_eq!(
            friend_after.guard.conversation_id,
            friend_before.guard.conversation_id
        );
        assert_eq!(friend_after.guard.epoch, friend_before.guard.epoch);
        assert_eq!(
            foreign_after.settings.persona,
            foreign_before.settings.persona
        );
        assert_eq!(
            foreign_after.guard.conversation_id,
            foreign_before.guard.conversation_id
        );
        assert_eq!(foreign_after.guard.epoch, foreign_before.guard.epoch);
        for unrelated in [
            persona_message(123, Scene::Group, 999, 7),
            persona_message(124, Scene::Group, 789, 8),
            persona_message(123, Scene::Friend, 789, 789),
        ] {
            let created = conversation(&host, &unrelated).await.unwrap();
            assert_eq!(
                created.settings.persona,
                host.config.plugins.ai.defaults.persona
            );
        }
        host.cancel.cancel();
        host.store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn group_persona_seed_survives_restart_and_new_modes_but_never_overwrites_existing_choice()
     {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("group-personas.sqlite");
        let host = persona_host(database.to_str().unwrap()).await;
        let chosen = host.config.plugins.ai.defaults.persona.clone();
        let seeded = chosen.clone();
        host.store
            .call(move |db| {
                db.execute(
                    "INSERT INTO ai_group_personas(scope,persona) VALUES('123:group:456',?1)",
                    [seeded],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let message = persona_message(123, Scene::Group, 456, 7);
        let initial = conversation(&host, &message).await.unwrap();
        assert_eq!(initial.settings.persona, chosen);

        let mut settings = initial.settings.clone();
        settings.recent_messages = 12;
        change_settings(&host, &initial, settings).await.unwrap();
        let id = initial.guard.conversation_id.clone();
        let explicit: String = host
            .store
            .call(move |db| {
                Ok(db.query_row(
                    "SELECT json_extract(settings,'$.persona') FROM ai_conversations WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            explicit, chosen,
            "saving another field must retain a seeded global-equal choice"
        );

        let current = conversation(&host, &message).await.unwrap();
        let mut custom = current.settings.clone();
        custom.persona = "理性搭档".into();
        change_settings(&host, &current, custom).await.unwrap();
        // Use a seed different from both the global default and the existing
        // conversation override, so a missing seed read cannot pass by accident.
        let future_persona = "沉稳前辈";
        host.store
            .call(|db| {
                db.execute(
                    "UPDATE ai_group_personas SET persona='沉稳前辈' WHERE scope='123:group:456'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        for _ in 0..2 {
            let loaded = conversation(&host, &message).await.unwrap();
            assert_eq!(loaded.settings.persona, "理性搭档");
            assert_eq!(loaded.settings.recent_messages, 12);
            assert_eq!(loaded.guard.conversation_id, initial.guard.conversation_id);
        }

        switch_mode(&host, &message.key.scope, "user")
            .await
            .unwrap();
        let isolated = conversation(&host, &message).await.unwrap();
        assert_eq!(isolated.settings.persona, future_persona);
        assert_eq!(isolated.owner_id, Some(7));
        assert_ne!(
            isolated.guard.conversation_id,
            initial.guard.conversation_id
        );
        switch_mode(&host, &message.key.scope, "group")
            .await
            .unwrap();
        let new_shared = conversation(&host, &message).await.unwrap();
        assert_eq!(new_shared.settings.persona, future_persona);
        assert_ne!(
            new_shared.guard.conversation_id,
            initial.guard.conversation_id
        );
        assert_ne!(
            new_shared.guard.conversation_id,
            isolated.guard.conversation_id
        );
        host.cancel.cancel();
        host.store.stop().await.unwrap();
        drop(host);

        let restarted = persona_host(database.to_str().unwrap()).await;
        switch_mode(&restarted, &message.key.scope, "user")
            .await
            .unwrap();
        let new_member = conversation(&restarted, &persona_message(123, Scene::Group, 456, 99))
            .await
            .unwrap();
        assert_eq!(new_member.settings.persona, future_persona);
        assert_eq!(new_member.owner_id, Some(99));
        assert_ne!(
            new_member.guard.conversation_id,
            isolated.guard.conversation_id
        );
        let original_id = initial.guard.conversation_id;
        let original: String = restarted
            .store
            .call(move |db| {
                Ok(db.query_row(
                    "SELECT json_extract(settings,'$.persona') FROM ai_conversations WHERE id=?1",
                    [original_id],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            original, "理性搭档",
            "new generations must not rewrite historical selections"
        );
        restarted.cancel.cancel();
        restarted.store.stop().await.unwrap();
    }
}
