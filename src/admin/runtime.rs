//! The control panel stays alive while the bot waits for QQ or changes configuration.
use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
    config::Config,
    host::{Host, Router},
    plugins::ai::memory::MemoryStore,
    store::Store,
    types::now,
};

#[derive(Clone, Serialize)]
pub struct RuntimeStatus {
    pub runtime_generation: u64,
    pub phase: String,
    pub online: bool,
    pub bot_id: Option<i64>,
    pub message: String,
    pub started_at: Option<i64>,
    pub applied_revision: Option<String>,
    pub loaded_plugins: Vec<String>,
}

impl Default for RuntimeStatus {
    fn default() -> Self {
        Self {
            runtime_generation: 0,
            phase: "stopped".into(),
            online: false,
            bot_id: None,
            message: "机器人已停止，管理页面仍可使用。".into(),
            started_at: None,
            applied_revision: None,
            loaded_plugins: Vec::new(),
        }
    }
}

enum Command {
    Apply(Box<Config>, String),
    Stop,
}

#[derive(Debug)]
pub struct DataUnavailable;
impl std::fmt::Display for DataUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("数据服务尚未就绪或数据库不可用")
    }
}
impl std::error::Error for DataUnavailable {}

pub struct Runtime {
    status: RwLock<RuntimeStatus>,
    active: RwLock<Option<Arc<Host>>>,
    commands: mpsc::Sender<Command>,
    data: RwLock<Option<Store>>,
    config: RwLock<Arc<Config>>,
    router: RwLock<Option<Arc<Router>>>,
    database: String,
    database_queue: usize,
}

struct BotTask {
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl Runtime {
    pub fn start(
        config: Config,
        revision: String,
        cancel: CancellationToken,
    ) -> (Arc<Self>, tokio::task::JoinHandle<()>) {
        let (commands, receiver) = mpsc::channel(1);
        let runtime = Arc::new(Self {
            status: RwLock::new(RuntimeStatus::default()),
            active: RwLock::new(None),
            commands,
            data: RwLock::new(None),
            config: RwLock::new(Arc::new(config.clone())),
            router: RwLock::new(None),
            database: config.app.database.clone(),
            database_queue: config.limits.database_queue,
        });
        let worker = runtime.clone();
        let task = tokio::spawn(async move {
            worker.supervise(receiver, config, revision, cancel).await;
        });
        (runtime, task)
    }

    pub async fn status(&self) -> RuntimeStatus {
        let mut status = self.status.read().await.clone();
        status.online = self
            .active
            .read()
            .await
            .as_ref()
            .is_some_and(|h| h.milky.is_verified());
        status.loaded_plugins = self.loaded_plugins().await;
        if status.phase == "running" && !status.online {
            status.phase = "waiting_login".into();
            status.message = "QQ / Milky 连接暂不可用，正在自动重连。".into();
        }
        status
    }

    /// Report initialized handlers, rather than the next configuration being applied.
    pub async fn loaded_plugins(&self) -> Vec<String> {
        let active = self.active.read().await;
        let Some(host) = active.as_ref().filter(|host| !host.cancel.is_cancelled()) else {
            return Vec::new();
        };
        let mut plugins: Vec<_> = host.epochs.keys().cloned().collect();
        plugins.sort();
        plugins
    }

    pub fn apply(&self, config: Config, revision: String) -> Result<()> {
        anyhow::ensure!(
            config.app.database == self.database
                && config.limits.database_queue == self.database_queue,
            "数据库路径或数据库队列配置已改变，请重启程序以应用"
        );
        config.validate_runtime()?;
        self.commands
            .try_send(Command::Apply(Box::new(config), revision))
            .context("已有管理操作正在等待执行，请稍后重试")
    }

    pub fn stop(&self) -> Result<()> {
        self.commands
            .try_send(Command::Stop)
            .context("已有管理操作正在等待执行，请稍后重试")
    }

    pub async fn data(&self) -> Result<Store> {
        self.data
            .read()
            .await
            .clone()
            .ok_or_else(|| DataUnavailable.into())
    }

    pub async fn config(&self) -> Arc<Config> {
        self.config.read().await.clone()
    }

    /// Keep configuration-dependent mutations consistent until their database commit.
    pub(crate) async fn config_read(&self) -> tokio::sync::RwLockReadGuard<'_, Arc<Config>> {
        self.config.read().await
    }

    pub async fn cancel_scopes(&self, scopes: &[String]) {
        let Ok(store) = self.data().await else { return };
        let scopes = scopes.to_vec();
        let epochs = store
            .call(move |db| {
                let mut statement =
                    db.prepare("SELECT id,epoch FROM ai_conversations WHERE scope=?1")?;
                let mut epochs = Vec::new();
                for scope in scopes {
                    epochs.extend(
                        statement
                            .query_map([scope], |r| {
                                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                            })?
                            .collect::<rusqlite::Result<Vec<_>>>()?,
                    );
                }
                Ok(epochs)
            })
            .await;
        match epochs {
            Ok(epochs) => {
                if let Some(router) = self.router.read().await.as_ref() {
                    for (id, epoch) in epochs {
                        router.chat.cancel_before(&id, epoch);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error,"prompt cancellation deferred; durable epoch still blocks stale output")
            }
        }
    }

    async fn phase(&self, phase: &str, message: &str) {
        let mut status = self.status.write().await;
        status.phase = phase.into();
        status.message = message.into();
    }

    async fn finish(&self, task: BotTask) -> Result<()> {
        self.phase("stopping", "正在等待已提交操作完成并停止机器人……")
            .await;
        task.cancel.cancel();
        task.task.await.context("机器人任务异常结束")??;
        Ok(())
    }

    async fn supervise(
        self: Arc<Self>,
        mut receiver: mpsc::Receiver<Command>,
        config: Config,
        revision: String,
        cancel: CancellationToken,
    ) {
        let opened = async {
            let store = Store::open(&config.app.database, config.limits.database_queue).await?;
            let ready = async {
                MemoryStore::new(
                    store.clone(),
                    config.plugins.ai.max_memories_per_conversation,
                )
                .init()
                .await?;
                crate::plugins::ai::stickers::init_store(&store).await?;
                crate::budget::Budget::new(store.clone(), config.limits.clone())
                    .init()
                    .await?;
                anyhow::Ok(())
            }
            .await;
            if let Err(error) = ready {
                let _ = store.stop().await;
                return Err(error);
            }
            Ok(store)
        }
        .await;
        match opened {
            Ok(store) => *self.data.write().await = Some(store),
            Err(error) => {
                tracing::error!(%error,"shared data service failed");
            }
        }
        let mut pending = Some(Command::Apply(Box::new(config), revision));
        let mut running: Option<BotTask> = None;
        loop {
            if let Some(command) = pending.take() {
                if let Some(task) = running.take()
                    && let Err(error) = self.finish(task).await
                {
                    tracing::error!(%error, "bot shutdown failed");
                    self.phase("error", "机器人未能正常停止，请检查服务日志后重启程序。")
                        .await;
                    // Never start a second database owner after an uncertain shutdown.
                    break;
                }
                *self.active.write().await = None;
                *self.router.write().await = None;
                let generation = self
                    .status
                    .read()
                    .await
                    .runtime_generation
                    .saturating_add(1);
                match command {
                    Command::Stop => {
                        *self.status.write().await = RuntimeStatus {
                            runtime_generation: generation,
                            ..Default::default()
                        }
                    }
                    Command::Apply(config, revision) => {
                        if cancel.is_cancelled() {
                            break;
                        }
                        *self.config.write().await = Arc::new((*config).clone());
                        *self.status.write().await = RuntimeStatus {
                            runtime_generation: generation,
                            phase: "starting".into(),
                            message: "正在加载已保存的配置……".into(),
                            started_at: Some(now()),
                            ..Default::default()
                        };
                        let stop = CancellationToken::new();
                        let child_cancel = stop.clone();
                        let worker = self.clone();
                        let task = tokio::spawn(async move {
                            worker.run_bot(*config, revision, child_cancel).await
                        });
                        running = Some(BotTask { cancel: stop, task });
                    }
                }
            }
            if let Some(task) = running.as_mut() {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    command = receiver.recv() => match command { Some(command) => pending = Some(command), None => break },
                    result = &mut task.task => {
                        running = None;
                        *self.active.write().await = None;
                        *self.router.write().await = None;
                        match result {
                            Ok(Ok(())) => *self.status.write().await = RuntimeStatus::default(),
                            Ok(Err(error)) => {
                                tracing::error!(%error, "bot runtime failed");
                                self.phase("error", "机器人启动或运行失败，请检查连接配置和服务日志；管理页仍可使用。").await;
                            }
                            Err(error) => {
                                tracing::error!(%error, "bot task panicked");
                                self.phase("error", "机器人任务异常，请检查日志并重启程序。").await;
                                break;
                            }
                        }
                    }
                }
            } else {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    command = receiver.recv() => match command { Some(command) => pending = Some(command), None => break },
                }
            }
        }
        if let Some(task) = running {
            if let Err(error) = self.finish(task).await {
                tracing::error!(%error, "final bot shutdown failed");
            }
            *self.active.write().await = None;
        }
        *self.router.write().await = None;
        if let Some(store) = self.data.write().await.take()
            && let Err(error) = store.stop().await
        {
            tracing::error!(%error,"shared data shutdown failed");
        }
    }

    async fn run_bot(
        &self,
        config: Config,
        revision: String,
        cancel: CancellationToken,
    ) -> Result<()> {
        // Reserve before changing plugin epochs; a port conflict cannot leave a
        // partially started runtime. This small listener also serves native PNGs.
        let bridge_listener = if config.nonebot.enabled || !config.plugins.native.enabled.is_empty()
        {
            Some(
                tokio::net::TcpListener::bind(&config.nonebot.listen)
                    .await
                    .context("无法监听插件媒体/NoneBot本机端口")?,
            )
        } else {
            None
        };
        let host = Host::create_with_store(config, self.data().await?).await?;
        let router = Arc::new(Router::new(host.clone()));
        if let Some(listener) = bridge_listener {
            let host = host.clone();
            host.tasks.clone().spawn(async move {
                if let Err(error) = crate::bridge::serve(host.clone(), listener).await {
                    tracing::error!(%error,"plugin gateway stopped");
                    host.cancel.cancel();
                }
            });
        }
        *self.router.write().await = Some(router.clone());
        *self.active.write().await = Some(host.clone());
        self.status.write().await.applied_revision = Some(revision);
        self.phase(
            "waiting_login",
            "等待 QQ 登录或 Milky 连接；请扫码并确认 Milky 地址和令牌。",
        )
        .await;
        let bot = loop {
            let result = tokio::select! {
                _ = cancel.cancelled() => { host.cancel.cancel(); router.shutdown().await?; return Ok(()); },
                _ = host.cancel.cancelled() => { router.shutdown().await?; return Ok(()); },
                result = host.milky.login() => result,
            };
            match result {
                Ok(bot) => break bot,
                Err(_) => tokio::select! {
                    _ = cancel.cancelled() => { host.cancel.cancel(); router.shutdown().await?; return Ok(()); },
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {},
                },
            }
        };
        {
            let mut status = self.status.write().await;
            status.phase = "running".into();
            status.bot_id = Some(bot);
            status.message = "机器人正在运行。".into();
        }
        let (sender, mut receiver) = mpsc::channel(128);
        let milky = host.milky.clone();
        let stop = host.cancel.clone();
        let events = tokio::spawn(async move {
            milky.events(bot, sender, stop).await;
        });
        let maintenance = {
            let router = router.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(60));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = router.host.cancel.cancelled() => break,
                        _ = tick.tick() => if let Err(error) = router.maintenance().await {
                            tracing::warn!(%error, "maintenance deferred");
                        },
                    }
                }
            })
        };
        let embeddings = {
            let host = host.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(2));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = host.cancel.cancelled() => break,
                        _ = tick.tick() => {
                            if host.embedding.is_some() {
                                if host.stickers.tick(&host, &host.cancel).await.is_err() {
                                    tracing::debug!("sticker indexing deferred");
                                }
                                if crate::plugins::ai::semantic::index_next(&host, &host.cancel).await.is_err() {
                                    tracing::debug!("memory semantic indexing deferred");
                                }
                            }
                        },
                    }
                }
            })
        };
        let reminders = {
            let router = router.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = router.host.cancel.cancelled() => break,
                        _ = tick.tick() => if let Err(error) = router.reminders().await {
                            tracing::warn!(%error, "reminder delivery stopped");
                        },
                    }
                }
            })
        };
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = host.cancel.cancelled() => break,
                message = receiver.recv() => match message {
                    Some(message) => if let Err(error) = router.dispatch(message).await {
                        tracing::warn!(%error, "event rejected");
                    },
                    None => break,
                }
            }
        }
        host.cancel.cancel();
        let event_result = events.await;
        let maintenance_result = maintenance.await;
        let reminder_result = reminders.await;
        let embedding_result = embeddings.await;
        router.shutdown().await?;
        if event_result.is_err()
            || maintenance_result.is_err()
            || reminder_result.is_err()
            || embedding_result.is_err()
        {
            bail!("机器人后台任务异常结束");
        }
        Ok(())
    }
}
