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
/// explicit command, which increments the revision instead of reviving old IDs.
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
    crate::config::validate_settings(&new)?;
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

pub async fn command(
    host: &Host,
    chat: &chat::Chat,
    msg: &Incoming,
    conv: &Conversation,
) -> Result<String> {
    use memory::MemoryKind;
    let args = msg.text.trim().strip_prefix("/ai").unwrap_or("").trim();
    let parts: Vec<&str> = args.split_whitespace().collect();
    match parts.as_slice() {
        ["status"] => {
            let usage=host.store.call(|c|Ok(c.query_row("SELECT COALESCE(SUM(requests),0),COALESCE(SUM(input_tokens),0),COALESCE(SUM(output_tokens),0) FROM usage_daily WHERE day=date('now')",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?)))?)).await?;
            return Ok(format!(
                "模式：{}；人设：{}；暂停：{}\n输入预算：{}（估算）；输出上限：{}；历史：{}\n记忆读/写：{}/{}；摘要：{}\n队列：{}；今日全局请求：{}，输入/输出 tokens：{}/{}",
                conv.settings.mode,
                conv.settings.persona,
                conv.settings.paused,
                conv.settings.input_budget,
                conv.settings.output_budget,
                conv.settings.recent_messages,
                conv.settings.memory_read,
                conv.settings.memory_write,
                conv.settings.summary,
                chat.pending(),
                usage.0,
                usage.1,
                usage.2
            ));
        }
        ["tools"] => {
            return Ok(format!(
                "本会话允许工具：{}",
                conv.settings.allowed_tools.join(", ")
            ));
        }
        ["persona", "list"] => {
            return Ok(host
                .config
                .plugins
                .ai
                .personas
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join("、"));
        }
        ["memory", "list"] | ["memory", "list", _] => {
            let page = parts
                .get(2)
                .map(|value| value.parse::<usize>())
                .transpose()?
                .unwrap_or(1);
            let listing = host
                .memory
                .list_page(&conv.guard, msg.sender_id, page)
                .await?;
            if listing.total == 0 {
                return Ok("你在当前会话没有长期记忆。".into());
            }
            let entries = listing
                .memories
                .iter()
                .map(|m| format!("{} · {:?}：{}", m.id, m.kind, m.text))
                .collect::<Vec<_>>()
                .join("\n");
            let navigation = if listing.page < listing.total_pages {
                format!("\n下一页：/ai memory list {}", listing.page + 1)
            } else {
                String::new()
            };
            return Ok(format!(
                "第 {}/{} 页，共 {} 条（每页20条）\n{entries}{navigation}",
                listing.page, listing.total_pages, listing.total
            ));
        }
        ["memory", "forget", id] => {
            host.memory
                .forget(&conv.guard, msg.sender_id, id.parse()?)
                .await?;
            chat.cancel(&conv.guard);
            return Ok("已遗忘该记忆，并清空相关摘要和活跃上下文；旧来源不会重新学习。".into());
        }
        ["memory", "remember", ..] => {
            let text = args
                .strip_prefix("memory")
                .unwrap()
                .trim_start()
                .strip_prefix("remember")
                .unwrap()
                .trim();
            let id = host
                .memory
                .remember(
                    &conv.guard,
                    msg.sender_id,
                    &msg.key.key(),
                    text,
                    MemoryKind::Fact,
                    None,
                )
                .await?;
            return Ok(format!("已记住，ID：{id}。"));
        }
        _ => {}
    }
    let manageable =
        conv.owner_id == Some(msg.sender_id) || host.config.access.admin(msg.sender_id);
    ensure!(manageable, "群共享会话的设置需要配置中的管理员操作");
    let mut settings = conv.settings.clone();
    match parts.as_slice() {
        ["new"]=>{reset(host,conv).await?;chat.cancel(&conv.guard);return Ok("已开始新对话，长期记忆保留。".into());}
        ["mode",mode]=>{
            ensure!(conv.scope.scene==Scene::Group&&host.config.access.admin(msg.sender_id),"群模式切换需要管理员");
            ensure!(*mode=="group"||*mode=="user","模式只能是 group 或 user");
            chat.cancel_scope(&conv.scope);switch_mode(host,&conv.scope,mode).await?;return Ok(format!("群模式已切换为 {mode}，上下文重新开始。"));
        }
        ["pause"]=>{settings.paused=true;settings.once=false;}
        ["resume"]=>{settings.paused=false;settings.once=false;}
        ["once"]=>{settings.paused=true;settings.once=true;}
        ["context",value]=>settings.input_budget=value.parse()?,
        ["output",value]=>settings.output_budget=value.parse()?,
        ["history",value]=>settings.recent_messages=value.parse()?,
        ["persona",name]=>{ensure!(host.config.plugins.ai.personas.contains_key(*name),"人设不存在，请使用 /ai persona list");settings.persona=(*name).into();}
        ["memory","read",value]=>settings.memory_read=on_off(value)?,
        ["memory","write",value]=>settings.memory_write=on_off(value)?,
        ["summary",value]=>settings.summary=on_off(value)?,
        _=>return Ok("/ai status | new | mode group|user | pause | resume | once\n/ai context N | output N | history N | persona list|名称\n/ai memory list [页码] | remember 内容 | forget ID | read on|off | write on|off\n/ai summary on|off | tools".into()),
    }
    change_settings(host, conv, settings).await?;
    chat.cancel(&conv.guard);
    Ok("设置已更新，旧任务已取消。".into())
}
fn on_off(value: &str) -> Result<bool> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => anyhow::bail!("参数必须是 on 或 off"),
    }
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
