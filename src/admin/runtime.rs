//! The control panel stays alive while the bot waits for QQ or changes configuration.
use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{
    config::Config,
    host::{Host, Router},
    types::now,
};

#[derive(Clone, Serialize)]
pub struct RuntimeStatus {
    pub phase: String,
    pub online: bool,
    pub bot_id: Option<i64>,
    pub message: String,
    pub started_at: Option<i64>,
    pub applied_revision: Option<String>,
}

impl Default for RuntimeStatus {
    fn default() -> Self {
        Self {
            phase: "stopped".into(),
            online: false,
            bot_id: None,
            message: "机器人已停止，管理页面仍可使用。".into(),
            started_at: None,
            applied_revision: None,
        }
    }
}

enum Command {
    Apply(Box<Config>, String),
    Stop,
}

pub struct Runtime {
    status: RwLock<RuntimeStatus>,
    active: RwLock<Option<Arc<Host>>>,
    commands: mpsc::Sender<Command>,
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
        if status.phase == "running" && !status.online {
            status.phase = "waiting_login".into();
            status.message = "QQ / Milky 连接暂不可用，正在自动重连。".into();
        }
        status
    }

    pub fn apply(&self, config: Config, revision: String) -> Result<()> {
        self.commands
            .try_send(Command::Apply(Box::new(config), revision))
            .context("已有管理操作正在等待执行，请稍后重试")
    }

    pub fn stop(&self) -> Result<()> {
        self.commands
            .try_send(Command::Stop)
            .context("已有管理操作正在等待执行，请稍后重试")
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
                match command {
                    Command::Stop => *self.status.write().await = RuntimeStatus::default(),
                    Command::Apply(config, revision) => {
                        if cancel.is_cancelled() {
                            break;
                        }
                        *self.status.write().await = RuntimeStatus {
                            phase: "starting".into(),
                            message: "正在加载已保存的配置……".into(),
                            applied_revision: Some(revision),
                            started_at: Some(now()),
                            ..Default::default()
                        };
                        let stop = CancellationToken::new();
                        let child_cancel = stop.clone();
                        let worker = self.clone();
                        let task =
                            tokio::spawn(
                                async move { worker.run_bot(*config, child_cancel).await },
                            );
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
    }

    async fn run_bot(&self, config: Config, cancel: CancellationToken) -> Result<()> {
        let host = Host::create(config).await?;
        let router = Arc::new(Router::new(host.clone()));
        *self.active.write().await = Some(host.clone());
        self.phase(
            "waiting_login",
            "等待 QQ 登录或 Milky 连接；请扫码并确认 Milky 地址和令牌。",
        )
        .await;
        let bot = loop {
            let result = tokio::select! {
                _ = cancel.cancelled() => { host.cancel.cancel(); router.shutdown().await?; return Ok(()); },
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
        router.shutdown().await?;
        if event_result.is_err() || maintenance_result.is_err() || reminder_result.is_err() {
            bail!("机器人后台任务异常结束");
        }
        Ok(())
    }
}
