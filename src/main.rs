use anyhow::{Context, Result};
use qkbot::{
    admin::{self, AdminState, auth::AuthService, runtime::Runtime, settings::ConfigStore},
    config::Config,
};
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut path = PathBuf::from("config.toml");
    let mut check = false;
    let mut rebuild = None;
    let mut rebuild_status = None;
    let mut rebuild_batches = 1000;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => path = PathBuf::from(args.next().context("--config 需要文件路径")?),
            "--check" => check = true,
            "--rebuild-memory" => {
                rebuild = Some(args.next().context("--rebuild-memory 需要任务名")?)
            }
            "--memory-rebuild-status" => {
                rebuild_status = Some(args.next().context("--memory-rebuild-status 需要任务名")?)
            }
            "--rebuild-max-batches" => {
                rebuild_batches = args
                    .next()
                    .context("--rebuild-max-batches 需要数量")?
                    .parse::<usize>()
                    .context("回扫批次数无效")?
            }
            "--help" | "-h" => {
                println!(
                    "qkbot [--config config.toml] [--check]\n--check 只校验配置，不连接 QQ 或模型。\n--rebuild-memory TASK [--rebuild-max-batches N] 补整理当前会话保留的可学历史，同名任务可续跑；不启动机器人、不改变摘要和正常游标。\n--memory-rebuild-status TASK 查看回扫统计。\n首次启动自动创建配置，在终端显示管理地址和一次性初始化码，用于设置管理员密码。\n密钥环境变量：QKBOT_API_KEY、QKBOT_MILKY_TOKEN、QKBOT_LLBOT_TOKEN、QKBOT_NONEBOT_TOKEN"
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
    anyhow::ensure!(
        usize::from(check) + usize::from(rebuild.is_some()) + usize::from(rebuild_status.is_some())
            <= 1,
        "校验、历史回扫和状态查询不能同时运行"
    );
    if let Some(id) = rebuild_status {
        return qkbot::plugins::ai::memory_rebuild::show_status(&path, &id);
    }
    if let Some(id) = rebuild {
        let result = qkbot::plugins::ai::memory_rebuild::run(&path, &id, rebuild_batches).await;
        if let Err(error) = result {
            // Only the outer fixed stage is logged. Provider output, source
            // quotes, SQL parameters and credentials must not reach the CLI.
            eprintln!(
                "{}",
                qkbot::plugins::ai::memory_rebuild::failure_diagnostic(&error)
            );
            anyhow::bail!("历史回扫未完成，已提交进度保留，可使用同一任务名续跑");
        }
        return Ok(());
    }
    if check {
        let config = Config::load(&path)?;
        config.validate_runtime()?;
        let (_, _, enabled) = qkbot::plugins::assemble(&config)?;
        println!(
            "配置有效；启用插件：{}",
            enabled.into_iter().collect::<Vec<_>>().join(", ")
        );
        return Ok(());
    }
    let store = Arc::new(ConfigStore::open(path).await?);
    let (config, revision) = store.effective_with_revision().await?;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_new(&config.app.log)?)
        .with_writer(std::io::stderr)
        .init();
    // Bind before opening the database. A duplicate process must not invalidate
    // the running bot's plugin epochs merely because its panel port is occupied.
    let listener = if config.web.enabled {
        Some(
            tokio::net::TcpListener::bind(&config.web.listen)
                .await
                .context("无法监听管理员页面地址")?,
        )
    } else {
        None
    };
    let token = if config.web.token.is_empty() {
        format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        )
    } else {
        config.web.token.clone()
    };
    let auth = AuthService::open(store.path(), &token, config.web.secure_cookie).await?;
    if let Some(listener) = &listener {
        tracing::info!(address = %listener.local_addr()?, "admin panel ready");
        eprintln!("管理员页面：http://{}", listener.local_addr()?);
        if !auth.is_configured() {
            eprintln!(
                "管理员初始化码：{token}\n打开管理页面并设置管理员密码；设置成功后初始化码失效。"
            );
        }
    }
    let cancel = CancellationToken::new();
    let (runtime, worker) = Runtime::start(config, revision, cancel.clone());
    let web_cancel = CancellationToken::new();
    let admin_stop = web_cancel.clone();
    let mut server = if let Some(listener) = listener {
        let state = AdminState::new(store, runtime, auth);
        tokio::spawn(async move { admin::serve(listener, state, admin_stop).await })
    } else {
        tokio::spawn(async move {
            admin_stop.cancelled().await;
            Ok(())
        })
    };
    let server_result = tokio::select! {
        _ = shutdown_signal() => None,
        result = &mut server => Some(result),
    };
    web_cancel.cancel();
    let result = match server_result {
        Some(result) => result,
        None => server.await,
    };
    cancel.cancel();
    worker.await.context("机器人控制任务异常结束")?;
    result.context("管理员页面任务异常结束")??;
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
