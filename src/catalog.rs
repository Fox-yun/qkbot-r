//! One lightweight feature catalog and durable per-chat switches for both runtimes.
use crate::{config::Config, store::Store, types::Scope};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::collections::BTreeSet;

pub(crate) const MIGRATION: &str = r#"
CREATE TABLE scope_plugin_access(
 scope TEXT NOT NULL, plugin TEXT NOT NULL, enabled INTEGER NOT NULL,
 revision INTEGER NOT NULL, PRIMARY KEY(scope,plugin)
);
ALTER TABLE messages ADD COLUMN command_plugin TEXT;
ALTER TABLE messages ADD COLUMN plugin_revision INTEGER NOT NULL DEFAULT 0;
"#;

#[derive(Clone, Serialize)]
pub struct Feature {
    pub id: String,
    pub name: String,
    pub description: String,
    pub runtime: String,
    pub commands: Vec<String>,
    pub group_only: bool,
    pub enabled: bool,
}
pub const NATIVE_IDS: &[&str] = &[
    "help",
    "checkin",
    "wordle",
    "kfc",
    "meal",
    "daily_partner",
    "mc_status",
    "sticker_save",
    "nudge",
    "mock_chat_image",
    "mock_forward",
    "mute_roulette",
];
pub fn features(config: &Config) -> Vec<Feature> {
    let definitions: &[(&str, &str, &str, &[&str], bool)] = &[
        (
            "help",
            "帮助",
            "查看当前会话可用功能及用法",
            &["/help", "/帮助"],
            false,
        ),
        (
            "checkin",
            "鹿管签到",
            "鹿/帮鹿/补鹿、次数、帮助开关、禁签、月历和群排行；默认北京时间",
            &["/鹿", "/鹿管", "/补鹿", "/鹿管排行榜"],
            true,
        ),
        (
            "wordle",
            "Wordle",
            "/wordle [3–8词长] [词库]；直接发单词猜测；/提示；/结束。尝试次数=词长+1，300秒续期",
            &["/wordle", "/猜词"],
            false,
        ),
        (
            "kfc",
            "疯狂星期四",
            "原版文案库；支持原星期关键词及文案中的星期替换",
            &["/kfc", "/疯狂星期四"],
            false,
        ),
        (
            "meal",
            "今天吃什么",
            "吃什么/喝什么发送实际图片；查看菜单；超级管理员可增删食物和饮料",
            &["/meal", "/吃什么", "/喝什么"],
            false,
        ),
        (
            "daily_partner",
            "今日老婆",
            "每日群成员抽取、换人次数、活跃成员模式、双向配对和可选自动撤回",
            &["/partner", "/今日老婆"],
            true,
        ),
        (
            "mc_status",
            "MC 状态",
            "/信息 查询本群绑定服务器；Java/Bedrock；/mc bind 名称绑定Web已配置目标",
            &["/mc", "/信息"],
            true,
        ),
        (
            "sticker_save",
            "表情保存",
            "引用一条带图片的消息，再发送 /sticker",
            &["/sticker", "/保存表情"],
            false,
        ),
        (
            "nudge",
            "戳一戳",
            "仅响应别人对机器人的真实戳一戳，带冷却",
            &[],
            false,
        ),
        (
            "mock_chat_image",
            "模拟聊天图片",
            "QQ号【昵称】说内容+QQ号说内容；支持真实图片；输出明确标记模拟",
            &["/mock-image"],
            false,
        ),
        (
            "mock_forward",
            "模拟转发",
            "QQ号说内容|QQ号说内容；单条用 /mock-forward；输出明确标记模拟",
            &["/mock-forward"],
            false,
        ),
        (
            "mute_roulette",
            "轮盘禁言",
            "共享六发一颗子弹，命中禁言开枪者2–6分钟；群管理员可禁言及解禁真实@成员",
            &["/roulette", "开枪"],
            true,
        ),
    ];
    let mut entries: Vec<_> = definitions
        .iter()
        .map(|(id, name, description, commands, group_only)| Feature {
            id: (*id).into(),
            name: (*name).into(),
            description: (*description).into(),
            runtime: "rust".into(),
            commands: commands.iter().map(|s| (*s).into()).collect(),
            group_only: *group_only,
            enabled: config.plugins.native.enabled.iter().any(|s| s == id),
        })
        .collect();
    entries.extend(config.nonebot.plugins.iter().map(|p| Feature {
        id: p.id.clone(),
        name: p.name.clone(),
        description: p.description.clone(),
        runtime: "nonebot".into(),
        commands: p.commands.clone(),
        group_only: false,
        enabled: config.nonebot.enabled && p.enabled,
    }));
    // Actual command declarations are authoritative; UI/help never maintain a
    // second hand-written alias list that drifts from the handlers.
    if let Ok((registry, _, _)) = crate::plugins::assemble(config) {
        for entry in &mut entries {
            if let Some(spec) = registry.specs().find(|spec| spec.id == entry.id) {
                entry.commands = spec.commands.iter().map(|s| (*s).into()).collect();
            }
        }
    }
    entries
}

pub fn validate_config(config: &Config) -> Result<()> {
    let native = &config.plugins.native;
    ensure!(
        native.enabled.len() <= NATIVE_IDS.len(),
        "原生插件启用项过多"
    );
    let mut ids = BTreeSet::new();
    for id in &native.enabled {
        ensure!(
            NATIVE_IDS.contains(&id.as_str()) && ids.insert(id),
            "原生插件名称无效或重复"
        );
    }
    ensure!(
        native.meal_items.len() <= 128
            && native
                .meal_items
                .iter()
                .all(|s| !s.trim().is_empty() && s.chars().count() <= 80),
        "食物条目最多128项，每项1到80字符"
    );
    ensure!(
        native.meal_cooldown_secs <= 3600 && native.meal_daily_limit <= 10000,
        "食物冷却或每日次数无效"
    );
    ensure!(native.mc_servers.len() <= 32, "最多配置32个MC服务器");
    let mut names = BTreeSet::new();
    for server in &native.mc_servers {
        ensure!(
            !server.name.is_empty()
                && server.name.len() <= 64
                && !server.name.chars().any(char::is_whitespace)
                && names.insert(&server.name),
            "MC服务器名称无效或重复"
        );
        ensure!(
            !server.host.is_empty()
                && server.host.len() <= 253
                && !server.host.contains(['/', '\\', '@', '?', '#'])
                && !server.host.chars().any(char::is_whitespace)
                && server.port > 0,
            "MC服务器地址无效"
        );
        ensure!(
            ["java", "bedrock"].contains(&server.kind.as_str())
                && server.group_ids.len() <= 256
                && server.group_ids.iter().all(|id| *id > 0),
            "MC类型需为java/bedrock，绑定群号必须为正数且最多256项"
        );
    }
    let poke = &native.poke;
    ensure!(
        !poke.nickname.trim().is_empty()
            && poke.nickname.chars().count() <= 64
            && !poke.nickname.chars().any(char::is_control),
        "戳一戳昵称无效"
    );
    ensure!(
        !poke.resource_dir.is_empty()
            && poke.resource_dir.len() <= 1024
            && !poke.resource_dir.chars().any(char::is_control),
        "戳一戳资源目录无效"
    );
    ensure!(
        poke.blocked_groups.len() <= 256
            && poke.allowed_groups.len() <= 256
            && poke
                .blocked_groups
                .iter()
                .chain(&poke.allowed_groups)
                .all(|id| *id > 0)
            && (1..=3600).contains(&poke.cooldown_secs),
        "戳一戳名单或冷却配置无效"
    );
    let fake = &native.fakemsg;
    ensure!(
        !fake.user_split.is_empty()
            && fake.user_split.chars().count() <= 8
            && !fake.message_split.is_empty()
            && fake.message_split.chars().count() <= 8
            && fake.user_split != fake.message_split
            && !fake
                .user_split
                .chars()
                .chain(fake.message_split.chars())
                .any(char::is_control)
            && fake.protected_ids.len() <= 256
            && fake.protected_ids.iter().all(|id| *id > 0),
        "模拟转发分隔符或保护名单无效"
    );
    ensure!(
        native.image_font_path.len() <= 1024 && (1..=3600).contains(&native.mute_max_secs),
        "绘图或禁言配置无效"
    );
    ensure!(
        native.superusers.len() <= 128 && native.superusers.iter().all(|id| *id > 0),
        "超级管理员ID无效"
    );
    let waifu = &native.today_waifu;
    ensure!(
        waifu.change_limit <= 100
            && (1..=300).contains(&waifu.recall_delay_secs)
            && (1..=365).contains(&waifu.active_days)
            && ["random", "active"].contains(&waifu.select_mode.as_str())
            && waifu.excluded_ids.len() <= 1000
            && waifu.excluded_ids.iter().all(|id| *id > 0),
        "今日老婆配置无效"
    );
    let bridge = &config.nonebot;
    let address: std::net::SocketAddr = bridge
        .listen
        .parse()
        .map_err(|_| anyhow::anyhow!("NoneBot地址必须为本机IP:端口"))?;
    ensure!(
        address.ip().is_loopback() && address.port() > 0,
        "NoneBot入口仅监听本机地址"
    );
    ensure!(
        (1..=128).contains(&bridge.queue_capacity)
            && (10..=600).contains(&bridge.task_timeout_secs),
        "NoneBot队列或任务期限无效"
    );
    ensure!(
        (1024..=1024 * 1024 * 1024).contains(&bridge.media_max_bytes)
            && !bridge.media_dir.is_empty()
            && bridge.media_dir.len() <= 1024,
        "NoneBot媒体限制须为1 KiB到1 GiB，目录不能为空"
    );
    ensure!(
        bridge.token.is_empty() || (32..=4096).contains(&bridge.token.len()),
        "NoneBot令牌至少32字节"
    );
    reqwest::header::HeaderValue::from_str(&bridge.token)
        .map_err(|_| anyhow::anyhow!("NoneBot令牌格式无效"))?;
    ensure!(bridge.plugins.len() <= 32, "最多配置32个NoneBot插件");
    let mut external = BTreeSet::new();
    let mut commands: BTreeSet<String> = ["/bot", "/clock", "/reminder"].map(str::to_owned).into();
    for feature in features(config).into_iter().filter(|f| f.runtime == "rust") {
        commands.extend(feature.commands);
    }
    for plugin in &bridge.plugins {
        ensure!(
            plugin.id.starts_with("nb_")
                && plugin.id.len() <= 64
                && plugin
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && external.insert(&plugin.id),
            "NoneBot插件ID必须为唯一的nb_标识"
        );
        ensure!(
            !plugin.module.is_empty()
                && plugin.module.len() <= 128
                && plugin
                    .module
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.'),
            "NoneBot模块名称无效"
        );
        ensure!(
            !plugin.name.trim().is_empty()
                && plugin.name.len() <= 120
                && plugin.description.len() <= 800,
            "NoneBot插件说明过长或为空"
        );
        ensure!(
            plugin.commands.len() <= 32
                && plugin.url_hosts.len() <= 32
                && plugin.patterns.len() <= 8
                && (!plugin.commands.is_empty()
                    || !plugin.url_hosts.is_empty()
                    || !plugin.patterns.is_empty()),
            "NoneBot插件需设置有界触发命令、正则或域名"
        );
        for command in &plugin.commands {
            ensure!(
                !command.is_empty()
                    && command.len() <= 64
                    && !command.chars().any(char::is_whitespace)
                    && commands.insert(command.clone()),
                "插件命令无效或冲突"
            );
        }
        for pattern in &plugin.patterns {
            ensure!(
                pattern.starts_with('^') && pattern.len() <= 512,
                "插件匹配表达式需以^锚定且最多512字节"
            );
            regex::RegexBuilder::new(pattern)
                .size_limit(256 * 1024)
                .build()
                .map_err(|_| anyhow::anyhow!("插件匹配表达式无效或过于复杂"))?;
        }
        for host in &plugin.url_hosts {
            ensure!(
                !host.is_empty()
                    && host.len() <= 253
                    && host
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'),
                "NoneBot触发域名无效"
            );
        }
    }
    Ok(())
}

pub fn allowed_revision(db: &Connection, scope: &str, plugin: &str) -> Result<Option<i64>> {
    let row: Option<(bool, i64)> = db
        .query_row(
            "SELECT enabled,revision FROM scope_plugin_access WHERE scope=?1 AND plugin=?2",
            params![scope, plugin],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match row {
        None => Some(0),
        Some((true, revision)) => Some(revision),
        Some((false, _)) => None,
    })
}
pub fn check_revision(db: &Connection, scope: &str, plugin: &str, expected: i64) -> Result<()> {
    ensure!(
        allowed_revision(db, scope, plugin)? == Some(expected),
        "此会话的插件已关闭或任务已失效"
    );
    Ok(())
}
pub fn check_message(db: &Connection, source_key: &str, plugin: &str) -> Result<()> {
    let (scope, owner, revision): (String, Option<String>, i64) = db.query_row(
        "SELECT scope,command_plugin,plugin_revision FROM messages WHERE key=?1",
        [source_key],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    ensure!(owner.as_deref() == Some(plugin), "插件任务来源不匹配");
    crate::access::message_revision(db, &scope, source_key)?;
    check_revision(db, &scope, plugin, revision)
}
pub async fn set_enabled(store: &Store, scope: Scope, plugin: String, enabled: bool) -> Result<()> {
    ensure!(scope.bot_id > 0 && scope.peer_id > 0, "会话ID无效");
    let _gate = store.scope_gate(&scope.key())?.write_owned().await;
    store.call(move|db| {
        let tx=db.transaction()?;
        if allowed_revision(&tx,&scope.key(),&plugin)?.is_some()!=enabled {
            tx.execute("INSERT INTO scope_plugin_access(scope,plugin,enabled,revision) VALUES(?1,?2,?3,1) ON CONFLICT(scope,plugin) DO UPDATE SET enabled=excluded.enabled,revision=scope_plugin_access.revision+1",params![scope.key(),plugin,enabled])?;
        }
        tx.commit()?;Ok(())
    }).await
}

pub struct HelpPlugin;
#[async_trait::async_trait]
impl crate::plugin::Plugin for HelpPlugin {
    fn spec(&self) -> crate::plugin::PluginSpec {
        crate::plugin::PluginSpec {
            id: "help",
            commands: &["/help", "/帮助", "/菜单", "帮助", "菜单"],
            tools: &[],
            dependencies: &["host"],
        }
    }
    async fn command(
        &self,
        host: &crate::host::Host,
        message: &crate::types::Incoming,
    ) -> Result<String> {
        let args: Vec<_> = message
            .text
            .split_whitespace()
            .skip(1)
            .map(str::to_owned)
            .collect();
        let show_hidden = args.iter().any(|s| s == "-H" || s == "--show-hidden");
        ensure!(
            !show_hidden
                || host
                    .config
                    .plugins
                    .native
                    .superusers
                    .contains(&message.sender_id),
            "只有机器人超级管理员可以查看隐藏功能"
        );
        let queries: Vec<_> = args
            .into_iter()
            .filter(|s| s != "-H" && s != "--show-hidden")
            .collect();
        let scope = message.key.scope.clone();
        let entries = features(&host.config);
        let clock = host.config.plugins.clock.enabled;
        let reminder = host.config.plugins.reminder.enabled;
        host.store
            .call(move |db| {
                let entries: Vec<_> = entries
                    .into_iter()
                    .filter(|f| {
                        show_hidden
                            || (f.enabled
                                && (!f.group_only || scope.scene == crate::types::Scene::Group)
                                && allowed_revision(db, &scope.key(), &f.id)
                                    .ok()
                                    .flatten()
                                    .is_some())
                    })
                    .collect();
                let mut lines = vec!["小鱼 · 功能帮助".to_owned()];
                if let Some(query) = queries.first() {
                    let found = query
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .and_then(|i| entries.get(i))
                        .or_else(|| {
                            entries.iter().find(|f| {
                                format!("{} {} {}", f.id, f.name, f.commands.join(" "))
                                    .to_lowercase()
                                    .contains(&query.to_lowercase())
                            })
                        });
                    let Some(feature) = found else {
                        return Ok("未找到对应功能。发送 /帮助 查看当前列表。".into());
                    };
                    lines.push(format!(
                        "{}\n{}\n命令：{}",
                        feature.name,
                        feature.description,
                        feature.commands.join("、")
                    ));
                    if let Some(function) = queries.get(1) {
                        let command = function
                            .parse::<usize>()
                            .ok()
                            .and_then(|n| n.checked_sub(1))
                            .and_then(|i| feature.commands.get(i))
                            .or_else(|| feature.commands.iter().find(|c| c.contains(function)));
                        lines.push(
                            command
                                .map(|c| format!("入口：{c}；发送该命令查看用法。"))
                                .unwrap_or_else(|| "没有找到该命令入口。".into()),
                        );
                    }
                } else {
                    for (i, f) in entries.iter().enumerate() {
                        lines.push(format!(
                            "{}. {}{}\n{}",
                            i + 1,
                            f.name,
                            if f.enabled { "" } else { "（已关闭）" },
                            f.description
                        ));
                    }
                    if clock {
                        lines.push("/clock 查看时间".into());
                    }
                    if reminder {
                        lines.push("/remind 管理提醒".into());
                    }
                    lines.push("/帮助 序号或名称 查看详情；可再加命令序号。".into());
                }
                Ok(lines.join("\n\n"))
            })
            .await
    }
    async fn reply(
        &self,
        host: &crate::host::Host,
        message: &crate::types::Incoming,
    ) -> Result<crate::types::OutgoingMessage> {
        let text = self.command(host, message).await?;
        let lines = text
            .lines()
            .filter(|s| !s.is_empty() && *s != "小鱼 · 功能帮助")
            .map(str::to_owned)
            .collect::<Vec<_>>();
        match crate::plugins::render::lines(host, "小鱼 · 功能帮助", &lines).await {
            Ok(image) => Ok(image),
            Err(error) => {
                tracing::debug!(%error,"image help unavailable; preserving text help");
                Ok(crate::types::OutgoingMessage::text(text))
            }
        }
    }
}
