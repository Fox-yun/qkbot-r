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
    register_with_confirmation(registry, max_active, false)
}

pub fn register_with_confirmation(
    registry: &mut ToolRegistry,
    max_active: usize,
    require_confirmation: bool,
) -> Result<()> {
    registry.register(Arc::new(ReminderList))?;
    registry.register(Arc::new(ReminderCreate {
        max_active: max_active.clamp(1, 10),
        require_confirmation,
    }))?;
    registry.register(Arc::new(ReminderCancel {
        require_confirmation,
    }))
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
        .await?;
    store
        .migrate(
            "reminder",
            2,
            "ALTER TABLE reminder_jobs ADD COLUMN outbox_id TEXT;
CREATE UNIQUE INDEX reminder_outbox ON reminder_jobs(outbox_id) WHERE outbox_id IS NOT NULL;",
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
    require_confirmation: bool,
}
impl Tool for ReminderCreate {
    fn requires_confirmation(&self) -> bool {
        self.require_confirmation
    }

    fn spec(&self) -> ToolSpec {
        spec(
            "reminder_create",
            if self.require_confirmation {
                "按用户明确请求创建本人本会话提醒；after_seconds从本条消息收信时刻起计秒，20分钟=1200。需本人确认。"
            } else {
                "按用户明确请求直接创建本人本会话提醒，无需再次确认；after_seconds从本条消息收信时刻起计秒，20分钟=1200。时间或内容不明先问。"
            },
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
            "查询当前用户在本会话的提醒及真实ID，最多10条，待处理优先。取消前先查询，按内容和时间匹配；投递状态unknown不得自动重试。",
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
struct ReminderCancel {
    require_confirmation: bool,
}
impl Tool for ReminderCancel {
    fn requires_confirmation(&self) -> bool {
        self.require_confirmation
    }

    fn spec(&self) -> ToolSpec {
        spec(
            "reminder_cancel",
            if self.require_confirmation {
                "取消当前用户本会话待发送提醒；先用reminder_list取得真实ID，多个候选不明确先问。需本人确认。"
            } else {
                "按用户明确请求直接取消本人本会话待发送提醒，无需再次确认；先用reminder_list取得真实ID，多个候选不明确先问，不猜ID。"
            },
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
    pub outbox_id: String,
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

/// The caller must serialize this with delivery: no attempt for this account may
/// still be active. Only durable evidence of no submission permits another send.
pub async fn recover_interrupted(store: &Store, bot_id: i64) -> Result<usize> {
    ensure!(bot_id > 0, "invalid reminder account");
    let prefix = format!("{bot_id}:%");
    store.call_wait(move |conn| {
        let tx = conn.transaction()?;
        let mut query = tx.prepare("SELECT id,outbox_id FROM reminder_jobs WHERE status='dispatching' AND scope_key LIKE ?1")?;
        let rows = query.query_map([prefix], |r| Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
        drop(query);
        for (id, outbox_id) in &rows {
            let ledger: Option<(String,String)> = if let Some(outbox_id) = outbox_id {
                tx.query_row("SELECT state,detail FROM outbox WHERE id=?1", [outbox_id], |r| Ok((r.get(0)?,r.get(1)?))).optional()?
            } else { None };
            let (status, detail) = match (outbox_id, &ledger) {
                // A pre-v2 claim has no correlation and cannot safely be retried.
                (None, _) => ("unknown", "interrupted legacy delivery".to_owned()),
                (Some(_), None) => ("pending", String::new()),
                (_, Some((state,_))) if state == "prepared" => ("pending", String::new()),
                (_, Some((state,detail))) if state == "failed" && (detail.starts_with("not submitted") || detail == "process stopped before submission" || detail == "runtime stopped before submission") => ("pending", String::new()),
                (_, Some((state,_))) if state == "confirmed" => ("sent", String::new()),
                (_, Some((state,detail))) if state == "failed" => ("failed", detail.clone()),
                _ => ("unknown", "interrupted delivery; submission may have succeeded".to_owned()),
            };
            if status == "pending" {
                tx.execute("UPDATE outbox SET state='failed',detail='not submitted: reminder attempt interrupted',updated_at=?2 WHERE id=?1 AND state='prepared'",params![outbox_id,now()])?;
                tx.execute("UPDATE reminder_jobs SET status='pending',outbox_id=NULL,completed_at=NULL,send_error=NULL WHERE id=?1",[id])?;
            } else {
                tx.execute("UPDATE reminder_jobs SET status=?2,completed_at=?3,send_error=?4 WHERE id=?1",params![id,status,now(),detail])?;
            }
        }
        tx.commit()?; Ok(rows.len())
    }).await
}

/// Release a claimed job only when the caller positively knows no external send
/// was submitted. An uncertain network result must use `Delivery::Unknown`.
pub async fn release_claim(store: &Store, due: &ReminderDue) -> Result<bool> {
    let id = due.id.clone();
    let outbox_id = due.outbox_id.clone();
    store
        .call_wait(move |conn| {
            let tx = conn.transaction()?;
            let changed = tx.execute(
                "UPDATE reminder_jobs SET status='pending',outbox_id=NULL,completed_at=NULL,send_error=NULL WHERE id=?1 AND status='dispatching' AND outbox_id=?2 AND NOT EXISTS(SELECT 1 FROM outbox WHERE id=?2 AND state IN ('confirmed','unknown'))",
                params![id,outbox_id],
            )? == 1;
            if changed {
                tx.execute("UPDATE outbox SET state='failed',detail='not submitted: reminder attempt deferred',updated_at=?2 WHERE id=?1 AND state IN ('prepared','submitted')",params![outbox_id,now()])?;
            }
            tx.commit()?; Ok(changed)
        })
        .await
}

pub async fn maintain(store: &Store, retention_seconds: i64) -> Result<usize> {
    store.call(move|conn|Ok(conn.execute("DELETE FROM reminder_jobs WHERE status IN ('sent','failed','cancelled') AND completed_at<?1",[now()-retention_seconds.max(300)])?)).await
}

pub async fn claim_due(store: &Store, bot_id: i64) -> Result<Option<ReminderDue>> {
    ensure!(bot_id > 0, "invalid reminder account");
    store.call(move|conn|{
        let tx=conn.transaction()?;
        let epoch=tx.query_row("SELECT epoch FROM plugin_states WHERE id='reminder' AND enabled=1",[],|r|r.get::<_,i64>(0)).optional()?;
        let Some(epoch)=epoch else{return Ok(None)};
        let row=tx.query_row("SELECT id,scope_json,actor_id,text,due_at,scope_key FROM reminder_jobs j WHERE status='pending' AND due_at<=?1 AND scope_key LIKE ?2 AND NOT EXISTS(SELECT 1 FROM scope_access a WHERE a.scope=j.scope_key AND a.enabled=0) ORDER BY due_at,id LIMIT 1",params![now(),format!("{bot_id}:%")],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?,r.get::<_,String>(5)?))).optional()?;
        let Some((id,scope,actor_id,text,due_at,scope_key))=row else{return Ok(None)};
        let scope:Scope=serde_json::from_str(&scope)?;
        ensure!(scope.bot_id==bot_id && scope.key()==scope_key,"reminder account metadata does not match");
        let outbox_id=format!("reminder:{}",uuid::Uuid::new_v4());
        tx.execute("UPDATE reminder_jobs SET status='dispatching',outbox_id=?2 WHERE id=?1 AND status='pending'",params![id,outbox_id])?;
        tx.commit()?;Ok(Some(ReminderDue{id,outbox_id,scope,actor_id,text,due_at,plugin_epoch:epoch}))
    }).await
}

pub async fn finish_delivery(
    store: &Store,
    due: &ReminderDue,
    outcome: Delivery,
    error: Option<&str>,
) -> Result<()> {
    let id = due.id.clone();
    let outbox_id = due.outbox_id.clone();
    let error = error.map(|s| s.chars().take(500).collect::<String>());
    store.call_wait(move|conn|{let status=match outcome{Delivery::Sent=>"sent",Delivery::KnownFailure=>"failed",Delivery::Unknown=>"unknown"};conn.execute("UPDATE reminder_jobs SET status=?2,completed_at=?3,send_error=?4 WHERE id=?1 AND status='dispatching' AND outbox_id=?5",params![id,status,now(),error,outbox_id])?;Ok(())}).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        tools::{Gateway, ToolOutcome},
        types::Scene,
    };
    use std::time::Duration;

    async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("reminder regression timed out")
    }
    async fn seed(store: &Store, id: &str, bot_id: i64, status: &str, outbox_id: Option<&str>) {
        let (id, status, outbox_id) = (
            id.to_owned(),
            status.to_owned(),
            outbox_id.map(str::to_owned),
        );
        let scope = Scope {
            bot_id,
            scene: Scene::Group,
            peer_id: 2,
        };
        store.call(move|db|{
            db.execute("INSERT INTO reminder_jobs(id,operation_id,scope_key,scope_json,actor_id,text,due_at,status,created_at,outbox_id) VALUES(?1,?1,?2,?3,7,?1,0,?4,0,?5)",params![id,scope.key(),serde_json::to_string(&scope)?,status,outbox_id])?;
            Ok(())
        }).await.unwrap();
    }
    async fn status(store: &Store, id: &str) -> String {
        let id = id.to_owned();
        store
            .call(move |db| {
                Ok(
                    db.query_row("SELECT status FROM reminder_jobs WHERE id=?1", [id], |r| {
                        r.get(0)
                    })?,
                )
            })
            .await
            .unwrap()
    }
    async fn test_store(capacity: usize) -> Store {
        let store = Store::open(":memory:", capacity).await.unwrap();
        store
            .activate_plugins(vec!["reminder".into()])
            .await
            .unwrap();
        migrate(&store).await.unwrap();
        store
    }

    #[tokio::test]
    async fn claims_and_recovery_are_isolated_to_the_current_account() {
        let store = test_store(8).await;
        seed(&store, "a-current", 1, "pending", None).await;
        seed(&store, "b-other", 11, "pending", None).await;
        seed(
            &store,
            "c-other-claimed",
            2,
            "dispatching",
            Some("other-attempt"),
        )
        .await;
        let due = claim_due(&store, 1).await.unwrap().unwrap();
        assert_eq!(due.scope.bot_id, 1);
        assert_eq!(due.id, "a-current");
        assert!(claim_due(&store, 1).await.unwrap().is_none());
        assert_eq!(recover_interrupted(&store, 1).await.unwrap(), 1);
        assert_eq!(status(&store, "a-current").await, "pending");
        assert_eq!(status(&store, "b-other").await, "pending");
        assert_eq!(status(&store, "c-other-claimed").await, "dispatching");
        assert!(claim_due(&store, 0).await.is_err());
        assert_eq!(claim_due(&store, 11).await.unwrap().unwrap().id, "b-other");
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn recovery_uses_submission_evidence_and_never_retries_unknown_or_confirmed_sends() {
        let store = test_store(16).await;
        let cases = [
            ("missing", None, "", "pending"),
            ("prepared", Some("prepared"), "", "pending"),
            ("submitted", Some("submitted"), "", "unknown"),
            (
                "uncertain",
                Some("unknown"),
                "lost acknowledgement",
                "unknown",
            ),
            ("confirmed", Some("confirmed"), "", "sent"),
            ("failed", Some("failed"), "remote rejected", "failed"),
            (
                "deferred",
                Some("failed"),
                "not submitted; deferred: unverified",
                "pending",
            ),
            (
                "stopped",
                Some("failed"),
                "process stopped before submission",
                "pending",
            ),
        ];
        for (id, ledger, detail, _) in cases {
            seed(&store, id, 1, "dispatching", Some(id)).await;
            if let Some(ledger) = ledger {
                store.call(move|db|{db.execute("INSERT INTO outbox(id,scope,origin_plugin,plugin_epoch,body,state,detail,created_at,updated_at) VALUES(?1,'1:group:2','reminder',1,'test',?2,?3,0,0)",params![id,ledger,detail])?;Ok(())}).await.unwrap();
            }
        }
        seed(&store, "legacy", 1, "dispatching", None).await;
        assert_eq!(recover_interrupted(&store, 1).await.unwrap(), 9);
        for (id, _, _, expected) in cases {
            assert_eq!(status(&store, id).await, expected, "{id}");
        }
        assert_eq!(status(&store, "legacy").await, "unknown");
        assert_eq!(recover_interrupted(&store, 1).await.unwrap(), 0);
        let mut claimed = Vec::new();
        while let Some(due) = claim_due(&store, 1).await.unwrap() {
            claimed.push(due.id);
        }
        assert_eq!(claimed, vec!["deferred", "missing", "prepared", "stopped"]);
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn finalization_and_unsubmitted_release_wait_past_queue_backpressure() {
        let store = test_store(1).await;
        seed(&store, "a-finish", 1, "pending", None).await;
        seed(&store, "b-release", 1, "pending", None).await;
        let sent = claim_due(&store, 1).await.unwrap().unwrap();
        let deferred = claim_due(&store, 1).await.unwrap().unwrap();
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let blocking_store = store.clone();
        let blocker = tokio::spawn(async move {
            blocking_store
                .call(move |_| {
                    let _ = entered.send(());
                    blocked.recv_timeout(Duration::from_secs(10))?;
                    Ok(())
                })
                .await
        });
        within(started).await.unwrap();
        let mut queued = Box::pin(store.call(|_| Ok(())));
        assert!(futures_util::poll!(&mut queued).is_pending());
        assert!(store.call(|_| Ok(())).await.is_err());
        let mut finish = Box::pin(finish_delivery(&store, &sent, Delivery::Sent, None));
        let mut restore = Box::pin(release_claim(&store, &deferred));
        assert!(futures_util::poll!(&mut finish).is_pending());
        assert!(futures_util::poll!(&mut restore).is_pending());
        release.send(()).unwrap();
        within(blocker).await.unwrap().unwrap();
        within(queued).await.unwrap();
        within(finish).await.unwrap();
        assert!(within(restore).await.unwrap());
        assert_eq!(status(&store, "a-finish").await, "sent");
        assert_eq!(status(&store, "b-release").await, "pending");
        assert_eq!(claim_due(&store, 1).await.unwrap().unwrap().id, "b-release");
        assert!(claim_due(&store, 1).await.unwrap().is_none());
        store.stop().await.unwrap();
    }

    struct DeliveryMock {
        host: Arc<Host>,
        router: Arc<crate::host::Router>,
        requests: tokio::sync::mpsc::Receiver<tokio::sync::oneshot::Sender<Value>>,
        cancel: tokio_util::sync::CancellationToken,
        server: tokio::task::JoinHandle<()>,
    }
    impl DeliveryMock {
        async fn start() -> Self {
            use axum::{Json, routing::post};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (sender, requests) = tokio::sync::mpsc::channel(8);
            let app = axum::Router::new()
                .route(
                    "/api/get_login_info",
                    post(|| async { Json(json!({"status":"ok","retcode":0,"data":{"uin":1}})) }),
                )
                .route(
                    "/api/send_group_message",
                    post(move || {
                        let sender = sender.clone();
                        async move {
                            let (reply, receive) = tokio::sync::oneshot::channel();
                            sender.send(reply).await.unwrap();
                            Json(
                                receive
                                    .await
                                    .unwrap_or(json!({"status":"failed","retcode":-1})),
                            )
                        }
                    }),
                );
            let cancel = tokio_util::sync::CancellationToken::new();
            let stop = cancel.clone();
            let server = tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(stop.cancelled_owned())
                    .await
                    .unwrap();
            });
            let mut config =
                crate::config::Config::from_toml(include_str!("../../config.example.toml"))
                    .unwrap();
            config.app.database = ":memory:".into();
            config.plugins.ai.enabled = false;
            config.plugins.reminder.enabled = true;
            config.milky.url = url;
            config.milky.bot_id = Some(1);
            config.milky.request_timeout_secs = 2;
            let host = Host::create(config).await.unwrap();
            host.milky.login().await.unwrap();
            let router = Arc::new(crate::host::Router::new(host.clone()));
            Self {
                host,
                router,
                requests,
                cancel,
                server,
            }
        }
        async fn request(&mut self) -> tokio::sync::oneshot::Sender<Value> {
            within(self.requests.recv()).await.unwrap()
        }
        async fn stop(self) {
            self.host.cancel.cancel();
            within(self.router.shutdown()).await.unwrap();
            self.cancel.cancel();
            within(self.server).await.unwrap();
        }
    }
    fn acknowledgement(seq: i64) -> Value {
        json!({"status":"ok","retcode":0,"data":{"message_seq":seq,"time":now()}})
    }

    #[tokio::test]
    async fn failed_result_write_recovers_during_runtime_without_consuming_the_remaining_batch() {
        let mut mock = DeliveryMock::start().await;
        seed(&mock.host.store, "a-first", 1, "pending", None).await;
        seed(&mock.host.store, "b-second", 1, "pending", None).await;
        seed(&mock.host.store, "c-other-account", 2, "pending", None).await;
        mock.host.store.call(|db|{db.execute_batch("CREATE TRIGGER fail_reminder_result BEFORE UPDATE OF status ON reminder_jobs WHEN NEW.status='sent' BEGIN SELECT RAISE(ABORT,'injected result write failure'); END;")?;Ok(())}).await.unwrap();
        let router = mock.router.clone();
        let attempt = tokio::spawn(async move { router.reminders().await });
        mock.request().await.send(acknowledgement(10)).unwrap();
        assert!(within(attempt).await.unwrap().is_err());
        assert_eq!(status(&mock.host.store, "a-first").await, "dispatching");
        assert_eq!(status(&mock.host.store, "b-second").await, "pending");
        assert_eq!(status(&mock.host.store, "c-other-account").await, "pending");
        // Even a long-lived result-write failure must keep the referenced send
        // receipt: removing it would make a confirmed delivery look unsubmitted.
        mock.host.store.call(|db|{
            db.execute("UPDATE outbox SET created_at=0",[])?;
            db.execute("INSERT INTO outbox(id,scope,origin_plugin,plugin_epoch,body,state,created_at,updated_at) VALUES('unrelated-old','1:group:2','reminder',1,'old','confirmed',0,0)",[])?;
            Ok(())
        }).await.unwrap();
        mock.router.maintenance().await.unwrap();
        mock.host.store.call(|db|{
            assert_eq!(db.query_row("SELECT count(*) FROM outbox WHERE id=(SELECT outbox_id FROM reminder_jobs WHERE id='a-first')",[],|r|r.get::<_,i64>(0))?,1);
            assert_eq!(db.query_row("SELECT count(*) FROM outbox WHERE id='unrelated-old'",[],|r|r.get::<_,i64>(0))?,0);
            Ok(())
        }).await.unwrap();
        mock.host
            .store
            .call(|db| {
                db.execute_batch("DROP TRIGGER fail_reminder_result;")?;
                Ok(())
            })
            .await
            .unwrap();
        let router = mock.router.clone();
        let attempt = tokio::spawn(async move { router.reminders().await });
        mock.request().await.send(acknowledgement(11)).unwrap();
        within(attempt).await.unwrap().unwrap();
        assert_eq!(status(&mock.host.store, "a-first").await, "sent");
        assert_eq!(status(&mock.host.store, "b-second").await, "sent");
        assert_eq!(status(&mock.host.store, "c-other-account").await, "pending");
        mock.router.reminders().await.unwrap();
        assert!(mock.requests.try_recv().is_err());
        mock.stop().await;
    }

    #[tokio::test]
    async fn live_delivery_is_serialized_and_cancelled_submission_is_never_retried() {
        let mut mock = DeliveryMock::start().await;
        seed(&mock.host.store, "submitted", 1, "pending", None).await;
        let router = mock.router.clone();
        let attempt = tokio::spawn(async move { router.reminders().await });
        let receipt = mock.request().await;
        mock.router.reminders().await.unwrap();
        assert_eq!(status(&mock.host.store, "submitted").await, "dispatching");
        assert!(mock.requests.try_recv().is_err());
        attempt.abort();
        assert!(within(attempt).await.unwrap_err().is_cancelled());
        let _ = receipt.send(acknowledgement(12));
        mock.router.reminders().await.unwrap();
        assert_eq!(status(&mock.host.store, "submitted").await, "unknown");
        mock.router.reminders().await.unwrap();
        assert!(mock.requests.try_recv().is_err());
        mock.stop().await;
    }

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
        let job = claim_due(&store, 1).await.unwrap().unwrap();
        assert_eq!(job.id, id);
        // Shutdown begins before Host::send is entered, so retrying is safe.
        assert!(release_claim(&store, &job).await.unwrap());
        assert_eq!(recover_interrupted(&store, 1).await.unwrap(), 0);
        let retry = claim_due(&store, 1).await.unwrap().unwrap();
        assert_eq!(retry.id, id);
        assert_eq!(retry.scope, scope);
        assert_ne!(job.outbox_id, retry.outbox_id);
        assert!(
            !release_claim(&store, &job).await.unwrap(),
            "old attempt cannot release the current claim"
        );
        finish_delivery(&store, &job, Delivery::Unknown, None)
            .await
            .unwrap();
        assert_eq!(status(&store, &id).await, "dispatching");
        finish_delivery(&store, &retry, Delivery::Sent, None)
            .await
            .unwrap();
        assert!(!release_claim(&store, &retry).await.unwrap());
        assert!(claim_due(&store, 1).await.unwrap().is_none());
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
            peer_id: 3,
            ..disabled.clone()
        };
        let a = disabled.clone();
        let b = enabled.clone();
        store.call(move|db|{
            for (id,scope) in [("disabled",a),("enabled",b)] { db.execute("INSERT INTO reminder_jobs(id,operation_id,scope_key,scope_json,actor_id,text,due_at,status,created_at) VALUES(?1,?1,?2,?3,7,'reminder',0,'pending',0)",params![id,scope.key(),serde_json::to_string(&scope)?])?; }Ok(())
        }).await.unwrap();
        let access = crate::access::AccessStore::new(store.clone());
        access.set_enabled(disabled.clone(), false).await.unwrap();
        let due = claim_due(&store, 1).await.unwrap().unwrap();
        assert_eq!(due.scope, enabled);
        assert!(claim_due(&store, 1).await.unwrap().is_none());
        access.set_enabled(disabled.clone(), true).await.unwrap();
        let due = claim_due(&store, 1).await.unwrap().unwrap();
        assert_eq!(due.scope, disabled);
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
        register_with_confirmation(&mut registry, 10, true).unwrap();
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
        let job = claim_due(&restarted, scope.bot_id).await.unwrap().unwrap();
        assert_eq!(job.id, job_id);
        let current_epoch = restarted.epoch("reminder").await.unwrap();
        assert!(current_epoch > old_epoch);
        assert_eq!(job.plugin_epoch, current_epoch);
        finish_delivery(&restarted, &job, Delivery::Sent, None)
            .await
            .unwrap();
        assert!(claim_due(&restarted, scope.bot_id).await.unwrap().is_none());
        restarted.stop().await.unwrap();
    }
}
