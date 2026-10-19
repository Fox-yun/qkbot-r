use anyhow::{Context, Result};
use qkbot::{
    config::Config,
    host::{Host, Router},
};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut path = PathBuf::from("config.toml");
    let mut check = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => path = PathBuf::from(args.next().context("--config 需要文件路径")?),
            "--check" => check = true,
            "--help" | "-h" => {
                println!(
                    "qkbot [--config config.toml] [--check]\n--check 只校验配置，不连接 QQ 或模型。\n密钥环境变量：QKBOT_API_KEY、QKBOT_MILKY_TOKEN"
                );
                return Ok(());
            }
            "--version" | "-V" => {
                println!("qkbot {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => anyhow::bail!("未知参数：{arg}"),
        }
    }
    let config = Config::load(&path)?;
    // Relative data paths belong to the configuration directory.
    let mut config = config;
    if PathBuf::from(&config.app.database).is_relative() {
        config.app.database = path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join(&config.app.database)
            .to_string_lossy()
            .into_owned();
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_new(&config.app.log)?)
        .with_writer(std::io::stderr)
        .init();
    if check {
        let (_, _, enabled) = qkbot::plugins::assemble(&config)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        qkbot::milky::Milky::new(config.milky.clone(), client.clone())?;
        if config.plugins.ai.enabled {
            qkbot::llm::Llm::new(config.llm.clone(), client)?;
        }
        println!(
            "配置有效；启用插件：{}",
            enabled.into_iter().collect::<Vec<_>>().join(", ")
        );
        return Ok(());
    }
    let host = Host::create(config).await?;
    let bot = host
        .milky
        .login()
        .await
        .context("无法核对 Milky 登录账号")?;
    tracing::info!(bot_id = bot, "qkbot started");
    let router = Arc::new(Router::new(host.clone()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
    let milky = host.milky.clone();
    let cancel = host.cancel.clone();
    let events = tokio::spawn(async move {
        milky.events(bot, tx, cancel).await;
    });
    let background = {
        let router = router.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {_=router.host.cancel.cancelled()=>break,_=tick.tick()=>{if let Err(error)=router.maintenance().await{tracing::warn!(%error,"maintenance deferred");}}}
            }
        })
    };
    let reminders = {
        let router = router.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {_=router.host.cancel.cancelled()=>break,_=tick.tick()=>{if let Err(error)=router.reminders().await{tracing::warn!(%error,"reminder delivery stopped");}}}
            }
        })
    };
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _=&mut shutdown=>break,
            msg=rx.recv()=>match msg{Some(msg)=>{if let Err(error)=router.dispatch(msg).await{tracing::warn!(%error,"event rejected");}},None=>break},
        }
    }
    host.cancel.cancel();
    let _ = events.await;
    let _ = background.await;
    let _ = reminders.await;
    router.shutdown().await?;
    tracing::info!("qkbot stopped");
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
