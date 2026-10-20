use crate::{
    access::AccessStore,
    config::Config,
    llm::Llm,
    milky::{Milky, SendOutcome},
    plugins::ai::memory::MemoryStore,
    store::Store,
    tools::Gateway,
    types::{ContextGuard, Scope, now},
};
use anyhow::{Result, ensure};
use rusqlite::params;
use std::{collections::HashMap, sync::Arc};
use tokio_util::sync::CancellationToken;

pub struct Host {
    pub config: Arc<Config>,
    pub store: Store,
    pub access: AccessStore,
    pub milky: Milky,
    pub llm: Option<Llm>,
    pub memory: MemoryStore,
    pub gateway: Gateway,
    pub cancel: CancellationToken,
    pub epochs: HashMap<String, i64>,
    pub plugins: crate::plugin::PluginRegistry,
    owns_store: bool,
}
impl Host {
    pub async fn create(config: Config) -> Result<Arc<Self>> {
        config.validate_runtime()?;
        let store = Store::open(&config.app.database, config.limits.database_queue).await?;
        match Self::create_inner(config, store.clone(), true).await {
            Ok(host) => Ok(host),
            Err(error) => {
                let _ = store.stop().await;
                Err(error)
            }
        }
    }

    pub async fn create_with_store(config: Config, store: Store) -> Result<Arc<Self>> {
        Self::create_inner(config, store, false).await
    }

    async fn create_inner(config: Config, store: Store, owns_store: bool) -> Result<Arc<Self>> {
        config.validate()?;
        let (plugins, registry, enabled) = crate::plugins::assemble(&config)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(10))
            .pool_max_idle_per_host(4)
            .build()?;
        let milky = Milky::new(config.milky.clone(), http.clone())?;
        let budget = crate::budget::Budget::new(store.clone(), config.limits.clone());
        budget.init().await?;
        let llm = if config.plugins.ai.enabled {
            Some(Llm::new(config.llm.clone(), http)?.with_budget(budget))
        } else {
            None
        };
        store
            .activate_plugins(enabled.iter().cloned().collect())
            .await?;
        let mut epochs = HashMap::new();
        for id in enabled {
            epochs.insert(id.clone(), store.epoch(&id).await?);
        }
        let memory = MemoryStore::new(
            store.clone(),
            config.plugins.ai.max_memories_per_conversation,
        );
        memory.init().await?;
        let allowed = if config.plugins.ai.enabled {
            config.plugins.ai.defaults.allowed_tools.clone()
        } else {
            vec![]
        };
        let gateway = Gateway::new(store.clone(), &config.app.timezone, registry, allowed)?;
        gateway.migrate().await?;
        crate::plugins::reminder::migrate(&store).await?;
        crate::plugins::reminder::recover_interrupted(&store).await?;
        let host = Arc::new(Self {
            config: Arc::new(config),
            access: AccessStore::new(store.clone()),
            store,
            milky,
            llm,
            memory,
            gateway,
            cancel: CancellationToken::new(),
            epochs,
            plugins,
            owns_store,
        });
        for plugin in host.plugins.enabled_handlers() {
            tokio::time::timeout(std::time::Duration::from_secs(15), plugin.init(&host)).await??;
        }
        Ok(host)
    }
    pub async fn valid(&self, guard: &ContextGuard) -> Result<bool> {
        let g = guard.clone();
        let epoch = self.epochs.get("ai").copied().unwrap_or(0);
        self.store.call(move|c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ai_conversations c,plugin_states p WHERE c.id=?1 AND c.epoch=?2 AND p.id='ai' AND p.enabled=1 AND p.epoch=?3 AND NOT EXISTS(SELECT 1 FROM scope_access a WHERE a.scope=c.scope AND a.enabled=0))",params![g.conversation_id,g.epoch,epoch],|r|r.get(0))?)).await
    }

    async fn accepts_message(&self, message: &crate::types::Incoming) -> Result<bool> {
        let scope = message.key.scope.key();
        let source = message.key.key();
        self.store
            .call(move |db| Ok(crate::access::message_revision(db, &scope, &source).is_ok()))
            .await
    }

    /// Persist intent before submission. An ambiguous send is never retried here.
    pub async fn send(
        &self,
        scope: &Scope,
        plugin: &str,
        guard: Option<ContextGuard>,
        inbound_position: i64,
        text: String,
    ) -> Result<SendOutcome> {
        let scope_key = scope.key();
        let access_revision = self
            .store
            .call(move |db| {
                let revision = crate::access::require_enabled(db, &scope_key)?;
                if inbound_position > 0 {
                    use rusqlite::OptionalExtension;
                    let original: Option<i64> = db
                        .query_row(
                            "SELECT access_revision FROM messages WHERE id=?1 AND scope=?2",
                            params![inbound_position, scope_key],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if original != Some(revision) {
                        return Err(crate::access::AccessDisabled.into());
                    }
                }
                Ok(revision)
            })
            .await?;
        ensure!(
            !text.trim().is_empty() && text.len() <= self.config.limits.max_message_bytes,
            "发送文本为空或超出上限"
        );
        let plugin_epoch = *self
            .epochs
            .get(plugin)
            .ok_or_else(|| anyhow::anyhow!("plugin disabled"))?;
        let id = uuid::Uuid::new_v4().to_string();
        let hourly = self.config.limits.sends_per_hour as i64;
        let cooldown = if plugin == "ai" {
            self.config.limits.send_cooldown_secs as i64
        } else {
            0
        };
        loop {
            ensure!(!self.cancel.is_cancelled(), "宿主正在停止");
            let id1 = id.clone();
            let scope_key = scope.key();
            let body = text.clone();
            let origin = plugin.to_owned();
            let g = guard.clone();
            let delay=self.store.call(move|c| {
            let tx=c.transaction()?;
            crate::access::check_revision(&tx,&scope_key,access_revision)?;
            check_plugin(&tx,&origin,plugin_epoch)?;
            if let Some(g)=&g { check_guard(&tx,g)?; }
            let count:i64=tx.query_row("SELECT COUNT(*) FROM outbox WHERE scope=?1 AND created_at>?2 AND state IN('prepared','submitted','confirmed','unknown')",params![scope_key,now()-3600],|r|r.get(0))?;
            ensure!(count<hourly,"发送额度已用完，请稍后重试");
            if cooldown>0 {
                let last:Option<i64>=tx.query_row("SELECT MAX(created_at) FROM outbox WHERE scope=?1 AND origin_plugin='ai' AND state IN('prepared','submitted','confirmed','unknown')",[&scope_key],|r|r.get(0))?;
                if let Some(last)=last {let delay=last+cooldown-now();if delay>0{return Ok(Some(delay));}}
            }
            tx.execute("INSERT INTO outbox(id,scope,origin_plugin,plugin_epoch,conversation_id,context_epoch,body,state,created_at,updated_at,access_revision) VALUES(?1,?2,?3,?4,?5,?6,?7,'prepared',?8,?8,?9)",params![id1,scope_key,origin,plugin_epoch,g.as_ref().map(|x|&x.conversation_id),g.as_ref().map(|x|x.epoch),body,now(),access_revision])?;
            tx.commit()?; Ok(None)
        }).await?;
            match delay {
                None => break,
                Some(seconds) => {
                    tokio::select! {_=self.cancel.cancelled()=>anyhow::bail!("宿主正在停止"),_=tokio::time::sleep(std::time::Duration::from_secs(seconds as u64))=>{}}
                }
            }
        }
        let id1 = id.clone();
        let origin = plugin.to_owned();
        let g = guard.clone();
        let scope_key = scope.key();
        let _delivery = self.access.delivery_guard(scope).await?;
        let submit=self.store.call(move|c|{
            let tx=c.transaction()?;
            crate::access::check_revision(&tx,&scope_key,access_revision)?;
            check_plugin(&tx,&origin,plugin_epoch)?;
            if let Some(g)=&g {check_guard(&tx,g)?;}
            ensure!(tx.execute("UPDATE outbox SET state='submitted',updated_at=?2 WHERE id=?1 AND state='prepared'",params![id1,now()])?==1,"发送已取消");
            tx.commit()?;Ok(())
        }).await;
        if let Err(e) = submit {
            let why = e.to_string();
            self.store.call(move|c|{c.execute("UPDATE outbox SET state='failed',detail=?2,updated_at=?3 WHERE id=?1 AND state='prepared'",params![id,why,now()])?;Ok(())}).await?;
            return Err(e);
        }
        // Deliberately finish the ledger after submission, even if a reset cancels AI.
        let outcome = self.milky.send_text(scope, &text).await;
        let (state, seq, detail) = match &outcome {
            SendOutcome::Confirmed { message_seq } => {
                ("confirmed", Some(*message_seq), String::new())
            }
            SendOutcome::Failed { reason } => ("failed", None, reason.clone()),
            SendOutcome::Unknown { reason } => ("unknown", None, reason.clone()),
            SendOutcome::Deferred { reason } => {
                ("failed", None, format!("not submitted; deferred: {reason}"))
            }
        };
        if state != "confirmed" {
            tracing::warn!(outbox_id=%id,plugin,state,detail,"message delivery was not confirmed");
        }
        let scope = scope.clone();
        let origin = plugin.to_owned();
        let ledger_id = id.clone();
        let recorded=self.store.call(move|c| {
            let tx=c.transaction()?;
            tx.execute("UPDATE outbox SET state=?2,message_seq=?3,detail=?4,updated_at=?5 WHERE id=?1",params![id,state,seq,detail,now()])?;
            if let Some(seq)=seq {
                let valid=origin=="ai"&&guard.as_ref().is_some_and(|g|check_guard(&tx,g).is_ok())&&check_plugin(&tx,&origin,plugin_epoch).is_ok()&&crate::access::check_revision(&tx,&scope.key(),access_revision).is_ok();
                let (conversation,epoch)=if valid {let g=guard.as_ref().unwrap();(g.conversation_id.clone(),g.epoch)}else{(String::new(),0)};
                let role=if valid{"assistant"}else{"system"};
                let key=format!("{}:{}",scope.key(),seq);
                tx.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,origin_plugin,learnable,access_revision) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0,?11) ON CONFLICT(key) DO UPDATE SET role=excluded.role,content=excluded.content,conversation_id=excluded.conversation_id,epoch=excluded.epoch,inbound_position=excluded.inbound_position,origin_plugin=excluded.origin_plugin,access_revision=excluded.access_revision",params![key,scope.key(),conversation,epoch,scope.bot_id,role,text,now(),inbound_position,origin,access_revision])?;
            }
            tx.commit()?;Ok(())
        }).await;
        if let Err(error) = recorded {
            tracing::error!(outbox_id=%ledger_id,%error,state,"delivery result could not be persisted; submitted intent retained for recovery");
        }
        Ok(outcome)
    }

    pub async fn usage(&self, purpose: &str, response: &crate::llm::LlmResponse) -> Result<()> {
        let purpose = purpose.to_owned();
        let model = response.model.clone();
        let (Ok(input), Ok(output)) = (
            i64::try_from(response.usage.input_tokens),
            i64::try_from(response.usage.output_tokens),
        ) else {
            tracing::warn!(
                purpose,
                "provider usage exceeds supported counter range; usage not recorded"
            );
            return Ok(());
        };
        // Usage is observability, not a prerequisite for returning a paid-for answer
        // or committing a validated memory update. A logging failure must not repeat work.
        let recorded=self.store.call(move|c| { c.execute("INSERT INTO usage_daily(day,plugin_id,purpose,model,requests,input_tokens,output_tokens) VALUES(date('now'),'ai',?1,?2,1,?3,?4) ON CONFLICT(day,plugin_id,purpose,model) DO UPDATE SET requests=requests+1,input_tokens=input_tokens+excluded.input_tokens,output_tokens=output_tokens+excluded.output_tokens",params![purpose,model,input,output])?;Ok(()) }).await;
        if let Err(error) = recorded {
            tracing::warn!(%error, "model usage could not be recorded; preserving successful response");
        }
        Ok(())
    }
}

/// Fast ingress and a separately bounded command lane; model work never blocks controls.
pub struct Router {
    pub host: Arc<Host>,
    pub chat: Arc<crate::plugins::ai::chat::Chat>,
    controls: Arc<tokio::sync::Semaphore>,
    plugin_limits: HashMap<String, Arc<tokio::sync::Semaphore>>,
    tasks: tokio_util::task::TaskTracker,
}
impl Router {
    pub fn new(host: Arc<Host>) -> Self {
        let plugin_limits = host
            .epochs
            .keys()
            .map(|id| (id.clone(), Arc::new(tokio::sync::Semaphore::new(4))))
            .collect();
        Self {
            chat: crate::plugins::ai::chat::Chat::new(host.clone()),
            controls: Arc::new(tokio::sync::Semaphore::new(
                host.config.limits.control_tasks,
            )),
            host,
            plugin_limits,
            tasks: tokio_util::task::TaskTracker::new(),
        }
    }

    pub async fn dispatch(&self, msg: crate::types::Incoming) -> Result<()> {
        use crate::plugins::ai;
        let host = &self.host;
        if msg.text.len() > host.config.limits.max_message_bytes
            || removed_management_command(&msg.text)
        {
            return Ok(());
        }
        let Some(access_revision) = host.access.allowed_revision(&msg.key.scope).await? else {
            return Ok(());
        };
        let handler = msg
            .text
            .split_whitespace()
            .next()
            .and_then(|root| host.plugins.handler_for(root));
        let registered = msg.text.split_whitespace().next().is_some_and(|root| {
            host.plugins
                .specs()
                .any(|spec| spec.commands.contains(&root))
        });
        let command = command_root(&msg.text).or(if registered { Some("plugin") } else { None });
        let conv = if host.config.plugins.ai.enabled && !msg.is_self() {
            Some(ai::conversation(host, &msg).await?)
        } else {
            None
        };
        let trigger = ai::chat::triggered(&msg, &host.config.plugins.ai.keywords);
        let learn = conv
            .as_ref()
            .is_some_and(|c| c.settings.memory_write && (!c.settings.paused || c.settings.once))
            && command.is_none()
            && (trigger || host.config.plugins.ai.learn_all_group_messages)
            && !msg.is_self();
        let keep = conv.is_some()
            && !msg.is_self()
            && command.is_none()
            && (trigger || host.config.plugins.ai.read_group_background);
        let incoming = msg.clone();
        let context = conv.clone();
        let pos=host.store.call(move|c|{
            let tx=c.transaction()?;
            if crate::access::check_revision(&tx,&incoming.key.scope.key(),access_revision).is_err() { return Ok(None); }
            let (id,epoch)=context.as_ref().map(|v|(v.guard.conversation_id.clone(),v.guard.epoch)).unwrap_or((String::new(),0));
            let role=if incoming.is_self(){"self_unknown"}else if keep||learn{"user"}else{"control"};
            let text=if keep||learn{incoming.text.as_str()}else{""};
            let origin=if command.is_some(){Some("control")}else if learn&&!keep {Some("memory_only")}else{None};
            let changed=tx.execute("INSERT OR IGNORE INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,origin_plugin,learnable,access_revision) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",params![incoming.key.key(),incoming.key.scope.key(),id,epoch,incoming.sender_id,role,text,incoming.received_at,origin,learn,access_revision])?;
            if changed==0{return Ok(None);}
            let id=tx.last_insert_rowid();tx.execute("UPDATE messages SET inbound_position=id WHERE id=?1",[id])?;
            tx.commit()?;Ok(Some(id))
        }).await?;
        let Some(position) = pos else {
            return Ok(());
        };
        if msg.is_self() {
            return Ok(());
        }
        if let Some(command) = command {
            let limiter = handler
                .as_ref()
                .and_then(|h| self.plugin_limits.get(h.spec().id))
                .unwrap_or(&self.controls)
                .clone();
            let permit = match limiter.try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!("command queue full");
                    return Ok(());
                }
            };
            let host = host.clone();

            let Some(handler) = handler else {
                let result = route_command(&host, &msg, command).await;
                let text = result.unwrap_or_else(|e| format!("指令未完成：{e}"));
                self.tasks.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = host
                        .send(&msg.key.scope, "host", None, position, text)
                        .await
                    {
                        tracing::warn!(%error, "command receipt delivery failed");
                    }
                });
                return Ok(());
            };
            self.tasks.spawn(async move {
                let _permit = permit;
                if !host.accepts_message(&msg).await.unwrap_or(false) {
                    return;
                }
                let origin = handler.spec().id;
                let result = if handler.accepts(&msg) {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(15),
                        handler.command(&host, &msg),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => Err(anyhow::anyhow!("插件指令超时")),
                    }
                } else {
                    Err(anyhow::anyhow!("插件不接受当前会话"))
                };
                let text = result.unwrap_or_else(|e| format!("指令未完成：{e}"));
                if let Err(error) = host
                    .send(&msg.key.scope, origin, None, position, text)
                    .await
                {
                    tracing::warn!(%error,"command receipt delivery failed");
                }
            });
            return Ok(());
        }
        for observer in host.plugins.observers() {
            if !observer.accepts(&msg) {
                continue;
            }
            let Some(limit) = self.plugin_limits.get(observer.spec().id) else {
                continue;
            };
            let Ok(permit) = limit.clone().try_acquire_owned() else {
                continue;
            };
            let host = host.clone();
            let message = msg.clone();
            self.tasks.spawn(async move {
                let _permit = permit;
                if !host.accepts_message(&message).await.unwrap_or(false) {
                    return;
                }
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    observer.on_message(&host, &message),
                )
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    tracing::warn!(
                        plugin = observer.spec().id,
                        "plugin event failed or timed out"
                    );
                }
            });
        }
        let Some(conv) = conv else {
            return Ok(());
        };
        if !trigger || (conv.settings.paused && !conv.settings.once) {
            return Ok(());
        }
        match self.chat.enqueue(&msg, position, &conv) {
            Ok(()) => {
                if conv.settings.once {
                    let g = conv.guard.clone();
                    host.store.call(move|c|{
                        c.execute("UPDATE ai_conversations SET settings=json_set(settings,'$.once',json('false')) WHERE id=?1 AND epoch=?2",params![g.conversation_id,g.epoch])?;Ok(())
                    }).await?;
                }
            }
            Err(error) => {
                tracing::warn!(%error,"chat rejected at queue boundary");
                // Feedback itself has the bounded control and send limits.
                if let Ok(permit) = self.controls.clone().try_acquire_owned() {
                    let host = host.clone();
                    self.tasks.spawn(async move {
                        let _permit = permit;
                        let _ = host
                            .send(
                                &msg.key.scope,
                                "host",
                                None,
                                position,
                                "当前聊天队列已满，请稍后重试。".into(),
                            )
                            .await;
                    });
                }
            }
        }
        Ok(())
    }

    pub async fn maintenance(&self) -> Result<()> {
        let seconds = (self.host.config.limits.retention_days * 86400).min(i64::MAX as u64) as i64;
        let cutoff = now() - seconds;
        self.host
            .store
            .call(move |c| {
                // Never expire unresolved sends or active conversation work as ordinary logs.
                c.execute("DELETE FROM messages WHERE received_at<?1", [cutoff])?;
                c.execute(
                    "DELETE FROM outbox WHERE created_at<?1 AND state IN('confirmed','failed')",
                    [cutoff],
                )?;
                Ok(())
            })
            .await?;
        self.host.gateway.maintain(seconds).await?;
        crate::plugins::reminder::maintain(&self.host.store, seconds).await?;
        if self.host.config.plugins.ai.enabled {
            self.chat.compact_idle().await?;
        }
        Ok(())
    }

    pub async fn reminders(&self) -> Result<()> {
        use crate::plugins::reminder::{self, Delivery};
        if !self.host.config.plugins.reminder.enabled {
            return Ok(());
        }
        if !self.host.milky.is_verified() {
            return Ok(());
        }
        for due in reminder::claim_due(&self.host.store, 4).await? {
            if self.host.cancel.is_cancelled() {
                reminder::release_claim(&self.host.store, &due.id).await?;
                continue;
            }
            if self.host.epochs.get("reminder") != Some(&due.plugin_epoch) {
                reminder::finish_delivery(
                    &self.host.store,
                    &due.id,
                    Delivery::KnownFailure,
                    Some("plugin changed before submission"),
                )
                .await?;
                continue;
            }
            let text = format!("提醒 用户 {}：{}\nID：{}", due.actor_id, due.text, due.id);
            let result = self.host.send(&due.scope, "reminder", None, 0, text).await;
            if result.as_ref().is_err_and(|error| {
                error
                    .downcast_ref::<crate::access::AccessDisabled>()
                    .is_some()
            }) || (result.is_err() && self.host.cancel.is_cancelled())
            {
                reminder::release_claim(&self.host.store, &due.id).await?;
                continue;
            }
            let (state, error) = match result {
                Ok(SendOutcome::Deferred { .. }) => {
                    reminder::release_claim(&self.host.store, &due.id).await?;
                    continue;
                }
                Ok(SendOutcome::Confirmed { .. }) => (Delivery::Sent, None),
                Ok(SendOutcome::Failed { reason }) => (Delivery::KnownFailure, Some(reason)),
                Ok(SendOutcome::Unknown { reason }) => (Delivery::Unknown, Some(reason)),
                Err(error) => (Delivery::KnownFailure, Some(error.to_string())),
            };
            reminder::finish_delivery(&self.host.store, &due.id, state, error.as_deref()).await?;
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.chat.shutdown().await;
        self.tasks.close();
        self.tasks.wait().await;
        for plugin in self.host.plugins.enabled_handlers().into_iter().rev() {
            if !matches!(
                tokio::time::timeout(std::time::Duration::from_secs(15), plugin.stop(&self.host))
                    .await,
                Ok(Ok(()))
            ) {
                tracing::warn!(
                    plugin = plugin.spec().id,
                    "plugin cleanup failed or timed out"
                );
            }
        }
        self.host.store.call_wait(|db| {
            let tx=db.transaction()?;
            tx.execute("UPDATE outbox SET state='failed',detail='runtime stopped before submission',updated_at=?1 WHERE state='prepared'",[now()])?;
            tx.execute("UPDATE plugin_states SET enabled=0,epoch=epoch+1",[])?;
            tx.commit()?;
            Ok(())
        }).await?;
        if self.host.owns_store {
            self.host.store.stop().await?;
        }
        Ok(())
    }
}

// Tombstones live only at ingress: no handler, storage, reply, observer, or model work.
fn removed_management_command(text: &str) -> bool {
    let parts: Vec<_> = text.split_whitespace().take(4).collect();
    match parts.first().copied() {
        Some("/ai") => true,
        Some("/bot") => !matches!(parts.as_slice(), [_, "confirm" | "reject", _]),
        _ => false,
    }
}

async fn route_command(host: &Host, msg: &crate::types::Incoming, command: &str) -> Result<String> {
    ensure!(host.accepts_message(msg).await?, "此指令已失效");
    use crate::tools::ToolOutcome;
    let parts: Vec<_> = msg.text.split_whitespace().collect();
    if command == "/bot" {
        match parts.as_slice() {
            [_, "confirm", token] => match host
                .gateway
                .confirm(&msg.key.scope, msg.sender_id, token)
                .await?
            {
                ToolOutcome::Completed { receipt, .. } => return Ok(receipt),
                _ => anyhow::bail!("确认后状态异常"),
            },
            [_, "reject", token] => {
                host.gateway
                    .reject(&msg.key.scope, msg.sender_id, token)
                    .await?;
                return Ok("已拒绝该操作。".into());
            }
            _ => anyhow::bail!("无效确认操作"),
        }
    }
    anyhow::bail!("此插件已停用")
}

fn command_root(text: &str) -> Option<&'static str> {
    match text.split_whitespace().next()? {
        "/bot" => Some("/bot"),

        "/clock" => Some("/clock"),
        "/remind" => Some("/remind"),
        _ => None,
    }
}
pub fn check_guard(c: &rusqlite::Connection, g: &ContextGuard) -> Result<()> {
    let valid: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM ai_conversations c WHERE id=?1 AND epoch=?2 AND NOT EXISTS(SELECT 1 FROM scope_access a WHERE a.scope=c.scope AND a.enabled=0))",
        params![g.conversation_id, g.epoch],
        |r| r.get(0),
    )?;
    ensure!(valid, "会话已变更，旧操作已失效");
    Ok(())
}
pub fn check_plugin(c: &rusqlite::Connection, id: &str, epoch: i64) -> Result<()> {
    let valid: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM plugin_states WHERE id=?1 AND epoch=?2 AND enabled=1)",
        params![id, epoch],
        |r| r.get(0),
    )?;
    ensure!(valid, "插件已停用或重启");
    Ok(())
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    #[test]
    fn only_top_level_known_commands_are_claimed() {
        assert_eq!(command_root("/clock now"), Some("/clock"));
        assert_eq!(command_root("他说 /bot confirm TOKEN"), None);
        assert_eq!(command_root("/aifoo"), None);
    }
    #[tokio::test]
    async fn disabled_group_ingress_is_silent_and_never_persists_or_executes() {
        let mut config = Config::from_toml(include_str!("../config.example.toml")).unwrap();
        config.app.database = ":memory:".into();
        config.plugins.ai.enabled = false;
        let host = Host::create(config).await.unwrap();
        let router = Router::new(host.clone());
        let scope = Scope {
            bot_id: 1,
            scene: crate::types::Scene::Group,
            peer_id: 20,
        };
        host.access.set_enabled(scope.clone(), false).await.unwrap();
        for (index, text) in [
            "小鱼记住我喜欢绿茶",
            "/clock",
            "/remind add 60 不应创建",
            "/bot confirm anything",
        ]
        .into_iter()
        .enumerate()
        {
            router
                .dispatch(crate::types::Incoming {
                    key: crate::types::MessageKey {
                        scope: scope.clone(),
                        message_seq: index as i64 + 1,
                    },
                    sender_id: 7,
                    text: text.into(),
                    mentions: vec![1],
                    quote_seq: None,
                    received_at: now(),
                })
                .await
                .unwrap();
        }
        assert!(
            host.send(&scope, "host", None, 0, "不应发送".into())
                .await
                .is_err()
        );
        host.store
            .call(|db| {
                for table in [
                    "messages",
                    "outbox",
                    "ai_memories",
                    "tool_runs",
                    "reminder_jobs",
                ] {
                    assert_eq!(
                        db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                            .get::<_, i64>(0))?,
                        0
                    );
                }
                Ok(())
            })
            .await
            .unwrap();
        router.shutdown().await.unwrap();
    }
}
