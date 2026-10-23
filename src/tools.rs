//! Explicit model tools. Only registered, locally validated operations can run.
use crate::{
    store::Store,
    types::{ContextGuard, Scope, now},
};
use anyhow::{Context, Result, bail, ensure};
use chrono_tz::Tz;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, OnceLock},
};
use tokio_util::sync::CancellationToken;
mod external;
pub(crate) use external::check_external_result;
pub use external::{ExternalInvalidated, ExternalQuota, ExternalTool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Read,
    /// External reads may disclose a query and incur provider charges.
    NetworkRead,
    /// Network-backed bounded reads, handled outside the SQLite gateway.
    AsyncRead,
    LocalWrite,
    /// Ends the current AI turn; handled by chat, never by the transaction gateway.
    Reply,
}

/// Administrator display metadata, not an authorization grant or a model tool
/// definition. Every value comes from the registered implementation's spec.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ToolCatalogEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub plugin_id: &'static str,
    pub effect: Effect,
}

#[derive(Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub plugin: &'static str,
    pub version: i64,
    pub description: &'static str,
    pub parameters: Value,
    pub effect: Effect,
    /// Results are visible in the same physical chat as the triggering message.
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub value: Value,
    pub receipt: String,
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// Model-initiated local writes require approval unless the implementation
    /// explicitly permits immediate execution. Authorization still applies.
    fn requires_confirmation(&self) -> bool {
        true
    }
    fn external(&self) -> Option<&dyn ExternalTool> {
        None
    }
    /// Tool policy is checked independently from the plugin's command/event
    /// filter, including immediately before a confirmed operation commits.
    fn permits(&self, _scope: &Scope, _actor: i64) -> bool {
        true
    }
    /// Produce fixed arguments before approval; never trust model-supplied identity.
    fn prepare(
        &self,
        args: Value,
        scope: &Scope,
        actor: i64,
        received_at: i64,
        timezone: Tz,
    ) -> Result<Value>;
    fn preview(&self, args: &Value, timezone: Tz) -> Result<String>;
    /// Must be local, bounded work. Never perform network IO within this transaction.
    fn apply(
        &self,
        tx: &Transaction<'_>,
        args: &Value,
        scope: &Scope,
        actor: i64,
        operation_id: &str,
        now: i64,
    ) -> Result<ToolResult>;
}

/// Publishes the final-reply contract to the model and administrator catalog.
/// The chat dispatcher validates host-issued references and queues the response.
struct ReplyTool;

impl Tool for ReplyTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "reply_message",
            plugin: "ai",
            version: 1,
            description: "提交唯一最终回复，仅用宿主本轮标识。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "reply_to": {
                        "type": ["string", "null"],
                        "description": "message_ref"
                    },
                    "mention_users": {
                        "type": "array",
                        "items": {"type": "string", "minLength": 1},
                        "maxItems": 3
                    },
                    "sticker_ref": {
                        "type": ["string", "null"],
                        "description": "本轮已预览候选的引用，最多一张。"
                    },
                    "text": {"type": "string", "minLength": 1}
                },
                "required": ["text"],
                "additionalProperties": false
            }),
            effect: Effect::Reply,
            max_output_bytes: 0,
        }
    }

    fn prepare(
        &self,
        _args: Value,
        _scope: &Scope,
        _actor: i64,
        _received_at: i64,
        _timezone: Tz,
    ) -> Result<Value> {
        bail!("final replies must be handled by the chat dispatcher")
    }

    fn preview(&self, _args: &Value, _timezone: Tz) -> Result<String> {
        bail!("final replies cannot request transaction confirmation")
    }

    fn apply(
        &self,
        _tx: &Transaction<'_>,
        _args: &Value,
        _scope: &Scope,
        _actor: i64,
        _operation_id: &str,
        _now: i64,
    ) -> Result<ToolResult> {
        bail!("final replies cannot execute inside a tool transaction")
    }
}

/// A model-visible contract only. Chat performs the asynchronous search and
/// binds its candidate references to the current turn.
struct SearchStickersTool;

impl Tool for SearchStickersTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search_stickers",
            plugin: "ai",
            version: 1,
            description: "按回复意图搜索本群表情。看预览后可选一张交给reply_message并结束本轮，也可只回文字。",
            parameters: json!({
                "type":"object",
                "properties":{"query":{"type":"string","minLength":1,"maxLength":256}},
                "required":["query"],
                "additionalProperties":false,
            }),
            effect: Effect::AsyncRead,
            max_output_bytes: 4096,
        }
    }

    fn permits(&self, scope: &Scope, _actor: i64) -> bool {
        scope.scene == crate::types::Scene::Group
    }

    fn prepare(
        &self,
        _args: Value,
        _scope: &Scope,
        _actor: i64,
        _received_at: i64,
        _timezone: Tz,
    ) -> Result<Value> {
        bail!("asynchronous reads must be handled by the chat dispatcher")
    }

    fn preview(&self, _args: &Value, _timezone: Tz) -> Result<String> {
        bail!("asynchronous reads cannot request transaction confirmation")
    }

    fn apply(
        &self,
        _tx: &Transaction<'_>,
        _args: &Value,
        _scope: &Scope,
        _actor: i64,
        _operation_id: &str,
        _now: i64,
    ) -> Result<ToolResult> {
        bail!("asynchronous reads cannot execute inside a tool transaction")
    }
}

pub fn register_reply(registry: &mut ToolRegistry) -> Result<()> {
    registry.register(Arc::new(ReplyTool))?;
    registry.register(Arc::new(SearchStickersTool))
}

fn require_transaction_tool(spec: &ToolSpec) -> Result<()> {
    ensure!(
        matches!(spec.effect, Effect::Read | Effect::LocalWrite),
        "asynchronous reads and final replies must be handled by the chat dispatcher"
    );
    Ok(())
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}
impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        let name = tool.spec().name.to_owned();
        ensure!(!self.tools.contains_key(&name), "duplicate tool: {name}");
        self.tools.insert(name, tool);
        Ok(())
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }
    pub fn catalog(&self) -> Vec<ToolCatalogEntry> {
        self.tools
            .values()
            .map(|tool| {
                let spec = tool.spec();
                ToolCatalogEntry {
                    name: spec.name,
                    description: spec.description,
                    plugin_id: spec.plugin,
                    effect: spec.effect,
                }
            })
            .collect()
    }
    fn get(&self, name: &str) -> Result<Arc<dyn Tool>> {
        self.tools
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {name}"))
    }
}

#[derive(Clone)]
pub struct ToolContext {
    pub scope: Scope,
    pub actor_id: i64,
    pub guard: ContextGuard,
    pub ai_epoch: i64,
    pub provider_epochs: BTreeMap<String, i64>,
    pub plugin_revisions: BTreeMap<String, i64>,
    pub turn_id: String,
    pub received_at: i64,
    pub cancel: CancellationToken,
    pub access_revision: i64,
}

#[derive(Debug, Clone)]
pub enum ToolOutcome {
    Completed {
        operation_id: String,
        result: Value,
        receipt: String,
    },
    Pending {
        operation_id: String,
        token: String,
        preview: String,
        expires_at: i64,
    },
}

#[derive(Clone)]
pub struct Gateway {
    store: Store,
    timezone: Tz,
    registry: ToolRegistry,
    allowed: BTreeSet<String>,
    runtime_epochs: Arc<OnceLock<BTreeMap<String, i64>>>,
    cancel: CancellationToken,
}

#[derive(Clone, Serialize, Deserialize)]
struct BoundContext {
    scope: Scope,
    actor_id: i64,
    guard: Option<ContextGuard>,
    ai_epoch: Option<i64>,
    provider_epoch: i64,
    #[serde(default)]
    access_revision: i64,
}

const MIGRATION: &str = "
CREATE TABLE IF NOT EXISTS tool_runs (
 operation_id TEXT PRIMARY KEY, tool_name TEXT NOT NULL, tool_version INTEGER NOT NULL,
 plugin_id TEXT NOT NULL, context_json TEXT NOT NULL, args_json TEXT NOT NULL,
 scope_key TEXT NOT NULL, actor_id INTEGER NOT NULL,
 args_hash TEXT NOT NULL, schema_hash TEXT NOT NULL, status TEXT NOT NULL, token TEXT UNIQUE,
 expires_at INTEGER, result_json TEXT, receipt TEXT, created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS tool_runs_created ON tool_runs(created_at);
CREATE INDEX IF NOT EXISTS tool_runs_owner ON tool_runs(scope_key,actor_id,status);
";

fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
fn canonical(value: &Value) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}
fn confirmation_preview(preview: &str, token: &str, expires_at: i64) -> String {
    format!(
        "{preview}\n请在 5 分钟内发送：/bot confirm {token}\n拒绝：/bot reject {token}\n确认期限：{expires_at}（Unix 秒）"
    )
}

/// Effects and their outcome commit together. A failed plugin operation rolls
/// back its own writes while leaving a durable, non-retryable failure receipt.
fn apply_and_record(
    tx: Transaction<'_>,
    tool: &dyn Tool,
    args: &Value,
    bound: &BoundContext,
    operation: String,
    cancel: &CancellationToken,
) -> Result<ToolOutcome> {
    let spec = tool.spec();
    require_transaction_tool(&spec)?;
    authorize(&tx, bound, spec.plugin)?;
    ensure!(
        tool.permits(&bound.scope, bound.actor_id),
        "tool is not permitted in this context"
    );
    tx.execute_batch("SAVEPOINT tool_effect")?;
    let attempt = (|| -> Result<ToolResult> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tool.apply(&tx, args, &bound.scope, bound.actor_id, &operation, now())
        }))
        .map_err(|_| anyhow::anyhow!("tool panicked; local changes were rolled back"))??;
        ensure!(
            tool.permits(&bound.scope, bound.actor_id),
            "tool permission changed before commit"
        );
        ensure!(!cancel.is_cancelled(), "turn cancelled");
        authorize(&tx, bound, spec.plugin)?;
        ensure!(
            serde_json::to_string(&result.value)?.len() <= spec.max_output_bytes,
            "tool result exceeds output limit"
        );
        ensure!(
            result.receipt.len() <= spec.max_output_bytes,
            "tool receipt exceeds output limit"
        );
        Ok(result)
    })();
    match attempt {
        Ok(result) => {
            tx.execute_batch("RELEASE tool_effect")?;
            let json = serde_json::to_string(&result.value)?;
            tx.execute("UPDATE tool_runs SET status='succeeded',result_json=?2,receipt=?3 WHERE operation_id=?1",params![operation,json,result.receipt])?;
            tx.commit()?;
            Ok(ToolOutcome::Completed {
                operation_id: operation,
                result: result.value,
                receipt: result.receipt,
            })
        }
        Err(error) => {
            tx.execute_batch("ROLLBACK TO tool_effect; RELEASE tool_effect")?;
            let message = error.to_string().chars().take(500).collect::<String>();
            tx.execute("UPDATE tool_runs SET status='failed',result_json=?2,receipt=?3 WHERE operation_id=?1",params![operation,json!({"error":message}).to_string(),format!("操作失败：{message}")])?;
            tx.commit()?;
            Err(error)
        }
    }
}

fn plugin_epoch(tx: &rusqlite::Connection, id: &str) -> Result<i64> {
    let row = tx
        .query_row(
            "SELECT epoch,enabled FROM plugin_states WHERE id=?1",
            [id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, bool>(1)?)),
        )
        .optional()?;
    match row {
        Some((epoch, true)) => Ok(epoch),
        _ => bail!("plugin {id} is disabled"),
    }
}

fn authorize(tx: &rusqlite::Connection, ctx: &BoundContext, plugin: &str) -> Result<()> {
    crate::access::check_revision(tx, &ctx.scope.key(), ctx.access_revision)?;
    ensure!(
        plugin_epoch(tx, plugin)? == ctx.provider_epoch,
        "plugin changed; operation cancelled"
    );
    if let Some(epoch) = ctx.ai_epoch {
        ensure!(
            plugin_epoch(tx, "ai")? == epoch,
            "AI plugin changed; operation cancelled"
        );
    }
    if let Some(guard) = &ctx.guard {
        let epoch = tx
            .query_row(
                "SELECT epoch FROM ai_conversations WHERE id=?1",
                [&guard.conversation_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        ensure!(
            epoch == Some(guard.epoch),
            "conversation changed; operation cancelled"
        );
    }
    Ok(())
}

fn check_runtime(epochs: &BTreeMap<String, i64>, ctx: &BoundContext, plugin: &str) -> Result<()> {
    ensure!(
        epochs.get(plugin) == Some(&ctx.provider_epoch),
        "tool belongs to a different plugin runtime"
    );
    if let Some(epoch) = ctx.ai_epoch {
        ensure!(
            epochs.get("ai") == Some(&epoch),
            "tool belongs to a different AI runtime"
        );
    }
    Ok(())
}

impl Gateway {
    pub fn new(
        store: Store,
        timezone: &str,
        registry: ToolRegistry,
        allowed_tools: Vec<String>,
    ) -> Result<Self> {
        for name in &allowed_tools {
            registry.get(name)?;
        }
        Ok(Self {
            store,
            timezone: timezone.parse().context("invalid timezone")?,
            registry,
            allowed: allowed_tools.into_iter().collect(),
            runtime_epochs: Arc::new(OnceLock::new()),
            cancel: CancellationToken::new(),
        })
    }
    pub async fn migrate(&self) -> Result<()> {
        self.store.migrate("tools", 1, MIGRATION).await?;
        self.migrate_external().await?;
        if self.runtime_epochs.get().is_none() {
            let epochs = self
                .store
                .call(|conn| {
                    let mut stmt =
                        conn.prepare("SELECT id,epoch FROM plugin_states WHERE enabled=1")?;
                    Ok(stmt
                        .query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                        })?
                        .collect::<std::result::Result<BTreeMap<_, _>, _>>()?)
                })
                .await?;
            // Reinitializing an old gateway must never adopt a newer runtime's permissions.
            let _ = self.runtime_epochs.set(epochs);
        }
        Ok(())
    }

    fn epochs(&self) -> Result<&BTreeMap<String, i64>> {
        self.runtime_epochs
            .get()
            .context("tool gateway has not been initialized")
    }

    pub async fn maintain(&self, retention_seconds: i64) -> Result<()> {
        self.store.call(move |conn| {
            let tx=conn.transaction()?;
            tx.execute("UPDATE tool_runs SET status='expired' WHERE status='pending_confirmation' AND expires_at<=?1",[now()])?;
            tx.execute("UPDATE tool_runs SET status='cancelled',result_json=NULL,receipt=NULL WHERE status='executing' AND created_at<?1 AND operation_id IN (SELECT operation_id FROM external_tool_usage)",[now()-60])?;
            tx.execute("DELETE FROM tool_runs WHERE status IN ('expired','rejected','failed','succeeded','cancelled') AND created_at<?1",[now()-retention_seconds.max(3600)])?;
            tx.execute("DELETE FROM external_tool_usage WHERE created_at<?1 AND operation_id NOT IN (SELECT operation_id FROM tool_runs)",[now()-3600])?;
            tx.commit()?;Ok(())
        }).await
    }

    // All trusted identity/version fields are supplied by the host, never model arguments.
    #[allow(clippy::too_many_arguments)]
    pub async fn context(
        &self,
        scope: Scope,
        actor_id: i64,
        guard: ContextGuard,
        ai_epoch: i64,
        turn_id: String,
        received_at: i64,
        cancel: CancellationToken,
    ) -> Result<ToolContext> {
        let epochs = self.epochs()?.clone();
        let plugins: Vec<String> = epochs.keys().cloned().collect();
        let key = scope.key();
        let (access_revision, plugin_revisions) = self
            .store
            .call(move |db| {
                let access = crate::access::require_enabled(db, &key)?;
                let mut revisions = BTreeMap::new();
                for plugin in plugins {
                    if let Some(revision) = crate::catalog::allowed_revision(db, &key, &plugin)? {
                        revisions.insert(plugin, revision);
                    }
                }
                Ok((access, revisions))
            })
            .await?;
        Ok(ToolContext {
            scope,
            actor_id,
            guard,
            ai_epoch,
            provider_epochs: epochs,
            plugin_revisions,
            turn_id,
            received_at,
            cancel,
            access_revision,
        })
    }

    /// Observation has no explicit actor request authorizing a state change.
    pub(crate) fn permits_observation(&self, name: &str) -> bool {
        self.registry
            .get(name)
            .is_ok_and(|tool| tool.spec().effect != Effect::LocalWrite)
    }

    pub async fn definitions(&self, ctx: &ToolContext) -> Result<Vec<Value>> {
        if ctx.cancel.is_cancelled() {
            return Ok(vec![]);
        }
        let mut definitions = Vec::new();
        for name in &self.allowed {
            let tool = self.registry.get(name)?;
            let spec = tool.spec();
            if !tool.permits(&ctx.scope, ctx.actor_id) {
                continue;
            }
            let Some(epoch) = ctx.provider_epochs.get(spec.plugin) else {
                continue;
            };
            let bound = BoundContext {
                scope: ctx.scope.clone(),
                actor_id: ctx.actor_id,
                guard: Some(ctx.guard.clone()),
                ai_epoch: Some(ctx.ai_epoch),
                provider_epoch: *epoch,
                access_revision: ctx.access_revision,
            };
            if check_runtime(self.epochs()?, &bound, spec.plugin).is_err() {
                continue;
            }
            let plugin = spec.plugin.to_owned();
            let expected_revision = ctx.plugin_revisions.get(spec.plugin).copied();
            if !self
                .store
                .call(move |conn| {
                    let tx = conn.transaction()?;
                    if spec.effect == Effect::NetworkRead
                        && (expected_revision.is_none()
                            || crate::catalog::allowed_revision(&tx, &bound.scope.key(), &plugin)?
                                != expected_revision
                            || external::check_conversation(&tx, &bound, spec.name).is_err())
                    {
                        return Ok(false);
                    }
                    Ok(authorize(&tx, &bound, &plugin).is_ok())
                })
                .await?
            {
                continue;
            }
            definitions.push(json!({"type":"function","function":{"name":spec.name,"description":spec.description,"parameters":spec.parameters}}));
        }
        Ok(definitions)
    }

    pub async fn execute(&self, ctx: &ToolContext, name: &str, args: Value) -> Result<ToolOutcome> {
        ensure!(!ctx.cancel.is_cancelled(), "turn cancelled");
        ensure!(self.allowed.contains(name), "tool is not allowed");
        let tool = self.registry.get(name)?;
        let spec = tool.spec();
        if spec.effect != Effect::NetworkRead {
            require_transaction_tool(&spec)?;
        }
        ensure!(
            tool.permits(&ctx.scope, ctx.actor_id),
            "tool is not permitted in this context"
        );
        let epoch = ctx
            .provider_epochs
            .get(spec.plugin)
            .context("tool plugin is disabled")?;
        let bound = BoundContext {
            scope: ctx.scope.clone(),
            actor_id: ctx.actor_id,
            guard: Some(ctx.guard.clone()),
            ai_epoch: Some(ctx.ai_epoch),
            provider_epoch: *epoch,
            access_revision: ctx.access_revision,
        };
        let normalized = tool.prepare(
            args,
            &ctx.scope,
            ctx.actor_id,
            ctx.received_at,
            self.timezone,
        )?;
        let operation = hash(&format!(
            "{}|{}|{}|{}|{}",
            ctx.turn_id,
            ctx.scope.key(),
            ctx.actor_id,
            spec.name,
            canonical(&normalized)?
        ));
        if spec.effect == Effect::NetworkRead {
            let revision = ctx
                .plugin_revisions
                .get(spec.plugin)
                .copied()
                .ok_or(ExternalInvalidated)?;
            self.run_external(
                tool,
                normalized,
                bound,
                operation,
                ctx.turn_id.clone(),
                Some(revision),
                None,
                ctx.cancel.clone(),
            )
            .await
        } else {
            self.run(tool, normalized, bound, operation, true, ctx.cancel.clone())
                .await
        }
    }

    /// Explicit host-parsed user commands use exactly the same local business code.
    pub async fn direct(
        &self,
        scope: Scope,
        actor: i64,
        name: &str,
        args: Value,
        received_at: i64,
    ) -> Result<ToolOutcome> {
        self.direct_bound(scope, actor, name, args, received_at, None)
            .await
    }

    /// Binds commands to the ingress permission revision, including an off/on cycle.
    pub async fn direct_message(
        &self,
        message: &crate::types::Incoming,
        name: &str,
        args: Value,
    ) -> Result<ToolOutcome> {
        self.direct_bound(
            message.key.scope.clone(),
            message.sender_id,
            name,
            args,
            message.received_at,
            Some(message.key.key()),
        )
        .await
    }

    async fn direct_bound(
        &self,
        scope: Scope,
        actor: i64,
        name: &str,
        args: Value,
        received_at: i64,
        source_key: Option<String>,
    ) -> Result<ToolOutcome> {
        let tool = self.registry.get(name)?;
        let spec = tool.spec();
        if spec.effect != Effect::NetworkRead {
            require_transaction_tool(&spec)?;
        }
        let key = scope.key();
        let external_source = source_key.clone();
        let access_revision = self
            .store
            .call(move |db| match source_key {
                Some(source) => crate::access::message_revision(db, &key, &source),
                None => crate::access::require_enabled(db, &key),
            })
            .await?;
        ensure!(
            tool.permits(&scope, actor),
            "tool is not permitted in this context"
        );
        let epoch = *self
            .epochs()?
            .get(spec.plugin)
            .context("tool plugin is disabled in this runtime")?;
        let normalized = tool.prepare(args, &scope, actor, received_at, self.timezone)?;
        let bound = BoundContext {
            scope,
            actor_id: actor,
            guard: None,
            ai_epoch: None,
            provider_epoch: epoch,
            access_revision,
        };
        if spec.effect == Effect::NetworkRead {
            let turn = external_source
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let operation = hash(&format!(
                "{}|{}|{}|{}|{}",
                turn,
                bound.scope.key(),
                actor,
                spec.name,
                canonical(&normalized)?
            ));
            return self
                .run_external(
                    tool,
                    normalized,
                    bound,
                    operation,
                    turn,
                    None,
                    external_source,
                    self.cancel.clone(),
                )
                .await;
        }
        self.run(
            tool,
            normalized,
            bound,
            uuid::Uuid::new_v4().to_string(),
            false,
            CancellationToken::new(),
        )
        .await
    }

    async fn run(
        &self,
        tool: Arc<dyn Tool>,
        args: Value,
        bound: BoundContext,
        operation: String,
        confirm: bool,
        cancel: CancellationToken,
    ) -> Result<ToolOutcome> {
        let spec = tool.spec();
        require_transaction_tool(&spec)?;
        check_runtime(self.epochs()?, &bound, spec.plugin)?;
        let args_json = canonical(&args)?;
        let args_hash = hash(&args_json);
        let schema_hash = hash(&canonical(&spec.parameters)?);
        let context_json = serde_json::to_string(&bound)?;
        let preview = tool.preview(&args, self.timezone)?;
        self.store.call(move |conn| {
            ensure!(!cancel.is_cancelled(), "turn cancelled");
            ensure!(tool.permits(&bound.scope,bound.actor_id),"tool is not permitted in this context");
            let tx = conn.transaction()?;
            authorize(&tx, &bound, spec.plugin)?;
            if let Some((status, token, expires, result, receipt)) = tx.query_row(
                "SELECT status,token,expires_at,result_json,receipt FROM tool_runs WHERE operation_id=?1", [&operation],
                |r| Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<i64>>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?))
            ).optional()? {
                return match status.as_str() {
                    "succeeded" => Ok(ToolOutcome::Completed { operation_id: operation, result: serde_json::from_str(&result.context("missing result")?)?, receipt: receipt.unwrap_or_default() }),
                    "pending_confirmation" if expires.unwrap_or(0) > now() => {
                        let token=token.context("missing token")?;
                        let expires=expires.unwrap();
                        Ok(ToolOutcome::Pending { operation_id: operation, preview:confirmation_preview(&preview,&token,expires), token, expires_at: expires })
                    },
                    _ => bail!("operation already finished or expired ({status})"),
                };
            }
            let time = now();
            if confirm && spec.effect == Effect::LocalWrite && tool.requires_confirmation() {
                tx.execute("UPDATE tool_runs SET status='expired' WHERE status='pending_confirmation' AND expires_at<=?1",[time])?;
                let pending:i64=tx.query_row("SELECT count(*) FROM tool_runs WHERE status='pending_confirmation'",[],|r|r.get(0))?;
                ensure!(pending<128,"too many pending confirmations; retry after they expire");
                let own:i64=tx.query_row("SELECT count(*) FROM tool_runs WHERE status='pending_confirmation' AND scope_key=?1 AND actor_id=?2",params![bound.scope.key(),bound.actor_id],|r|r.get(0))?;
                ensure!(own<8,"你在当前会话最多可以有 8 项待确认操作");
                let token = uuid::Uuid::new_v4().simple().to_string();
                let expires = time + 300;
                tx.execute("INSERT INTO tool_runs(operation_id,tool_name,tool_version,plugin_id,context_json,args_json,args_hash,schema_hash,status,token,expires_at,created_at,scope_key,actor_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'pending_confirmation',?9,?10,?11,?12,?13)", params![operation,spec.name,spec.version,spec.plugin,context_json,args_json,args_hash,schema_hash,token,expires,time,bound.scope.key(),bound.actor_id])?;
                tx.commit()?;
                return Ok(ToolOutcome::Pending { operation_id: operation, preview:confirmation_preview(&preview,&token,expires), token, expires_at: expires });
            }
            tx.execute("INSERT INTO tool_runs(operation_id,tool_name,tool_version,plugin_id,context_json,args_json,args_hash,schema_hash,status,created_at,scope_key,actor_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'executing',?9,?10,?11)", params![operation,spec.name,spec.version,spec.plugin,context_json,args_json,args_hash,schema_hash,time,bound.scope.key(),bound.actor_id])?;
            apply_and_record(tx,tool.as_ref(),&args,&bound,operation,&cancel)
        }).await
    }

    pub async fn confirm(&self, scope: &Scope, actor: i64, token: &str) -> Result<ToolOutcome> {
        let scope = scope.clone();
        let token = token.to_owned();
        let registry = self.registry.clone();
        let allowed = self.allowed.clone();
        let runtime_epochs = self.epochs()?.clone();
        self.store.call(move |conn| {
            let tx = conn.transaction()?;
            let row = tx.query_row("SELECT operation_id,tool_name,tool_version,plugin_id,context_json,args_json,args_hash,status,expires_at,result_json,receipt,schema_hash FROM tool_runs WHERE token=?1", [&token], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?,r.get::<_,i64>(8)?,r.get::<_,Option<String>>(9)?,r.get::<_,Option<String>>(10)?,r.get::<_,String>(11)?))).optional()?.context("confirmation token not found")?;
            let (operation,name,version,plugin,context_json,args_json,args_hash,status,expires,result_json,receipt,schema_hash) = row;
            let bound: BoundContext = serde_json::from_str(&context_json)?;
            ensure!(bound.scope == scope && bound.actor_id == actor, "confirmation belongs to another user or chat");
            check_runtime(&runtime_epochs,&bound,&plugin)?;
            ensure!(allowed.contains(&name), "tool is no longer allowed");
            let tool = registry.get(&name)?;
            let spec = tool.spec();
            require_transaction_tool(&spec)?;
            ensure!(tool.permits(&scope,actor),"tool is not permitted in this context");
            ensure!(spec.version == version && spec.plugin == plugin, "tool changed; request again");
            ensure!(schema_hash == hash(&canonical(&spec.parameters)?),"tool schema changed; request again");
            ensure!(hash(&args_json) == args_hash, "confirmation arguments changed");
            authorize(&tx, &bound, &plugin)?;
            if status == "succeeded" {
                return Ok(ToolOutcome::Completed { operation_id: operation, result: serde_json::from_str(&result_json.context("missing result")?)?, receipt: receipt.unwrap_or_default() });
            }
            ensure!(status == "pending_confirmation", "confirmation already consumed ({status})");
            ensure!(expires > now(), "confirmation expired");
            tx.execute("UPDATE tool_runs SET status='executing' WHERE operation_id=?1 AND status='pending_confirmation'", [&operation])?;
            let args: Value = serde_json::from_str(&args_json)?;
            apply_and_record(tx,tool.as_ref(),&args,&bound,operation,&CancellationToken::new())
        }).await
    }

    pub async fn reject(&self, scope: &Scope, actor: i64, token: &str) -> Result<()> {
        let scope = scope.clone();
        let token = token.to_owned();
        self.store.call(move |conn| {
            let tx=conn.transaction()?;
            crate::access::require_enabled(&tx,&scope.key())?;
            let context=tx.query_row("SELECT context_json FROM tool_runs WHERE token=?1", [&token], |r|r.get::<_,String>(0)).optional()?.context("confirmation token not found")?;
            let bound:BoundContext=serde_json::from_str(&context)?;
            ensure!(bound.scope==scope && bound.actor_id==actor,"confirmation belongs to another user or chat");
            ensure!(tx.execute("UPDATE tool_runs SET status='rejected' WHERE token=?1 AND status='pending_confirmation'",[token])?==1,"confirmation already consumed");
            tx.commit()?; Ok(())
        }).await
    }
}

/// JSON Schema is advisory to the model; these validators enforce it locally.
pub fn object(args: &Value, keys: &[&str]) -> Result<()> {
    let map = args.as_object().context("arguments must be an object")?;
    ensure!(
        map.len() == keys.len() && keys.iter().all(|k| map.contains_key(*k)),
        "arguments contain missing or unknown fields"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        plugins::{clock, reminder},
        types::Scene,
    };

    async fn setup() -> (Store, Gateway, ToolContext) {
        setup_with_confirmation(true).await
    }

    async fn setup_with_confirmation(confirm_reminders: bool) -> (Store, Gateway, ToolContext) {
        let store = Store::open(":memory:", 32).await.unwrap();
        store
            .activate_plugins(vec!["ai".into(), "clock".into(), "reminder".into()])
            .await
            .unwrap();
        store.call(|conn|{conn.execute("INSERT INTO ai_conversations(id,scope,mode,epoch) VALUES('conversation','1:group:2','group',1)",[])?;Ok(())}).await.unwrap();
        let mut registry = ToolRegistry::new();
        register_reply(&mut registry).unwrap();
        clock::register(&mut registry).unwrap();
        if confirm_reminders {
            reminder::register_with_confirmation(&mut registry, 10, true).unwrap();
        } else {
            reminder::register(&mut registry, 10).unwrap();
        }
        reminder::migrate(&store).await.unwrap();
        let names = registry.names().map(str::to_owned).collect();
        let gateway = Gateway::new(store.clone(), "Asia/Shanghai", registry, names).unwrap();
        gateway.migrate().await.unwrap();
        let ctx = gateway
            .context(
                Scope {
                    bot_id: 1,
                    scene: Scene::Group,
                    peer_id: 2,
                },
                3,
                ContextGuard {
                    conversation_id: "conversation".into(),
                    epoch: 1,
                },
                1,
                "turn-1".into(),
                now(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        (store, gateway, ctx)
    }
    fn pending(outcome: ToolOutcome) -> (String, String) {
        match outcome {
            ToolOutcome::Pending {
                operation_id,
                token,
                ..
            } => (operation_id, token),
            _ => panic!("expected confirmation"),
        }
    }

    #[tokio::test]
    async fn final_reply_is_advertised_but_never_runs_as_a_transaction() {
        let (store, gateway, ctx) = setup().await;
        let definitions = gateway.definitions(&ctx).await.unwrap();
        let reply = definitions
            .iter()
            .find(|definition| definition["function"]["name"] == "reply_message")
            .unwrap();
        assert_eq!(
            reply["function"]["parameters"]["additionalProperties"],
            false
        );
        assert_eq!(reply["function"]["parameters"]["required"], json!(["text"]));
        assert_eq!(
            reply["function"]["parameters"]["properties"]["mention_users"]["maxItems"],
            3
        );
        assert_eq!(
            reply["function"]["parameters"]["properties"]["sticker_ref"]["type"],
            json!(["string", "null"])
        );
        let args = json!({"reply_to":null,"mention_users":[],"text":"reply"});
        let error = gateway
            .execute(&ctx, "reply_message", args.clone())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("chat dispatcher"));
        let error = gateway
            .direct(
                ctx.scope.clone(),
                ctx.actor_id,
                "reply_message",
                args.clone(),
                now(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("chat dispatcher"));
        let bound = BoundContext {
            scope: ctx.scope.clone(),
            actor_id: ctx.actor_id,
            guard: Some(ctx.guard.clone()),
            ai_epoch: Some(ctx.ai_epoch),
            provider_epoch: ctx.provider_epochs["ai"],
            access_revision: ctx.access_revision,
        };
        let error = gateway
            .run(
                Arc::new(ReplyTool),
                args,
                bound.clone(),
                "must-not-exist".into(),
                true,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("chat dispatcher"));
        let context_json = serde_json::to_string(&bound).unwrap();
        store.call(move |db| {
            let count: i64 = db.query_row("SELECT count(*) FROM tool_runs", [], |row| row.get(0))?;
            assert_eq!(count, 0);
            // A stale or forged confirmation row must not turn a final reply
            // into a transactional operation, including successful replays.
            db.execute("INSERT INTO tool_runs(operation_id,tool_name,tool_version,plugin_id,context_json,args_json,scope_key,actor_id,args_hash,schema_hash,status,token,expires_at,result_json,receipt,created_at) VALUES('legacy-reply','reply_message',1,'ai',?1,'{}',?2,?3,'unused','unused','succeeded','reply-token',?4,'{}','reply',?5)", params![context_json,bound.scope.key(),bound.actor_id,now()+300,now()])?;
            Ok(())
        }).await.unwrap();
        let error = gateway
            .confirm(&ctx.scope, ctx.actor_id, "reply-token")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("chat dispatcher"));
        let only_clock = Gateway::new(
            store.clone(),
            "UTC",
            gateway.registry.clone(),
            vec!["clock_now".into()],
        )
        .unwrap();
        only_clock.migrate().await.unwrap();
        assert!(
            only_clock
                .definitions(&ctx)
                .await
                .unwrap()
                .iter()
                .all(|definition| definition["function"]["name"] != "reply_message")
        );
        ctx.cancel.cancel();
        assert!(gateway.definitions(&ctx).await.unwrap().is_empty());
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn asynchronous_sticker_search_is_advertised_but_rejected_by_every_transaction_entry() {
        let (store, gateway, ctx) = setup().await;
        let definitions = gateway.definitions(&ctx).await.unwrap();
        let search = definitions
            .iter()
            .find(|d| d["function"]["name"] == "search_stickers")
            .unwrap();
        assert_eq!(
            search["function"]["parameters"]["properties"]["query"]["maxLength"],
            256
        );
        assert_eq!(
            search["function"]["parameters"]["additionalProperties"],
            false
        );
        assert_eq!(SearchStickersTool.spec().effect, Effect::AsyncRead);
        let args = json!({"query":"开心庆祝"});
        assert!(
            gateway
                .execute(&ctx, "search_stickers", args.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("chat dispatcher")
        );
        assert!(
            gateway
                .direct(
                    ctx.scope.clone(),
                    ctx.actor_id,
                    "search_stickers",
                    args.clone(),
                    now()
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("chat dispatcher")
        );
        let bound = BoundContext {
            scope: ctx.scope.clone(),
            actor_id: ctx.actor_id,
            guard: Some(ctx.guard.clone()),
            ai_epoch: Some(ctx.ai_epoch),
            provider_epoch: ctx.provider_epochs["ai"],
            access_revision: ctx.access_revision,
        };
        assert!(
            gateway
                .run(
                    Arc::new(SearchStickersTool),
                    args,
                    bound.clone(),
                    "async-must-not-exist".into(),
                    true,
                    CancellationToken::new()
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("chat dispatcher")
        );
        let context = serde_json::to_string(&bound).unwrap();
        store.call(move |db| {
            assert_eq!(db.query_row("SELECT COUNT(*) FROM tool_runs",[],|r|r.get::<_,i64>(0))?,0);
            db.execute("INSERT INTO tool_runs(operation_id,tool_name,tool_version,plugin_id,context_json,args_json,scope_key,actor_id,args_hash,schema_hash,status,token,expires_at,result_json,receipt,created_at) VALUES('legacy-async','search_stickers',1,'ai',?1,'{}',?2,?3,'unused','unused','succeeded','async-token',?4,'{}','unused',?5)",params![context,bound.scope.key(),bound.actor_id,now()+300,now()])?;
            Ok(())
        }).await.unwrap();
        assert!(
            gateway
                .confirm(&ctx.scope, ctx.actor_id, "async-token")
                .await
                .unwrap_err()
                .to_string()
                .contains("chat dispatcher")
        );
        let mut private = ctx.scope.clone();
        private.scene = Scene::Friend;
        assert!(!SearchStickersTool.permits(&private, ctx.actor_id));
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn displaying_catalog_does_not_grant_tools_or_enable_their_plugins() {
        let (store, gateway, ctx) = setup().await;
        let catalog = gateway.registry.catalog();
        for entry in &catalog {
            let spec = gateway.registry.get(entry.name).unwrap().spec();
            assert_eq!(entry.description, spec.description);
            assert_eq!(entry.plugin_id, spec.plugin);
            assert_eq!(entry.effect, spec.effect);
        }
        let limited = Gateway::new(
            store.clone(),
            "UTC",
            gateway.registry.clone(),
            vec!["clock_now".into()],
        )
        .unwrap();
        limited.migrate().await.unwrap();
        let definitions = limited.definitions(&ctx).await.unwrap();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0]["function"]["name"], "clock_now");
        assert!(
            limited
                .execute(&ctx, "reminder_list", json!({}))
                .await
                .is_err()
        );
        store
            .call(|db| {
                db.execute("UPDATE plugin_states SET enabled=0 WHERE id='clock'", [])?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(limited.definitions(&ctx).await.unwrap().is_empty());
        assert!(limited.execute(&ctx, "clock_now", json!({})).await.is_err());
        assert_eq!(limited.registry.catalog(), catalog);
        let none = Gateway::new(store.clone(), "UTC", gateway.registry.clone(), vec![]).unwrap();
        none.migrate().await.unwrap();
        assert!(none.definitions(&ctx).await.unwrap().is_empty());
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn off_on_cycle_cancels_confirmations_and_does_not_replay_accepted_commands() {
        use crate::{
            access::AccessStore,
            types::{Incoming, MessageKey},
        };
        let (store, gateway, ctx) = setup().await;
        let (_, token) = pending(
            gateway
                .execute(
                    &ctx,
                    "reminder_create",
                    json!({"after_seconds":60,"text":"旧待确认"}),
                )
                .await
                .unwrap(),
        );
        let old = Incoming {
            key: MessageKey {
                scope: ctx.scope.clone(),
                message_seq: 8,
            },
            sender_id: ctx.actor_id,
            sender_nickname: None,
            sender_card: None,
            text: "/remind add 60 旧指令".into(),
            mentions: vec![],
            quote_seq: None,
            received_at: now(),
            raw_message: serde_json::Value::Null,
            notification: None,
        };
        let incoming = old.clone();
        store.call(move|db|{db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at) VALUES(?1,?2,'conversation',1,?3,'control','',?4)",params![incoming.key.key(),incoming.key.scope.key(),incoming.sender_id,incoming.received_at])?;Ok(())}).await.unwrap();
        let access = AccessStore::new(store.clone());
        access.set_enabled(ctx.scope.clone(), false).await.unwrap();
        assert!(
            gateway
                .direct(
                    ctx.scope.clone(),
                    ctx.actor_id,
                    "clock_now",
                    json!({}),
                    now()
                )
                .await
                .is_err()
        );
        assert!(gateway.definitions(&ctx).await.unwrap().is_empty());
        access.set_enabled(ctx.scope.clone(), true).await.unwrap();
        assert!(
            gateway
                .confirm(&ctx.scope, ctx.actor_id, &token)
                .await
                .is_err()
        );
        assert!(gateway.execute(&ctx, "clock_now", json!({})).await.is_err());
        assert!(
            gateway
                .direct_message(
                    &old,
                    "reminder_create",
                    json!({"after_seconds":60,"text":"旧指令"})
                )
                .await
                .is_err()
        );
        assert!(matches!(
            gateway
                .direct(
                    ctx.scope.clone(),
                    ctx.actor_id,
                    "clock_now",
                    json!({}),
                    now()
                )
                .await
                .unwrap(),
            ToolOutcome::Completed { .. }
        ));
        store
            .call(|db| {
                assert_eq!(
                    db.query_row("SELECT count(*) FROM reminder_jobs", [], |r| r
                        .get::<_, i64>(0))?,
                    0
                );
                assert_eq!(
                    db.query_row(
                        "SELECT status FROM tool_runs WHERE token IS NOT NULL",
                        [],
                        |r| r.get::<_, String>(0)
                    )?,
                    "rejected"
                );
                Ok(())
            })
            .await
            .unwrap();
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn confirmation_is_scoped_owner_bound_and_idempotent() {
        let (store, gateway, ctx) = setup().await;
        let args = json!({"after_seconds":3600,"text":"喝水"});
        let (op, token) = pending(
            gateway
                .execute(&ctx, "reminder_create", args.clone())
                .await
                .unwrap(),
        );
        let repeated = pending(
            gateway
                .execute(&ctx, "reminder_create", args)
                .await
                .unwrap(),
        );
        assert_eq!((op.clone(), token.clone()), repeated);
        assert!(gateway.confirm(&ctx.scope, 99, &token).await.is_err());
        let mut other = ctx.scope.clone();
        other.peer_id = 99;
        assert!(gateway.confirm(&other, ctx.actor_id, &token).await.is_err());
        let first = gateway
            .confirm(&ctx.scope, ctx.actor_id, &token)
            .await
            .unwrap();
        let second = gateway
            .confirm(&ctx.scope, ctx.actor_id, &token)
            .await
            .unwrap();
        match (first, second) {
            (
                ToolOutcome::Completed { result: a, .. },
                ToolOutcome::Completed { result: b, .. },
            ) => assert_eq!(a, b),
            _ => panic!("expected completed"),
        }
        let count = store
            .call(|conn| {
                Ok(
                    conn.query_row("SELECT count(*) FROM reminder_jobs", [], |r| {
                        r.get::<_, i64>(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert_eq!(count, 1);
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn ai_reminders_execute_without_confirmation_and_keep_owner_scope_and_idempotency() {
        let (store, gateway, ctx) = setup_with_confirmation(false).await;
        let args = json!({"after_seconds":1200,"text":"吃饭"});
        let ToolOutcome::Completed {
            operation_id,
            result,
            receipt,
        } = gateway
            .execute(&ctx, "reminder_create", args.clone())
            .await
            .unwrap()
        else {
            panic!("AI reminders should execute without a second confirmation");
        };
        assert_eq!(result["due_at"], ctx.received_at + 1200);
        assert_eq!(result["text"], "吃饭");
        assert!(!receipt.contains("/bot confirm"));
        let id = result["id"].as_str().unwrap().to_owned();
        let ToolOutcome::Completed {
            operation_id: repeated_operation,
            result: repeated_result,
            ..
        } = gateway
            .execute(&ctx, "reminder_create", args)
            .await
            .unwrap()
        else {
            panic!("repeated execution should return the committed receipt");
        };
        assert_eq!(operation_id, repeated_operation);
        assert_eq!(result, repeated_result);

        let mut other_actor = ctx.clone();
        other_actor.actor_id = 99;
        let mut other_chat = ctx.clone();
        other_chat.scope.peer_id = 99;
        for denied in [&other_actor, &other_chat] {
            let ToolOutcome::Completed { result, .. } = gateway
                .execute(denied, "reminder_list", json!({}))
                .await
                .unwrap()
            else {
                panic!("reads should execute immediately");
            };
            assert_eq!(result["reminders"], json!([]));
            assert!(
                gateway
                    .execute(denied, "reminder_cancel", json!({"reminder_id":id}))
                    .await
                    .is_err()
            );
        }
        for _ in 0..2 {
            let ToolOutcome::Completed {
                result, receipt, ..
            } = gateway
                .execute(&ctx, "reminder_cancel", json!({"reminder_id":id}))
                .await
                .unwrap()
            else {
                panic!("AI cancellation should execute without a second confirmation");
            };
            assert_eq!(result["status"], "cancelled");
            assert!(!receipt.contains("/bot confirm"));
        }
        store
            .call(move |db| {
                assert_eq!(
                    db.query_row("SELECT count(*) FROM reminder_jobs", [], |r| r
                        .get::<_, i64>(0))?,
                    1
                );
                let (owner, scope, status): (i64, String, String) = db.query_row(
                    "SELECT actor_id,scope_key,status FROM reminder_jobs WHERE id=?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                assert_eq!(owner, ctx.actor_id);
                assert_eq!(scope, ctx.scope.key());
                assert_eq!(status, "cancelled");
                assert_eq!(
                    db.query_row(
                        "SELECT count(*) FROM tool_runs WHERE status='pending_confirmation' OR token IS NOT NULL",
                        [],
                        |r| r.get::<_, i64>(0),
                    )?,
                    0
                );
                Ok(())
            })
            .await
            .unwrap();
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn immediate_ai_reminders_still_reject_stale_or_disabled_contexts() {
        for change in [
            "UPDATE ai_conversations SET epoch=epoch+1",
            "UPDATE plugin_states SET epoch=epoch+1 WHERE id='reminder'",
            "UPDATE plugin_states SET enabled=0 WHERE id='reminder'",
            "UPDATE plugin_states SET enabled=0 WHERE id='ai'",
        ] {
            let (store, gateway, ctx) = setup_with_confirmation(false).await;
            let ToolOutcome::Completed { result, .. } = gateway
                .execute(
                    &ctx,
                    "reminder_create",
                    json!({"after_seconds":1200,"text":"吃饭"}),
                )
                .await
                .unwrap()
            else {
                panic!("expected immediate reminder creation");
            };
            store
                .call(move |db| {
                    db.execute(change, [])?;
                    Ok(())
                })
                .await
                .unwrap();
            for (name, args) in [
                (
                    "reminder_create",
                    json!({"after_seconds":1800,"text":"喝水"}),
                ),
                ("reminder_cancel", json!({"reminder_id":result["id"]})),
            ] {
                assert!(gateway.execute(&ctx, name, args).await.is_err(), "{change}");
            }
            store
                .call(|db| {
                    assert_eq!(
                        db.query_row("SELECT count(*) FROM reminder_jobs", [], |r| r
                            .get::<_, i64>(0))?,
                        1
                    );
                    assert_eq!(
                        db.query_row("SELECT status FROM reminder_jobs", [], |r| r
                            .get::<_, String>(0))?,
                        "pending"
                    );
                    Ok(())
                })
                .await
                .unwrap();
            store.stop().await.unwrap();
        }
    }

    #[tokio::test]
    async fn immediate_ai_reminders_still_require_allowed_tools_and_active_turns() {
        let (store, gateway, ctx) = setup_with_confirmation(false).await;
        let limited = Gateway::new(store.clone(), "UTC", gateway.registry.clone(), vec![]).unwrap();
        limited.migrate().await.unwrap();
        assert!(
            limited
                .execute(
                    &ctx,
                    "reminder_create",
                    json!({"after_seconds":1200,"text":"吃饭"}),
                )
                .await
                .is_err()
        );
        ctx.cancel.cancel();
        assert!(
            gateway
                .execute(
                    &ctx,
                    "reminder_create",
                    json!({"after_seconds":1200,"text":"吃饭"}),
                )
                .await
                .is_err()
        );
        assert_eq!(
            store
                .call(
                    |db| Ok(db.query_row("SELECT count(*) FROM reminder_jobs", [], |r| r
                        .get::<_, i64>(0))?)
                )
                .await
                .unwrap(),
            0
        );
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn stale_context_plugin_and_expired_confirmation_cannot_write() {
        for change in [
            "UPDATE ai_conversations SET epoch=epoch+1",
            "UPDATE plugin_states SET epoch=epoch+1 WHERE id='reminder'",
            "UPDATE plugin_states SET enabled=0 WHERE id='ai'",
            "UPDATE tool_runs SET expires_at=0",
        ] {
            let (store, gateway, ctx) = setup().await;
            let (_, token) = pending(
                gateway
                    .execute(
                        &ctx,
                        "reminder_create",
                        json!({"after_seconds":3600,"text":"x"}),
                    )
                    .await
                    .unwrap(),
            );
            store
                .call(move |conn| {
                    conn.execute(change, [])?;
                    Ok(())
                })
                .await
                .unwrap();
            assert!(
                gateway
                    .confirm(&ctx.scope, ctx.actor_id, &token)
                    .await
                    .is_err(),
                "{change}"
            );
            assert_eq!(
                store
                    .call(|conn| Ok(conn.query_row(
                        "SELECT count(*) FROM reminder_jobs",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?))
                    .await
                    .unwrap(),
                0
            );
            store.stop().await.unwrap();
        }
    }

    #[tokio::test]
    async fn validates_arguments_and_anchors_due_time_to_event() {
        let (store, gateway, ctx) = setup().await;
        for args in [
            json!({"after_seconds":59,"text":"x"}),
            json!({"after_seconds":60.5,"text":"x"}),
            json!({"after_seconds":60,"text":" "}),
            json!({"after_seconds":60,"text":"x","actor_id":99}),
        ] {
            assert!(
                gateway
                    .execute(&ctx, "reminder_create", args)
                    .await
                    .is_err()
            );
        }
        let (_, token) = pending(
            gateway
                .execute(
                    &ctx,
                    "reminder_create",
                    json!({"after_seconds":3600,"text":"x"}),
                )
                .await
                .unwrap(),
        );
        match gateway
            .confirm(&ctx.scope, ctx.actor_id, &token)
            .await
            .unwrap()
        {
            ToolOutcome::Completed { result, .. } => {
                assert_eq!(result["due_at"], ctx.received_at + 3600)
            }
            _ => panic!("expected completed"),
        }
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_reminders_checks_owner_and_chat() {
        let (store, gateway, ctx) = setup().await;
        let id = match gateway
            .direct(
                ctx.scope.clone(),
                ctx.actor_id,
                "reminder_create",
                json!({"after_seconds":3600,"text":"x"}),
                ctx.received_at,
            )
            .await
            .unwrap()
        {
            ToolOutcome::Completed { result, .. } => result["id"].as_str().unwrap().to_owned(),
            _ => panic!("direct is already approved"),
        };
        assert!(
            gateway
                .direct(
                    ctx.scope.clone(),
                    99,
                    "reminder_cancel",
                    json!({"reminder_id":id}),
                    ctx.received_at
                )
                .await
                .is_err()
        );
        let mut other = ctx.scope.clone();
        other.peer_id = 99;
        assert!(
            gateway
                .direct(
                    other,
                    ctx.actor_id,
                    "reminder_cancel",
                    json!({"reminder_id":id}),
                    ctx.received_at
                )
                .await
                .is_err()
        );
        gateway
            .direct(
                ctx.scope,
                ctx.actor_id,
                "reminder_cancel",
                json!({"reminder_id":id}),
                ctx.received_at,
            )
            .await
            .unwrap();
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn committed_reminders_survive_context_reset_and_delivery_is_not_retried() {
        let (store, gateway, ctx) = setup().await;
        let bot_id = ctx.scope.bot_id;
        gateway
            .direct(
                ctx.scope,
                ctx.actor_id,
                "reminder_create",
                json!({"after_seconds":3600,"text":"x"}),
                ctx.received_at,
            )
            .await
            .unwrap();
        store
            .call(|conn| {
                conn.execute("UPDATE ai_conversations SET epoch=epoch+1", [])?;
                conn.execute("UPDATE reminder_jobs SET due_at=0", [])?;
                Ok(())
            })
            .await
            .unwrap();
        let due = reminder::claim_due(&store, bot_id).await.unwrap().unwrap();
        assert!(reminder::claim_due(&store, bot_id).await.unwrap().is_none());
        store.call(move|db|{
            db.execute("INSERT INTO outbox(id,scope,origin_plugin,plugin_epoch,body,state,created_at,updated_at) VALUES(?1,?2,'reminder',?3,'x','submitted',0,0)",params![due.outbox_id,due.scope.key(),due.plugin_epoch])?;
            Ok(())
        }).await.unwrap();
        assert_eq!(
            reminder::recover_interrupted(&store, bot_id).await.unwrap(),
            1
        );
        assert!(reminder::claim_due(&store, bot_id).await.unwrap().is_none());
        store.stop().await.unwrap();
    }

    struct PartialFailure;
    impl Tool for PartialFailure {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "fail",
                plugin: "reminder",
                version: 1,
                description: "test",
                parameters: json!({}),
                effect: Effect::LocalWrite,
                max_output_bytes: 100,
            }
        }
        fn prepare(&self, args: Value, _: &Scope, _: i64, _: i64, _: Tz) -> Result<Value> {
            Ok(args)
        }
        fn preview(&self, _: &Value, _: Tz) -> Result<String> {
            Ok("test".into())
        }
        fn apply(
            &self,
            tx: &Transaction<'_>,
            _: &Value,
            _: &Scope,
            _: i64,
            _: &str,
            _: i64,
        ) -> Result<ToolResult> {
            tx.execute("INSERT INTO test_effect VALUES(1)", [])?;
            bail!("failure after mutation")
        }
    }
    #[tokio::test]
    async fn business_write_rolls_back_but_failure_receipt_persists() {
        let (store, _, ctx) = setup().await;
        store
            .call(|conn| {
                conn.execute("CREATE TABLE test_effect(id INTEGER)", [])?;
                Ok(())
            })
            .await
            .unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(PartialFailure)).unwrap();
        let gateway = Gateway::new(store.clone(), "UTC", registry, vec!["fail".into()]).unwrap();
        gateway.migrate().await.unwrap();
        let (_, token) = pending(gateway.execute(&ctx, "fail", json!({})).await.unwrap());
        assert!(
            gateway
                .confirm(&ctx.scope, ctx.actor_id, &token)
                .await
                .is_err()
        );
        let (result, state) = store
            .call(|conn| {
                Ok((
                    conn.query_row("SELECT count(*) FROM test_effect", [], |r| {
                        r.get::<_, i64>(0)
                    })?,
                    conn.query_row(
                        "SELECT status FROM tool_runs WHERE tool_name='fail'",
                        [],
                        |r| r.get::<_, String>(0),
                    )?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(result, 0);
        assert_eq!(state, "failed");
        assert!(
            gateway
                .confirm(&ctx.scope, ctx.actor_id, &token)
                .await
                .is_err()
        );
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn pending_confirmations_are_bounded_per_user_and_rejection_frees_capacity() {
        let (store, gateway, ctx) = setup().await;
        let mut first = String::new();
        for n in 0..8 {
            let (_, token) = pending(
                gateway
                    .execute(
                        &ctx,
                        "reminder_create",
                        json!({"after_seconds":3600,"text":format!("reminder {n}")}),
                    )
                    .await
                    .unwrap(),
            );
            if n == 0 {
                first = token;
            }
        }
        let ninth = json!({"after_seconds":3600,"text":"ninth"});
        assert!(
            gateway
                .execute(&ctx, "reminder_create", ninth.clone())
                .await
                .is_err()
        );
        assert!(gateway.reject(&ctx.scope, 99, &first).await.is_err());
        gateway
            .reject(&ctx.scope, ctx.actor_id, &first)
            .await
            .unwrap();
        assert!(
            gateway
                .execute(&ctx, "reminder_create", ninth)
                .await
                .is_ok()
        );
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn disabled_ai_hides_tools_but_explicit_plugin_command_still_works() {
        let (store, gateway, ctx) = setup().await;
        assert!(!gateway.definitions(&ctx).await.unwrap().is_empty());
        store
            .call(|conn| {
                conn.execute(
                    "UPDATE plugin_states SET enabled=0,epoch=epoch+1 WHERE id='ai'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(gateway.definitions(&ctx).await.unwrap().is_empty());
        assert!(gateway.execute(&ctx, "clock_now", json!({})).await.is_err());
        assert!(
            gateway
                .direct(ctx.scope, ctx.actor_id, "clock_now", json!({}), now())
                .await
                .is_ok()
        );
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn altered_confirmation_arguments_or_schema_cannot_commit() {
        for sql in [
            "UPDATE tool_runs SET args_json='{}'",
            "UPDATE tool_runs SET schema_hash='modified'",
            "UPDATE tool_runs SET tool_version=2",
        ] {
            let (store, gateway, ctx) = setup().await;
            let (_, token) = pending(
                gateway
                    .execute(
                        &ctx,
                        "reminder_create",
                        json!({"after_seconds":3600,"text":"x"}),
                    )
                    .await
                    .unwrap(),
            );
            store
                .call(move |conn| {
                    conn.execute(sql, [])?;
                    Ok(())
                })
                .await
                .unwrap();
            assert!(
                gateway
                    .confirm(&ctx.scope, ctx.actor_id, &token)
                    .await
                    .is_err()
            );
            assert_eq!(
                store
                    .call(|conn| Ok(conn.query_row(
                        "SELECT count(*) FROM reminder_jobs",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?))
                    .await
                    .unwrap(),
                0
            );
            store.stop().await.unwrap();
        }
    }

    struct ScopedTool(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Tool for ScopedTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "scoped",
                plugin: "reminder",
                version: 1,
                description: "test scope policy",
                parameters: json!({}),
                effect: Effect::LocalWrite,
                max_output_bytes: 100,
            }
        }
        fn permits(&self, _: &Scope, _: i64) -> bool {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn prepare(&self, args: Value, _: &Scope, _: i64, _: i64, _: Tz) -> Result<Value> {
            object(&args, &[])?;
            Ok(args)
        }
        fn preview(&self, _: &Value, _: Tz) -> Result<String> {
            Ok("test".into())
        }
        fn apply(
            &self,
            tx: &Transaction<'_>,
            _: &Value,
            _: &Scope,
            _: i64,
            _: &str,
            _: i64,
        ) -> Result<ToolResult> {
            tx.execute("INSERT INTO scoped_effect VALUES(1)", [])?;
            Ok(ToolResult {
                value: json!({"status":"done"}),
                receipt: "done".into(),
            })
        }
    }
    #[tokio::test]
    async fn tool_scope_policy_filters_definitions_and_rechecks_every_execution_path() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (store, _, ctx) = setup().await;
        store
            .call(|conn| {
                conn.execute("CREATE TABLE scoped_effect(id INTEGER)", [])?;
                Ok(())
            })
            .await
            .unwrap();
        let allowed = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry
            .register(Arc::new(ScopedTool(allowed.clone())))
            .unwrap();
        let gateway = Gateway::new(store.clone(), "UTC", registry, vec!["scoped".into()]).unwrap();
        gateway.migrate().await.unwrap();
        assert!(gateway.definitions(&ctx).await.unwrap().is_empty());
        assert!(gateway.execute(&ctx, "scoped", json!({})).await.is_err());
        assert!(
            gateway
                .direct(ctx.scope.clone(), ctx.actor_id, "scoped", json!({}), now())
                .await
                .is_err()
        );
        allowed.store(true, Ordering::SeqCst);
        assert_eq!(gateway.definitions(&ctx).await.unwrap().len(), 1);
        let (_, token) = pending(gateway.execute(&ctx, "scoped", json!({})).await.unwrap());
        allowed.store(false, Ordering::SeqCst);
        assert!(
            gateway
                .confirm(&ctx.scope, ctx.actor_id, &token)
                .await
                .is_err()
        );
        assert_eq!(
            store
                .call(|conn| Ok(
                    conn.query_row("SELECT count(*) FROM scoped_effect", [], |r| r
                        .get::<_, i64>(0))?
                ))
                .await
                .unwrap(),
            0
        );
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn old_gateway_cannot_adopt_a_restarted_plugins_epoch() {
        let (store, old, ctx) = setup().await;
        let (_, old_token) = pending(
            old.execute(
                &ctx,
                "reminder_create",
                json!({"after_seconds":3600,"text":"old pending"}),
            )
            .await
            .unwrap(),
        );
        store
            .activate_plugins(vec!["ai".into(), "clock".into(), "reminder".into()])
            .await
            .unwrap();
        old.migrate().await.unwrap();
        assert!(
            old.direct(
                ctx.scope.clone(),
                ctx.actor_id,
                "reminder_create",
                json!({"after_seconds":3600,"text":"stale direct"}),
                now()
            )
            .await
            .is_err()
        );
        assert!(old.definitions(&ctx).await.unwrap().is_empty());
        let fresh = Gateway::new(
            store.clone(),
            "UTC",
            old.registry.clone(),
            old.allowed.iter().cloned().collect(),
        )
        .unwrap();
        fresh.migrate().await.unwrap();
        assert!(
            fresh
                .confirm(&ctx.scope, ctx.actor_id, &old_token)
                .await
                .is_err()
        );
        let fresh_ctx = fresh
            .context(
                ctx.scope.clone(),
                ctx.actor_id,
                ctx.guard.clone(),
                store.epoch("ai").await.unwrap(),
                "new-turn".into(),
                now(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let (_, fresh_token) = pending(
            fresh
                .execute(
                    &fresh_ctx,
                    "reminder_create",
                    json!({"after_seconds":3600,"text":"fresh pending"}),
                )
                .await
                .unwrap(),
        );
        assert!(
            old.confirm(&ctx.scope, ctx.actor_id, &fresh_token)
                .await
                .is_err()
        );
        fresh
            .confirm(&ctx.scope, ctx.actor_id, &fresh_token)
            .await
            .unwrap();
        assert_eq!(
            store
                .call(
                    |db| Ok(db.query_row("SELECT count(*) FROM reminder_jobs", [], |r| r
                        .get::<_, i64>(0))?)
                )
                .await
                .unwrap(),
            1
        );
        store.stop().await.unwrap();
    }

    struct BrokenTool {
        panic: bool,
    }
    impl Tool for BrokenTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: if self.panic {
                    "panic_write"
                } else {
                    "large_receipt"
                },
                plugin: "reminder",
                version: 1,
                description: "test rollback boundary",
                parameters: json!({}),
                effect: Effect::LocalWrite,
                max_output_bytes: 32,
            }
        }
        fn prepare(&self, args: Value, _: &Scope, _: i64, _: i64, _: Tz) -> Result<Value> {
            object(&args, &[])?;
            Ok(args)
        }
        fn preview(&self, _: &Value, _: Tz) -> Result<String> {
            Ok("test".into())
        }
        fn apply(
            &self,
            tx: &Transaction<'_>,
            _: &Value,
            _: &Scope,
            _: i64,
            _: &str,
            _: i64,
        ) -> Result<ToolResult> {
            tx.execute("INSERT INTO boundary_effect VALUES(1)", [])?;
            assert!(!self.panic, "synthetic plugin panic");
            Ok(ToolResult {
                value: json!({"ok":true}),
                receipt: "x".repeat(33),
            })
        }
    }
    #[tokio::test]
    async fn panic_and_oversized_receipt_rollback_without_killing_database_worker() {
        for panic in [true, false] {
            let (store, _, ctx) = setup().await;
            store
                .call(|db| {
                    db.execute("CREATE TABLE boundary_effect(id INTEGER)", [])?;
                    Ok(())
                })
                .await
                .unwrap();
            let tool = Arc::new(BrokenTool { panic });
            let name = tool.spec().name;
            let mut registry = ToolRegistry::new();
            registry.register(tool).unwrap();
            let gateway = Gateway::new(store.clone(), "UTC", registry, vec![name.into()]).unwrap();
            gateway.migrate().await.unwrap();
            let (_, token) = pending(gateway.execute(&ctx, name, json!({})).await.unwrap());
            assert!(
                gateway
                    .confirm(&ctx.scope, ctx.actor_id, &token)
                    .await
                    .is_err()
            );
            let (rows, state) = store
                .call(move |db| {
                    Ok((
                        db.query_row("SELECT count(*) FROM boundary_effect", [], |r| {
                            r.get::<_, i64>(0)
                        })?,
                        db.query_row(
                            "SELECT status FROM tool_runs WHERE tool_name=?1",
                            [name],
                            |r| r.get::<_, String>(0),
                        )?,
                    ))
                })
                .await
                .unwrap();
            assert_eq!(rows, 0);
            assert_eq!(state, "failed");
            assert!(
                gateway
                    .confirm(&ctx.scope, ctx.actor_id, &token)
                    .await
                    .is_err()
            );
            store.stop().await.unwrap();
        }
    }
}
