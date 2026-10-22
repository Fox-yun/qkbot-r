//! Three phases: short admission transaction, async IO, short completion transaction.
use super::*;
use async_trait::async_trait;

#[derive(Clone, Copy)]
pub struct ExternalQuota {
    pub per_turn: usize,
    pub per_user: usize,
    pub per_scope: usize,
    pub global: usize,
}
#[async_trait]
pub trait ExternalTool: Send + Sync {
    fn quota(&self) -> ExternalQuota;
    async fn execute(
        &self,
        args: &Value,
        operation: &str,
        cancel: &CancellationToken,
    ) -> Result<ToolResult>;
}

#[derive(Debug)]
pub struct ExternalInvalidated;
impl std::fmt::Display for ExternalInvalidated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("外部工具任务权限已失效")
    }
}
impl std::error::Error for ExternalInvalidated {}

pub(super) fn check_conversation(
    db: &rusqlite::Connection,
    bound: &BoundContext,
    name: &str,
) -> Result<()> {
    if let Some(guard) = &bound.guard {
        let settings: String = db.query_row(
            "SELECT settings FROM ai_conversations WHERE id=?1",
            [&guard.conversation_id],
            |r| r.get(0),
        )?;
        let settings: Value = serde_json::from_str(&settings)?;
        // Trigger admission owns pause/once. A consumed one-shot grant leaves
        // paused=true; administrative changes invalidate the conversation epoch.
        if let Some(allowed) = settings["allowed_tools"].as_array() {
            ensure!(
                allowed.iter().any(|value| value.as_str() == Some(name)),
                "会话未允许此工具"
            );
        }
    }
    Ok(())
}

/// Also used by the outbox immediately before a final answer is submitted.
pub(crate) fn check_external_result(db: &rusqlite::Connection, operation: &str) -> Result<()> {
    check_external_operation(db, operation, false)
}

fn check_external_operation(
    db: &rusqlite::Connection,
    operation: &str,
    executing: bool,
) -> Result<()> {
    let (context, plugin, name, revision, status): (String, String, String, i64, String) = db.query_row(
        "SELECT r.context_json,r.plugin_id,r.tool_name,u.plugin_revision,r.status FROM tool_runs r JOIN external_tool_usage u USING(operation_id) WHERE operation_id=?1",
        [operation], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
    let bound: BoundContext = serde_json::from_str(&context)?;
    ensure!(
        ["succeeded", "failed"].contains(&status.as_str()) || (executing && status == "executing"),
        "搜索操作已失效"
    );
    authorize(db, &bound, &plugin)?;
    crate::catalog::check_revision(db, &bound.scope.key(), &plugin, revision)?;
    check_conversation(db, &bound, &name)
}

impl Gateway {
    pub fn with_cancellation(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub(super) async fn migrate_external(&self) -> Result<()> {
        self.store.migrate("external_tools", 1, "
            CREATE TABLE external_tool_usage (
              operation_id TEXT PRIMARY KEY, tool_name TEXT NOT NULL, bot_id INTEGER NOT NULL,
              actor_id INTEGER NOT NULL, scope_key TEXT NOT NULL, turn_id TEXT NOT NULL,
              plugin_revision INTEGER NOT NULL, created_at INTEGER NOT NULL
            );
            CREATE INDEX external_tool_time ON external_tool_usage(tool_name,created_at);
            CREATE INDEX external_tool_turn ON external_tool_usage(tool_name,turn_id,scope_key,actor_id);
        ").await?;
        // Never replay an interrupted external request, even if the provider may have charged it.
        self.store.call(|db| {
            db.execute("UPDATE tool_runs SET status='failed',result_json='{}',receipt='外部请求已中断，未重试' WHERE status='executing' AND operation_id IN (SELECT operation_id FROM external_tool_usage) AND (created_at<?1 OR NOT EXISTS(SELECT 1 FROM plugin_states p WHERE p.id=tool_runs.plugin_id AND p.enabled=1 AND p.epoch=json_extract(tool_runs.context_json,'$.provider_epoch')))", [now()-60])?;
            db.execute("DELETE FROM external_tool_usage WHERE created_at<?1 AND operation_id NOT IN (SELECT operation_id FROM tool_runs)", [now()-3600])?;
            Ok(())
        }).await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_external(
        &self,
        tool: Arc<dyn Tool>,
        args: Value,
        bound: BoundContext,
        operation: String,
        turn: String,
        expected_revision: Option<i64>,
        source: Option<String>,
        cancel: CancellationToken,
    ) -> Result<ToolOutcome> {
        let spec = tool.spec();
        let external = tool.external().context("external executor missing")?;
        check_runtime(self.epochs()?, &bound, spec.plugin)?;
        let quota = external.quota();
        let context = serde_json::to_string(&bound)?;
        let args_hash = hash(&canonical(&args)?);
        let schema_hash = hash(&canonical(&spec.parameters)?);
        let admitted_tool = tool.clone();
        let admission_bound = bound.clone();
        let id = operation.clone();
        let stopped = cancel.clone();
        let runtime_stop = self.cancel.clone();
        let previous = self.store.call(move |db| {
            let tx = db.transaction()?;
            ensure!(!stopped.is_cancelled() && !runtime_stop.is_cancelled(), "搜索已取消");
            authorize(&tx, &admission_bound, spec.plugin).map_err(|_| ExternalInvalidated)?;
            check_conversation(&tx, &admission_bound, spec.name).map_err(|_| ExternalInvalidated)?;
            ensure!(admitted_tool.permits(&admission_bound.scope, admission_bound.actor_id), "工具未获授权");
            let scope = admission_bound.scope.key();
            let revision = crate::catalog::allowed_revision(&tx, &scope, spec.plugin)?.ok_or(ExternalInvalidated)?;
            if expected_revision.is_some_and(|expected| expected != revision) { return Err(ExternalInvalidated.into()); }
            if let Some(source) = source { crate::catalog::check_message(&tx, &source, spec.plugin).map_err(|_| ExternalInvalidated)?; }
            if let Some((status, result, receipt)) = tx.query_row(
                "SELECT status,result_json,receipt FROM tool_runs WHERE operation_id=?1", [&id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?))).optional()? {
                check_external_operation(&tx, &id, true).map_err(|_| ExternalInvalidated)?;
                ensure!(["succeeded", "failed"].contains(&status.as_str()), "同一搜索请求已提交，不重复请求");
                return Ok(Some(ToolOutcome::Completed { operation_id: id, result: serde_json::from_str(&result.context("搜索已中断，不重复请求")?)?, receipt: receipt.unwrap_or_default() }));
            }
            let time = now();
            let (global, user, conversation, calls): (i64, i64, i64, i64) = tx.query_row(
                "SELECT COUNT(*),COALESCE(SUM(bot_id=?3 AND actor_id=?4),0),COALESCE(SUM(scope_key=?5),0),COALESCE(SUM(scope_key=?5 AND actor_id=?4 AND turn_id=?6),0) FROM external_tool_usage WHERE tool_name=?1 AND created_at>?2",
                params![spec.name, time-3600, admission_bound.scope.bot_id, admission_bound.actor_id, scope, turn],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            ensure!(global < quota.global as i64 && user < quota.per_user as i64 && conversation < quota.per_scope as i64, "联网搜索每小时额度已用完");
            ensure!(calls < quota.per_turn as i64, "本轮联网搜索次数已用完");
            tx.execute("INSERT INTO external_tool_usage VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", params![id,spec.name,admission_bound.scope.bot_id,admission_bound.actor_id,scope,turn,revision,time])?;
            // The admission row stores only the argument hash; bounded results are retained for idempotency.
            tx.execute("INSERT INTO tool_runs(operation_id,tool_name,tool_version,plugin_id,context_json,args_json,scope_key,actor_id,args_hash,schema_hash,status,created_at) VALUES(?1,?2,?3,?4,?5,'{}',?6,?7,?8,?9,'executing',?10)",
                params![id,spec.name,spec.version,spec.plugin,context,scope,admission_bound.actor_id,args_hash,schema_hash,time])?;
            tx.commit()?;
            Ok(None)
        }).await?;
        if let Some(previous) = previous {
            return Ok(previous);
        }
        let start = std::time::Instant::now();
        let attempt = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ExternalInvalidated.into()),
            _ = self.cancel.cancelled() => Err(ExternalInvalidated.into()),
            result = external.execute(&args, &operation, &cancel) => result,
        };
        let id = operation.clone();
        let stopped = cancel.clone();
        let runtime_stop = self.cancel.clone();
        let result = self.store.call(move |db| {
            let tx = db.transaction()?;
            let valid = !stopped.is_cancelled() && !runtime_stop.is_cancelled()
                && tool.permits(&bound.scope, bound.actor_id) && check_external_operation(&tx, &id, true).is_ok();
            if !valid {
                tx.execute("UPDATE tool_runs SET status='cancelled',result_json=NULL,receipt=NULL WHERE operation_id=?1", [&id])?;
                tx.commit()?;
                return Err(ExternalInvalidated.into());
            }
            let result = match attempt {
                Ok(result) => result,
                Err(_) => ToolResult { value: json!({"status":"failed","code":"external_unavailable","error":"本次联网未成功，未重试"}), receipt: "本次联网未成功，未重试".into() },
            };
            let encoded = canonical(&result.value)?;
            ensure!(encoded.len() <= spec.max_output_bytes && result.receipt.len() <= spec.max_output_bytes, "外部工具结果超过大小限制");
            let status = if result.value["status"] == "failed" { "failed" } else { "succeeded" };
            tx.execute("UPDATE tool_runs SET status=?2,result_json=?3,receipt=?4 WHERE operation_id=?1", params![id,status,encoded,result.receipt])?;
            tx.commit()?;
            Ok(ToolOutcome::Completed { operation_id: id, result: result.value, receipt: result.receipt })
        }).await;
        let (status, result_count) = match &result {
            Ok(ToolOutcome::Completed { result, .. }) => (
                if result["status"] == "failed" {
                    "failed"
                } else {
                    "succeeded"
                },
                result["results"].as_array().map_or(0, Vec::len),
            ),
            _ => ("discarded", 0),
        };
        tracing::info!(operation_id = %operation, tool = spec.name, elapsed_ms = start.elapsed().as_millis(), status, result_count, attempts = 1, "external tool completed");
        result
    }
}
