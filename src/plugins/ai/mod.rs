pub mod chat;
pub mod memory;

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
        c.execute("INSERT OR IGNORE INTO ai_conversations(id,scope,mode,owner_id,settings) VALUES(?1,?2,?3,?4,'{}')",params![id,scope.key(),mode,owner])?;
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
        .retain(|k, v| k != "mode" && defaults.get(k) != Some(v));
    let json = serde_json::to_string(&values)?;
    host.store
        .call(move |c| {
            let tx = c.transaction()?;
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
}
