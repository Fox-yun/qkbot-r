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
    "roll",
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
            "roll",
            "随机选择",
            "/roll 选项1 选项2 …；按空白分隔选项，随机抽取一项并引用原消息回复",
            &["/roll"],
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
            "聊天图片",
            "QQ号【昵称】说内容+QQ号说内容；支持头像、群名片和图片消息",
            &["/mock-image"],
            false,
        ),
        (
            "mock_forward",
            "合并转发",
            "QQ号说内容|QQ号说内容；单条用 /mock-forward；按联系人名称展示",
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
        "转发分隔符或保护名单无效"
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

// Help text and images share one structured document. Do not split the text
// fallback into menu lines: doing so numbers descriptions as new features.
fn help_document(
    config: &Config,
    db: &Connection,
    message: &crate::types::Incoming,
) -> Result<crate::plugins::render::HelpDocument> {
    use crate::plugins::render::{HelpCard, HelpDocument};
    let args: Vec<_> = message.text.split_whitespace().skip(1).collect();
    let show_hidden = args.iter().any(|s| matches!(*s, "-H" | "--show-hidden"));
    ensure!(
        !show_hidden
            || config
                .plugins
                .native
                .superusers
                .contains(&message.sender_id),
        "只有机器人超级管理员可以查看隐藏功能"
    );
    let queries: Vec<_> = args
        .into_iter()
        .filter(|s| !matches!(*s, "-H" | "--show-hidden"))
        .collect();
    let scope = &message.key.scope;
    let mut entries = Vec::new();
    for feature in features(config) {
        let scope_enabled = allowed_revision(db, &scope.key(), &feature.id)?.is_some();
        let group_applicable = !feature.group_only || scope.scene == crate::types::Scene::Group;
        if !show_hidden && !(feature.enabled && scope_enabled && group_applicable) {
            continue;
        }
        let mut badges = Vec::new();
        if !feature.enabled {
            badges.push("已关闭".to_owned());
        }
        if !scope_enabled {
            badges.push("本会话关闭".to_owned());
        }
        if feature.group_only {
            badges.push(
                if group_applicable {
                    "群聊"
                } else {
                    "仅限群聊"
                }
                .to_owned(),
            );
        }
        entries.push((feature, badges));
    }
    let mut document = HelpDocument {
        title: "小鱼 · 功能帮助".into(),
        subtitle: if show_hidden {
            format!("管理员视图 · {} 项功能，含未启用项", entries.len())
        } else {
            format!("当前会话可用 · {} 项功能", entries.len())
        },
        cards: Vec::new(),
        footer: Vec::new(),
        detail: !queries.is_empty(),
    };
    if let Some(query) = queries.first() {
        let found = if query.bytes().all(|c| c.is_ascii_digit()) {
            query
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|i| entries.get(i).map(|entry| (i, entry)))
        } else {
            entries.iter().enumerate().find(|(_, (f, _))| {
                format!("{} {} {}", f.id, f.name, f.commands.join(" "))
                    .to_lowercase()
                    .contains(&query.to_lowercase())
            })
        };
        let Some((index, (feature, badges))) = found else {
            document.subtitle = "未找到对应功能。发送 /帮助 查看当前列表。".into();
            return Ok(document);
        };
        document.title = format!("{} · 使用指南", feature.name);
        document.subtitle = "功能详情 · 查看说明与完整命令入口".into();
        document.cards.push(HelpCard {
            number: Some(index + 1),
            title: feature.name.clone(),
            description: feature.description.clone(),
            commands: feature
                .commands
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{}. {c}", i + 1))
                .collect(),
            badges: badges.clone(),
        });
        if let Some(function) = queries.get(1) {
            let command = if function.bytes().all(|c| c.is_ascii_digit()) {
                function
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|i| feature.commands.get(i))
            } else {
                feature.commands.iter().find(|c| c.contains(*function))
            };
            document.footer.push(
                command
                    .map(|c| format!("入口：{c}；发送该命令查看用法。"))
                    .unwrap_or_else(|| "没有找到该命令入口。".into()),
            );
        }
        document
            .footer
            .push("/帮助 返回目录；/帮助 功能序号 命令序号 查看入口。".into());
    } else {
        for (index, (feature, badges)) in entries.into_iter().enumerate() {
            document.cards.push(HelpCard {
                number: Some(index + 1),
                title: feature.name,
                description: feature.description,
                commands: feature.commands.into_iter().take(2).collect(),
                badges,
            });
        }
        document
            .footer
            .push("/帮助 序号或名称 查看详情；可再加命令序号。".into());
        document
            .footer
            .push("卡片展示常用入口；详情中可查看全部命令。".into());
        let mut shortcuts = Vec::new();
        for (id, enabled, usage) in [
            ("clock", config.plugins.clock.enabled, "/clock 查看时间"),
            (
                "reminder",
                config.plugins.reminder.enabled,
                "/remind 管理提醒",
            ),
        ] {
            if enabled && allowed_revision(db, &scope.key(), id)?.is_some() {
                shortcuts.push(usage);
            }
        }
        if !shortcuts.is_empty() {
            document.footer.push(shortcuts.join("    ·    "));
        }
        if config.plugins.ai.enabled && allowed_revision(db, &scope.key(), "ai")?.is_some() {
            document.footer.push(
                match scope.scene {
                    crate::types::Scene::Group => "聊天：@机器人，或发送带触发词的消息。",
                    crate::types::Scene::Friend => "聊天：直接发送消息即可。",
                }
                .into(),
            );
        }
    }
    Ok(document)
}

fn help_text(document: &crate::plugins::render::HelpDocument) -> String {
    let mut blocks = vec![document.title.clone(), document.subtitle.clone()];
    for card in &document.cards {
        let number = card.number.map(|n| format!("{n}. ")).unwrap_or_default();
        let badges = if card.badges.is_empty() {
            String::new()
        } else {
            format!("（{}）", card.badges.join(" · "))
        };
        let mut block = format!("{number}{}{badges}\n{}", card.title, card.description);
        if !card.commands.is_empty() {
            block.push_str("\n命令：");
            block.push_str(
                &card
                    .commands
                    .join(if document.detail { "\n" } else { "、" }),
            );
        }
        blocks.push(block);
    }
    blocks.extend(document.footer.iter().cloned());
    blocks.join("\n\n")
}

pub struct HelpPlugin;
impl HelpPlugin {
    async fn document(
        &self,
        host: &crate::host::Host,
        message: &crate::types::Incoming,
    ) -> Result<crate::plugins::render::HelpDocument> {
        let config = host.config.clone();
        let message = message.clone();
        host.store
            .call(move |db| help_document(&config, db, &message))
            .await
    }
}
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
        Ok(help_text(&self.document(host, message).await?))
    }
    async fn reply(
        &self,
        host: &crate::host::Host,
        message: &crate::types::Incoming,
    ) -> Result<crate::types::OutgoingMessage> {
        let document = self.document(host, message).await?;
        let text = help_text(&document);
        match crate::plugins::render::help(host, document).await {
            Ok(image) => Ok(image),
            Err(error) => {
                tracing::debug!(%error,"image help unavailable; preserving text help");
                Ok(crate::types::OutgoingMessage::text(text))
            }
        }
    }
}

#[cfg(test)]
mod help_tests {
    use super::*;
    use crate::{
        plugin::Plugin,
        types::{Incoming, MessageKey, Scene},
    };

    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE scope_plugin_access(
                scope TEXT NOT NULL, plugin TEXT NOT NULL, enabled INTEGER NOT NULL,
                revision INTEGER NOT NULL, PRIMARY KEY(scope,plugin));",
        )
        .unwrap();
        db
    }
    fn incoming(text: &str) -> Incoming {
        Incoming {
            key: MessageKey {
                scope: Scope {
                    bot_id: 123,
                    scene: Scene::Group,
                    peer_id: 456,
                },
                message_seq: 1,
            },
            sender_id: 789,
            sender_nickname: None,
            sender_card: None,
            text: text.into(),
            mentions: Vec::new(),
            quote_seq: None,
            received_at: crate::types::now(),
            raw_message: serde_json::Value::Null,
            notification: None,
        }
    }
    fn disable(db: &Connection, message: &Incoming, plugin: &str) {
        db.execute(
            "INSERT INTO scope_plugin_access VALUES (?1,?2,0,1)",
            params![message.key.scope.key(), plugin],
        )
        .unwrap();
    }

    #[test]
    fn help_numbers_identify_whole_features_and_registered_commands() {
        let db = database();
        let config = Config::default();
        let mut message = incoming("/帮助");
        disable(&db, &message, "wordle");
        let menu = help_document(&config, &db, &message).unwrap();
        assert!(!menu.detail);
        assert!(!menu.cards.iter().any(|c| c.title == "Wordle"));
        for (index, card) in menu.cards.iter().enumerate() {
            assert_eq!(card.number, Some(index + 1));
            assert!(!card.title.starts_with(&format!("{}. ", index + 1)));
            assert!(!card.description.is_empty());
            message.text = format!("/帮助 {}", index + 1);
            let detail = help_document(&config, &db, &message).unwrap();
            assert_eq!(detail.cards.len(), 1);
            assert_eq!(detail.cards[0].title, card.title);
            assert_eq!(detail.cards[0].description, card.description);
            assert_eq!(detail.cards[0].number, card.number);
        }
        message.text = "/菜单 鹿管签到".into();
        let detail = help_document(&config, &db, &message).unwrap();
        let feature = features(&config)
            .into_iter()
            .find(|f| f.id == "checkin")
            .unwrap();
        assert!(feature.commands.len() > 2);
        assert_eq!(
            detail.cards[0].commands,
            feature
                .commands
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{}. {c}", i + 1))
                .collect::<Vec<_>>()
        );
        message.text = "/help /鹿 4".into();
        let command = help_document(&config, &db, &message).unwrap();
        assert!(command.footer[0].contains(&feature.commands[3]));
        assert!(help_text(&detail).contains("命令：1. /checkin"));
        assert_eq!(
            HelpPlugin.spec().commands,
            &["/help", "/帮助", "/菜单", "帮助", "菜单"]
        );
    }

    #[test]
    fn help_visibility_is_account_and_scope_local_and_hidden_requires_superuser() {
        let db = database();
        let mut config = Config::default();
        config.plugins.reminder.enabled = true;
        config.plugins.native.superusers = vec![789];
        let mut message = incoming("/帮助");
        for plugin in ["wordle", "clock", "reminder"] {
            disable(&db, &message, plugin);
        }
        let menu = help_document(&config, &db, &message).unwrap();
        assert!(!menu.cards.iter().any(|c| c.title == "Wordle"));
        assert!(
            !menu
                .footer
                .iter()
                .any(|f| f.contains("/clock") || f.contains("/remind"))
        );
        for scope in [
            Scope {
                bot_id: 999,
                ..message.key.scope.clone()
            },
            Scope {
                peer_id: 999,
                ..message.key.scope.clone()
            },
            Scope {
                scene: Scene::Friend,
                ..message.key.scope.clone()
            },
        ] {
            let mut other = message.clone();
            other.key.scope = scope;
            let menu = help_document(&config, &db, &other).unwrap();
            assert!(menu.cards.iter().any(|c| c.title == "Wordle"));
            assert!(menu.footer.iter().any(|f| f.contains("/clock")));
            if other.key.scope.scene == Scene::Friend {
                assert!(!menu.cards.iter().any(|c| c.title == "鹿管签到"));
            }
        }
        message.text = "/帮助 -H".into();
        message.sender_id = 555;
        assert!(help_document(&config, &db, &message).is_err());
        message.sender_id = 789;
        let hidden = help_document(&config, &db, &message).unwrap();
        assert!(
            hidden
                .cards
                .iter()
                .find(|c| c.title == "Wordle")
                .unwrap()
                .badges
                .contains(&"本会话关闭".into())
        );
        assert!(
            hidden
                .cards
                .iter()
                .find(|c| c.title == "戳一戳")
                .unwrap()
                .badges
                .contains(&"已关闭".into())
        );
        message.key.scope.scene = Scene::Friend;
        message.text = "/帮助 鹿管签到 --show-hidden".into();
        let hidden = help_document(&config, &db, &message).unwrap();
        assert!(hidden.cards[0].badges.contains(&"仅限群聊".into()));
    }

    #[test]
    fn help_numeric_errors_do_not_fall_through_to_substring_matches() {
        let db = database();
        let mut config = Config::default();
        config.nonebot.enabled = true;
        config.nonebot.plugins = vec![crate::config::ExternalPluginConfig {
            id: "nb_tool0".into(),
            name: "扩展0".into(),
            commands: vec!["/tool0".into(), "/tool999".into()],
            enabled: true,
            ..Default::default()
        }];
        for number in ["0", "999", "99999999999999999999999999999999999999"] {
            let message = incoming(&format!("/帮助 {number}"));
            let document = help_document(&config, &db, &message).unwrap();
            assert!(document.cards.is_empty());
            assert!(help_text(&document).contains("未找到对应功能"));
            let message = incoming(&format!("/帮助 nb_tool0 {number}"));
            let document = help_document(&config, &db, &message).unwrap();
            assert_eq!(document.cards.len(), 1);
            assert_eq!(document.footer[0], "没有找到该命令入口。");
        }
        let message = incoming("/帮助 NB_TOOL0 tool999");
        let document = help_document(&config, &db, &message).unwrap();
        assert!(document.footer[0].contains("入口：/tool999"));
    }

    #[tokio::test]
    async fn help_missing_font_preserves_complete_text_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.plugins.ai.enabled = false;
        config.plugins.native.enabled = vec!["help".into(), "checkin".into()];
        config.plugins.native.image_font_path = temp
            .path()
            .join("missing-font.ttf")
            .to_string_lossy()
            .into();
        config.app.database = temp.path().join("test.sqlite3").to_string_lossy().into();
        config.nonebot.media_dir = temp.path().join("media").to_string_lossy().into();
        let host = crate::host::Host::create(config).await.unwrap();
        let message = incoming("/帮助 鹿管签到");
        let expected = HelpPlugin.command(&host, &message).await.unwrap();
        let reply = HelpPlugin.reply(&host, &message).await.unwrap();
        assert_eq!(reply, crate::types::OutgoingMessage::text(expected.clone()));
        assert!(expected.contains("/禁鹿"));
        assert!(expected.contains("/鹿帮助"));
        host.cancel.cancel();
        host.store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn help_reply_publishes_png_pages_from_real_catalog() {
        let font = std::env::var("QKBOT_TEST_FONT").ok().or_else(|| {
            [
                "C:/Windows/Fonts/msyh.ttc",
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            ]
            .into_iter()
            .find(|p| std::path::Path::new(p).is_file())
            .map(str::to_owned)
        });
        let Some(font) = font else {
            eprintln!("help image integration skipped: no CJK font");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.plugins.ai.enabled = false;
        config.plugins.native.image_font_path = font;
        config.app.database = temp.path().join("test.sqlite3").to_string_lossy().into();
        config.nonebot.media_dir = temp.path().join("media").to_string_lossy().into();
        let host = crate::host::Host::create(config).await.unwrap();
        let reply = HelpPlugin.reply(&host, &incoming("/帮助")).await.unwrap();
        assert!(!reply.segments.is_empty());
        assert!(reply.segments.iter().all(|segment| matches!(segment,
            crate::types::OutgoingSegment::Image { uri, .. } if uri.contains("/native-media/")
        )));
        let files = std::fs::read_dir(temp.path().join("media/native/temporary"))
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), reply.segments.len());
        for file in files {
            let bytes = std::fs::read(file.path()).unwrap();
            let image = image::load_from_memory(&bytes).unwrap();
            assert_eq!(image.width(), 1080);
            assert!(image.height() <= 2200);
        }
        host.cancel.cancel();
        host.store.stop().await.unwrap();
    }
}
