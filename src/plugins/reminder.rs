use crate::{
    host::Host,
    plugin::{Plugin, PluginSpec},
    store::Store,
    tools::{Effect, Tool, ToolRegistry, ToolResult, ToolSpec, object},
    types::{Incoming, Scope, now},
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use chrono::TimeZone;
use chrono_tz::Tz;
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct ReminderPlugin;
#[async_trait]
impl Plugin for ReminderPlugin {
    fn spec(&self) -> PluginSpec {
        PluginSpec {
            id: "reminder",
            commands: &["/remind"],
            tools: &["reminder_list", "reminder_create", "reminder_cancel"],
            dependencies: &["host"],
        }
    }
    async fn command(&self, host: &Host, message: &Incoming) -> Result<String> {
        let parts: Vec<_> = message.text.split_whitespace().collect();
        let (name, args) = match parts.as_slice() {
            ["/remind", "list"] => ("reminder_list", json!({})),
            ["/remind", "cancel", id] => ("reminder_cancel", json!({"reminder_id":id})),
            ["/remind", "add", seconds, rest @ ..] => {
                ensure!(!rest.is_empty(), "用法：/remind add 秒数 内容");
                (
                    "reminder_create",
                    json!({"after_seconds":seconds.parse::<i64>()?,"text":rest.join(" ")}),
                )
            }
            _ => return Ok(
                "/remind add 秒数 内容 | list | cancel ID（仅当前用户、当前会话；群内提醒公开）"
                    .into(),
            ),
        };
        match host.gateway.direct_message(message, name, args).await? {
            crate::tools::ToolOutcome::Completed { receipt, .. } => Ok(receipt),
            crate::tools::ToolOutcome::Pending { preview, .. } => Ok(preview),
        }
    }
}

pub fn register(registry: &mut ToolRegistry, max_active: usize) -> Result<()> {
    registry.register(Arc::new(ReminderList))?;
    registry.register(Arc::new(ReminderCreate {
        max_active: max_active.clamp(1, 10),
    }))?;
    registry.register(Arc::new(ReminderCancel))
}

pub async fn migrate(store: &Store) -> Result<()> {
    store
        .migrate(
            "reminder",
            1,
            "
CREATE TABLE IF NOT EXISTS reminder_jobs (
 id TEXT PRIMARY KEY, operation_id TEXT NOT NULL UNIQUE, scope_key TEXT NOT NULL,
 scope_json TEXT NOT NULL, actor_id INTEGER NOT NULL, text TEXT NOT NULL,
 due_at INTEGER NOT NULL, status TEXT NOT NULL, created_at INTEGER NOT NULL,
 completed_at INTEGER, send_error TEXT
);
CREATE INDEX IF NOT EXISTS reminder_due ON reminder_jobs(status,due_at);
CREATE INDEX IF NOT EXISTS reminder_owner ON reminder_jobs(scope_key,actor_id,status);
",
        )
        .await
}

fn spec(
    name: &'static str,
    description: &'static str,
    parameters: Value,
    effect: Effect,
) -> ToolSpec {
    ToolSpec {
        name,
        plugin: "reminder",
        version: 1,
        description,
        parameters,
        effect,
        // Ten legal 200-character reminders can exceed 8 KiB with four-byte
        // Unicode. Preserve their complete contents in both JSON and receipts.
        max_output_bytes: 16384,
    }
}
struct ReminderCreate {
    max_active: usize,
}
impl Tool for ReminderCreate {
    fn spec(&self) -> ToolSpec {
        spec(
            "reminder_create",
            "Create a reminder for the requesting user in this chat. Requires that user's confirmation.",
            json!({"type":"object","properties":{"after_seconds":{"type":"integer","minimum":60,"maximum":2592000},"text":{"type":"string","minLength":1,"maxLength":200}},"required":["after_seconds","text"],"additionalProperties":false}),
            Effect::LocalWrite,
        )
    }
    fn prepare(&self, args: Value, _: &Scope, _: i64, received_at: i64, _: Tz) -> Result<Value> {
        object(&args, &["after_seconds", "text"])?;
        let seconds = args["after_seconds"]
            .as_i64()
            .context("after_seconds must be an integer")?;
        ensure!(
            (60..=2_592_000).contains(&seconds),
            "after_seconds must be between 60 and 2592000"
        );
        let text = args["text"]
            .as_str()
            .context("text must be a string")?
            .trim();
        ensure!(
            !text.is_empty() && text.chars().count() <= 200,
            "reminder text must contain 1 to 200 characters"
        );
        let due = received_at.checked_add(seconds).context("time overflow")?;
        Ok(json!({"due_at":due,"text":text}))
    }
    fn preview(&self, args: &Value, timezone: Tz) -> Result<String> {
        let time = timezone
            .timestamp_opt(args["due_at"].as_i64().context("missing time")?, 0)
            .single()
            .context("invalid time")?;
        Ok(format!(
            "在 {} ({}) 于本会话提醒你：{}",
            time.format("%Y-%m-%d %H:%M:%S %:z"),
            timezone,
            args["text"].as_str().unwrap_or_default()
        ))
    }
    fn apply(
        &self,
        tx: &Transaction<'_>,
        args: &Value,
        scope: &Scope,
        actor: i64,
        operation: &str,
        time: i64,
    ) -> Result<ToolResult> {
        let count:i64=tx.query_row("SELECT count(*) FROM reminder_jobs WHERE scope_key=?1 AND actor_id=?2 AND status IN ('pending','dispatching')",params![scope.key(),actor],|r|r.get(0))?;
        ensure!(
            count < self.max_active as i64,
            "每人在当前会话最多有 {} 条未完成提醒",
            self.max_active
        );
        let due = args["due_at"].as_i64().context("missing due time")?;
        ensure!(due > time, "预约时间已过，请重新创建提醒");
        let text = args["text"].as_str().context("missing text")?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        tx.execute("INSERT INTO reminder_jobs(id,operation_id,scope_key,scope_json,actor_id,text,due_at,status,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,'pending',?8)",params![id,operation,scope.key(),serde_json::to_string(scope)?,actor,text,due,time])?;
        Ok(ToolResult {
            value: json!({"id":id,"due_at":due,"text":text,"status":"pending"}),
            receipt: format!("已创建提醒 {id}。"),
        })
    }
}
struct ReminderList;
impl Tool for ReminderList {
    fn spec(&self) -> ToolSpec {
        spec(
            "reminder_list",
            "List only the requesting user's pending or uncertain-delivery reminders in this same chat; at most 10, active first. Unknown deliveries must not be retried automatically.",
            json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
            Effect::Read,
        )
    }
    fn prepare(&self, args: Value, _: &Scope, _: i64, _: i64, _: Tz) -> Result<Value> {
        object(&args, &[])?;
        Ok(args)
    }
    fn preview(&self, _: &Value, _: Tz) -> Result<String> {
        Ok("查询你在本会话的提醒".into())
    }
    fn apply(
        &self,
        tx: &Transaction<'_>,
        _: &Value,
        scope: &Scope,
        actor: i64,
        _: &str,
        _: i64,
    ) -> Result<ToolResult> {
        let mut stmt=tx.prepare("SELECT id,due_at,text,status FROM reminder_jobs WHERE scope_key=?1 AND actor_id=?2 AND status IN ('pending','dispatching','unknown') ORDER BY CASE WHEN status='unknown' THEN 1 ELSE 0 END,due_at LIMIT 10")?;
        let items=stmt.query_map(params![scope.key(),actor],|r|Ok(json!({"id":r.get::<_,String>(0)?,"due_at":r.get::<_,i64>(1)?,"text":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
        let receipt = if items.is_empty() {
            "当前会话没有待处理提醒。".into()
        } else {
            items
                .iter()
                .map(|v| {
                    format!(
                        "{}：{} (Unix {}；{})",
                        v["id"].as_str().unwrap_or_default(),
                        v["text"].as_str().unwrap_or_default(),
                        v["due_at"],
                        match v["status"].as_str() {
                            Some("unknown") => "投递结果不确定，未自动重试",
                            Some("dispatching") => "发送中",
                            _ => "等待提醒",
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(ToolResult {
            value: json!({"reminders":items}),
            receipt,
        })
    }
}
struct ReminderCancel;
impl Tool for ReminderCancel {
    fn spec(&self) -> ToolSpec {
        spec(
            "reminder_cancel",
            "Cancel one of the requesting user's pending reminders in this same chat. Requires confirmation.",
            json!({"type":"object","properties":{"reminder_id":{"type":"string","minLength":1,"maxLength":64}},"required":["reminder_id"],"additionalProperties":false}),
            Effect::LocalWrite,
        )
    }
    fn prepare(&self, args: Value, _: &Scope, _: i64, _: i64, _: Tz) -> Result<Value> {
        object(&args, &["reminder_id"])?;
        let id = args["reminder_id"]
            .as_str()
            .context("reminder_id must be a string")?;
        ensure!(!id.is_empty() && id.len() <= 64, "invalid reminder_id");
        Ok(args)
    }
    fn preview(&self, args: &Value, _: Tz) -> Result<String> {
        Ok(format!(
            "取消你在本会话的提醒 {}",
            args["reminder_id"].as_str().unwrap_or_default()
        ))
    }
    fn apply(
        &self,
        tx: &Transaction<'_>,
        args: &Value,
        scope: &Scope,
        actor: i64,
        _: &str,
        time: i64,
    ) -> Result<ToolResult> {
        let id = args["reminder_id"]
            .as_str()
            .context("missing reminder_id")?;
        let changed=tx.execute("UPDATE reminder_jobs SET status='cancelled',completed_at=?4 WHERE id=?1 AND scope_key=?2 AND actor_id=?3 AND status='pending'",params![id,scope.key(),actor,time])?;
        ensure!(changed == 1, "提醒不存在、不属于你，或已开始发送");
        Ok(ToolResult {
            value: json!({"id":id,"status":"cancelled"}),
            receipt: format!("已取消提醒 {id}。"),
        })
    }
}

#[derive(Debug)]
pub struct ReminderDue {
    pub id: String,
    pub scope: Scope,
    pub actor_id: i64,
    pub text: String,
    pub due_at: i64,
    pub plugin_epoch: i64,
}
#[derive(Debug, Clone, Copy)]
pub enum Delivery {
    Sent,
    KnownFailure,
    Unknown,
}

/// Claimed jobs cannot be sent twice. A crash after claiming becomes unknown,
/// because an external send may have succeeded even if its receipt was lost.
pub async fn recover_interrupted(store: &Store) -> Result<usize> {
    store.call(|conn|Ok(conn.execute("UPDATE reminder_jobs SET status='unknown',completed_at=?1,send_error='process stopped during delivery' WHERE status='dispatching'",[now()])?)).await
}

/// Release a claimed job only when the caller positively knows no external send
/// was submitted. An uncertain network result must use `Delivery::Unknown`.
pub async fn release_claim(store: &Store, id: &str) -> Result<bool> {
    let id = id.to_owned();
    store
        .call(move |conn| {
            Ok(conn.execute(
                "UPDATE reminder_jobs SET status='pending' WHERE id=?1 AND status='dispatching'",
                [id],
            )? == 1)
        })
        .await
}

pub async fn maintain(store: &Store, retention_seconds: i64) -> Result<usize> {
    store.call(move|conn|Ok(conn.execute("DELETE FROM reminder_jobs WHERE status IN ('sent','failed','cancelled') AND completed_at<?1",[now()-retention_seconds.max(300)])?)).await
}

pub async fn claim_due(store: &Store, limit: usize) -> Result<Vec<ReminderDue>> {
    let limit = limit.min(16);
    store.call(move|conn|{
        let tx=conn.transaction()?;
        let epoch=tx.query_row("SELECT epoch FROM plugin_states WHERE id='reminder' AND enabled=1",[],|r|r.get::<_,i64>(0)).optional()?;
        let Some(epoch)=epoch else{return Ok(vec![])};
        let mut stmt=tx.prepare("SELECT id,scope_json,actor_id,text,due_at FROM reminder_jobs j WHERE status='pending' AND due_at<=?1 AND NOT EXISTS(SELECT 1 FROM scope_access a WHERE a.scope=j.scope_key AND a.enabled=0) ORDER BY due_at LIMIT ?2")?;
        let rows=stmt.query_map(params![now(),limit as i64],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
        drop(stmt);
        let mut jobs=Vec::new();
        for(id,scope,actor_id,text,due_at)in rows{
            tx.execute("UPDATE reminder_jobs SET status='dispatching' WHERE id=?1 AND status='pending'",[&id])?;
            jobs.push(ReminderDue{id,scope:serde_json::from_str(&scope)?,actor_id,text,due_at,plugin_epoch:epoch});
        }
        tx.commit()?;Ok(jobs)
    }).await
}

pub async fn finish_delivery(
    store: &Store,
    id: &str,
    outcome: Delivery,
    error: Option<&str>,
) -> Result<()> {
    let id = id.to_owned();
    let error = error.map(|s| s.chars().take(500).collect::<String>());
    store.call(move|conn|{let status=match outcome{Delivery::Sent=>"sent",Delivery::KnownFailure=>"failed",Delivery::Unknown=>"unknown"};conn.execute("UPDATE reminder_jobs SET status=?2,completed_at=?3,send_error=?4 WHERE id=?1 AND status='dispatching'",params![id,status,now(),error])?;Ok(())}).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        tools::{Gateway, ToolOutcome},
        types::Scene,
    };
    #[tokio::test]
    async fn definitely_unsubmitted_claim_survives_shutdown_and_can_run_later() {
        let store = Store::open(":memory:", 16).await.unwrap();
        store
            .activate_plugins(vec!["reminder".into()])
            .await
            .unwrap();
        migrate(&store).await.unwrap();
        let mut registry = ToolRegistry::new();
        register(&mut registry, 10).unwrap();
        let gateway = Gateway::new(store.clone(), "UTC", registry, vec![]).unwrap();
        gateway.migrate().await.unwrap();
        let scope = Scope {
            bot_id: 1,
            scene: Scene::Group,
            peer_id: 2,
        };
        let id = match gateway
            .direct(
                scope.clone(),
                3,
                "reminder_create",
                json!({"after_seconds":3600,"text":"durable job"}),
                now(),
            )
            .await
            .unwrap()
        {
            ToolOutcome::Completed { result, .. } => result["id"].as_str().unwrap().to_owned(),
            _ => panic!("explicit command must complete"),
        };
        store
            .call(|conn| {
                conn.execute("UPDATE reminder_jobs SET due_at=0", [])?;
                Ok(())
            })
            .await
            .unwrap();
        let jobs = claim_due(&store, 1).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, id);
        // Shutdown begins before Host::send is entered, so retrying is safe.
        assert!(release_claim(&store, &id).await.unwrap());
        assert_eq!(recover_interrupted(&store).await.unwrap(), 0);
        let retry = claim_due(&store, 1).await.unwrap();
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0].id, id);
        assert_eq!(retry[0].scope, scope);
        finish_delivery(&store, &id, Delivery::Sent, None)
            .await
            .unwrap();
        assert!(!release_claim(&store, &id).await.unwrap());
        assert!(claim_due(&store, 1).await.unwrap().is_empty());
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn disabled_chats_pause_reminders_without_starving_enabled_chats() {
        let store = Store::open(":memory:", 16).await.unwrap();
        store
            .activate_plugins(vec!["reminder".into()])
            .await
            .unwrap();
        migrate(&store).await.unwrap();
        let disabled = Scope {
            bot_id: 1,
            scene: Scene::Group,
            peer_id: 2,
        };
        let enabled = Scope {
            bot_id: 2,
            ..disabled.clone()
        };
        let a = disabled.clone();
        let b = enabled.clone();
        store.call(move|db|{
            for (id,scope) in [("disabled",a),("enabled",b)] { db.execute("INSERT INTO reminder_jobs(id,operation_id,scope_key,scope_json,actor_id,text,due_at,status,created_at) VALUES(?1,?1,?2,?3,7,'reminder',0,'pending',0)",params![id,scope.key(),serde_json::to_string(&scope)?])?; }Ok(())
        }).await.unwrap();
        let access = crate::access::AccessStore::new(store.clone());
        access.set_enabled(disabled.clone(), false).await.unwrap();
        let due = claim_due(&store, 1).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].scope, enabled);
        assert!(claim_due(&store, 1).await.unwrap().is_empty());
        access.set_enabled(disabled.clone(), true).await.unwrap();
        let due = claim_due(&store, 1).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].scope, disabled);
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn list_preserves_ten_max_length_unicode_reminders() {
        let store = Store::open(":memory:", 32).await.unwrap();
        store
            .activate_plugins(vec!["reminder".into()])
            .await
            .unwrap();
        migrate(&store).await.unwrap();
        let mut registry = ToolRegistry::new();
        register(&mut registry, 10).unwrap();
        let gateway = Gateway::new(store.clone(), "UTC", registry, vec![]).unwrap();
        gateway.migrate().await.unwrap();
        let scope = Scope {
            bot_id: 1,
            scene: Scene::Group,
            peer_id: 2,
        };
        let text = "🌸".repeat(200);
        for _ in 0..10 {
            gateway
                .direct(
                    scope.clone(),
                    3,
                    "reminder_create",
                    json!({"after_seconds":3600,"text":text}),
                    now(),
                )
                .await
                .unwrap();
        }
        let ToolOutcome::Completed {
            result, receipt, ..
        } = gateway
            .direct(scope, 3, "reminder_list", json!({}), now())
            .await
            .unwrap()
        else {
            panic!("expected a read result")
        };
        let reminders = result["reminders"].as_array().unwrap();
        assert_eq!(reminders.len(), 10);
        assert!(reminders.iter().all(|r| r["text"] == text));
        assert_eq!(receipt.matches(&text).count(), 10);
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn restart_preserves_confirmed_jobs_but_invalidates_pending_approval() {
        use crate::types::ContextGuard;
        use tokio_util::sync::CancellationToken;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("restart.sqlite3");
        let store = Store::open(&path, 32).await.unwrap();
        store
            .activate_plugins(vec!["ai".into(), "reminder".into()])
            .await
            .unwrap();
        migrate(&store).await.unwrap();
        store.call(|db|{db.execute("INSERT INTO ai_conversations(id,scope,mode) VALUES('test','1:group:2','group')",[])?;Ok(())}).await.unwrap();
        let mut registry = ToolRegistry::new();
        register(&mut registry, 10).unwrap();
        let gateway = Gateway::new(
            store.clone(),
            "UTC",
            registry.clone(),
            vec!["reminder_create".into()],
        )
        .unwrap();
        gateway.migrate().await.unwrap();
        let scope = Scope {
            bot_id: 1,
            scene: Scene::Group,
            peer_id: 2,
        };
        let ctx = gateway
            .context(
                scope.clone(),
                3,
                ContextGuard {
                    conversation_id: "test".into(),
                    epoch: 1,
                },
                store.epoch("ai").await.unwrap(),
                "turn".into(),
                now(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Pending { token, .. } = gateway
            .execute(
                &ctx,
                "reminder_create",
                json!({"after_seconds":3600,"text":"confirmed durable reminder"}),
            )
            .await
            .unwrap()
        else {
            panic!("expected approval")
        };
        let ToolOutcome::Completed { result, .. } =
            gateway.confirm(&scope, 3, &token).await.unwrap()
        else {
            panic!("expected confirmed job")
        };
        let job_id = result["id"].as_str().unwrap().to_owned();
        let ToolOutcome::Pending {
            token: pending_token,
            ..
        } = gateway
            .execute(
                &ctx,
                "reminder_create",
                json!({"after_seconds":3600,"text":"unconfirmed reminder"}),
            )
            .await
            .unwrap()
        else {
            panic!("expected approval")
        };
        let old_epoch = store.epoch("reminder").await.unwrap();
        store.stop().await.unwrap();
        drop(gateway);
        drop(store);

        let restarted = Store::open(&path, 32).await.unwrap();
        restarted
            .activate_plugins(vec!["ai".into(), "reminder".into()])
            .await
            .unwrap();
        migrate(&restarted).await.unwrap();
        let gateway = Gateway::new(
            restarted.clone(),
            "UTC",
            registry,
            vec!["reminder_create".into()],
        )
        .unwrap();
        gateway.migrate().await.unwrap();
        assert!(gateway.confirm(&scope, 3, &pending_token).await.is_err());
        restarted
            .call(|db| {
                db.execute("UPDATE reminder_jobs SET due_at=0", [])?;
                Ok(())
            })
            .await
            .unwrap();
        let jobs = claim_due(&restarted, 4).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job_id);
        let current_epoch = restarted.epoch("reminder").await.unwrap();
        assert!(current_epoch > old_epoch);
        assert_eq!(jobs[0].plugin_epoch, current_epoch);
        finish_delivery(&restarted, &job_id, Delivery::Sent, None)
            .await
            .unwrap();
        assert!(claim_due(&restarted, 4).await.unwrap().is_empty());
        restarted.stop().await.unwrap();
    }
}
