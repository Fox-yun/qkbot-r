//! Daily partner rules follow today-waifu's published state machine. Identities,
//! opt-outs and state are scoped by bot + group; names never identify a person.
use crate::{
    config::{McServerConfig, TodayWaifuConfig},
    host::Host,
    milky::GroupMember,
    plugin::{Plugin, PluginSpec},
    types::{ImageType, Incoming, MessageKey, OutgoingMessage, OutgoingSegment, Scene, now},
};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use chrono::TimeZone;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MIGRATION: &str = r#"
CREATE TABLE daily_partner_settings(scope TEXT PRIMARY KEY, settings TEXT NOT NULL);
CREATE TABLE daily_partner_choices(
 scope TEXT NOT NULL, day TEXT NOT NULL, actor_id INTEGER NOT NULL,
 partner_id INTEGER NOT NULL, draws INTEGER NOT NULL, lost INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(scope,day,actor_id)
);
CREATE INDEX daily_partner_taken ON daily_partner_choices(scope,day,partner_id);
CREATE TABLE daily_partner_optout(
 scope TEXT NOT NULL, user_id INTEGER NOT NULL, PRIMARY KEY(scope,user_id)
);
CREATE TABLE daily_partner_active(
 scope TEXT NOT NULL, user_id INTEGER NOT NULL, last_at INTEGER NOT NULL,
 PRIMARY KEY(scope,user_id)
);
"#;

pub struct DailyPartnerPlugin;

const PARTNER_COMMANDS: &[&str] = &[
    "/partner",
    "/今日老婆",
    "今日老婆",
    "换老婆",
    "今日老婆信息",
    "今日老婆帮助",
    "刷新今日老婆",
    "重置今日老婆",
    "开启换老婆",
    "关闭换老婆",
    "设置换老婆次数",
    "开启自动撤回",
    "关闭自动撤回",
    "设置自动撤回延迟",
    "开启自动设置对方老婆",
    "关闭自动设置对方老婆",
    "设置抽取模式",
    "设置活跃天数",
    "退出今日老婆",
    "参与今日老婆",
];

#[derive(Debug, PartialEq, Eq)]
enum PartnerCommand {
    Draw,
    Change,
    Info,
    Help,
    Reset,
    Leave,
    Join,
    AllowChange(bool),
    ChangeLimit(u32),
    AutoRecall(bool),
    RecallDelay(u64),
    AutoPair(bool),
    Mode(String),
    ActiveDays(u32),
}

fn partner_command(text: &str) -> Option<PartnerCommand> {
    let text = text.trim();
    Some(match text {
        "/partner" | "/今日老婆" | "今日老婆" => PartnerCommand::Draw,
        "/partner change" | "换老婆" => PartnerCommand::Change,
        "/partner info" | "今日老婆信息" => PartnerCommand::Info,
        "/partner help" | "今日老婆帮助" => PartnerCommand::Help,
        "/partner reset" | "刷新今日老婆" | "重置今日老婆" => PartnerCommand::Reset,
        "/partner leave" | "退出今日老婆" => PartnerCommand::Leave,
        "/partner join" | "参与今日老婆" => PartnerCommand::Join,
        "开启换老婆" => PartnerCommand::AllowChange(true),
        "关闭换老婆" => PartnerCommand::AllowChange(false),
        "开启自动撤回" => PartnerCommand::AutoRecall(true),
        "关闭自动撤回" => PartnerCommand::AutoRecall(false),
        "开启自动设置对方老婆" => PartnerCommand::AutoPair(true),
        "关闭自动设置对方老婆" => PartnerCommand::AutoPair(false),
        _ if text.starts_with("设置换老婆次数") => {
            PartnerCommand::ChangeLimit(text.strip_prefix("设置换老婆次数")?.trim().parse().ok()?)
        }
        _ if text.starts_with("设置自动撤回延迟") => {
            PartnerCommand::RecallDelay(text.strip_prefix("设置自动撤回延迟")?.trim().parse().ok()?)
        }
        _ if text.starts_with("设置活跃天数") => {
            PartnerCommand::ActiveDays(text.strip_prefix("设置活跃天数")?.trim().parse().ok()?)
        }
        _ if text.starts_with("设置抽取模式") => {
            PartnerCommand::Mode(match text.strip_prefix("设置抽取模式")?.trim() {
                "随机模式" | "随机" => "random".into(),
                "活跃模式" | "活跃" => "active".into(),
                _ => return None,
            })
        }
        _ => return None,
    })
}

pub fn claims_partner(message: &Incoming) -> bool {
    message.notification.is_none()
        && (partner_command(&message.text).is_some()
            || matches!(
                message.text.split_whitespace().next(),
                Some("/partner" | "/今日老婆")
            ))
}

#[async_trait]
impl Plugin for DailyPartnerPlugin {
    fn spec(&self) -> PluginSpec {
        PluginSpec {
            id: "daily_partner",
            commands: PARTNER_COMMANDS,
            tools: &[],
            dependencies: &["host"],
        }
    }
    fn accepts(&self, message: &Incoming) -> bool {
        message.key.scope.scene == Scene::Group
    }
    fn claims(&self, message: &Incoming) -> bool {
        claims_partner(message)
    }
    async fn init(&self, host: &Host) -> Result<()> {
        host.store.migrate("daily_partner", 1, MIGRATION).await
    }
    async fn command(&self, host: &Host, message: &Incoming) -> Result<String> {
        Ok(partner_reply(host, message).await?.text_content())
    }
    async fn reply(&self, host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
        partner_reply(host, message).await
    }
    async fn after_send(
        &self,
        host: &Arc<Host>,
        message: &Incoming,
        sent: &MessageKey,
    ) -> Result<()> {
        let Some(delay) = partner_recall_delay(host, message).await? else {
            return Ok(());
        };
        let owner = host.clone();
        let key = message.key.key();
        let sent = sent.clone();
        let epoch = host
            .epochs
            .get("daily_partner")
            .copied()
            .context("plugin unavailable")?;
        host.tasks.spawn(async move {
            tokio::select! {_=owner.cancel.cancelled()=>return,_=tokio::time::sleep(delay)=>{}}
            let result = async {
                let _gate = owner.access.delivery_guard(&sent.scope).await?;
                let scope = sent.scope.key();
                owner
                    .store
                    .call(move |db| {
                        crate::host::check_plugin(db, "daily_partner", epoch)?;
                        crate::catalog::check_message(db, &key, "daily_partner")?;
                        crate::access::message_revision(db, &scope, &key)?;
                        Ok(())
                    })
                    .await?;
                owner.milky.recall_message(&sent, &owner.cancel).await
            }
            .await;
            match result {
                Ok(
                    crate::milky::ActionOutcome::Confirmed | crate::milky::ActionOutcome::Deferred,
                ) => {}
                _ => tracing::warn!("daily partner recall unconfirmed; not retried"),
            }
        });
        Ok(())
    }
    fn observe_messages(&self) -> bool {
        true
    }
    async fn on_message(&self, host: &Host, message: &Incoming) -> Result<()> {
        if message.key.scope.scene != Scene::Group
            || message.notification.is_some()
            || message.is_self()
        {
            return Ok(());
        }
        let key = message.key.key();
        let scope = message.key.scope.key();
        let actor = message.sender_id;
        let epoch = host
            .epochs
            .get("daily_partner")
            .copied()
            .context("plugin unavailable")?;
        host.store.call(move |db| {
            crate::host::check_plugin(db,"daily_partner",epoch)?;
            crate::access::message_revision(db,&scope,&key)?;
            ensure!(crate::catalog::allowed_revision(db,&scope,"daily_partner")?.is_some(),"plugin disabled");
            db.execute("INSERT INTO daily_partner_active(scope,user_id,last_at) VALUES(?1,?2,?3) ON CONFLICT(scope,user_id) DO UPDATE SET last_at=excluded.last_at",params![scope,actor,now()])?;
            db.execute("DELETE FROM daily_partner_active WHERE last_at<?1",[now()-366*86400])?;
            Ok(())
        }).await
    }
}

fn settings(db: &Connection, scope: &str, defaults: &TodayWaifuConfig) -> Result<TodayWaifuConfig> {
    let raw: Option<String> = db
        .query_row(
            "SELECT settings FROM daily_partner_settings WHERE scope=?1",
            [scope],
            |r| r.get(0),
        )
        .optional()?;
    match raw {
        Some(raw) => Ok(serde_json::from_str(&raw)?),
        None => Ok(defaults.clone()),
    }
}

/// Called by the host after a confirmed reply; the host owns the delayed task.
pub async fn partner_recall_delay(host: &Host, message: &Incoming) -> Result<Option<Duration>> {
    if !matches!(
        partner_command(&message.text),
        Some(PartnerCommand::Draw | PartnerCommand::Change)
    ) {
        return Ok(None);
    }
    let scope = message.key.scope.key();
    let defaults = host.config.plugins.native.today_waifu.clone();
    host.store
        .call(move |db| {
            let config = settings(db, &scope, &defaults)?;
            Ok(config
                .auto_recall
                .then(|| Duration::from_secs(config.recall_delay_secs.clamp(1, 300))))
        })
        .await
}

const PARTNER_HELP: &str = "今日老婆：每天固定抽取一位群友，显示头像和昵称；已被抽到的群友不重复分配。\n换老婆：默认每天最多换2次；超过次数今天不再获得人选。\n今日老婆信息：查看本群规则。\n退出今日老婆 / 参与今日老婆：控制自己是否参与和被抽到。\n群管理员设置：开启/关闭换老婆、设置换老婆次数 N、开启/关闭自动撤回、设置自动撤回延迟 N、开启/关闭自动设置对方老婆、设置抽取模式 随机模式/活跃模式、设置活跃天数 N、重置今日老婆。\n也可用 /partner、/partner change|info|leave|join|reset。";

async fn partner_reply(host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
    ensure!(
        message.key.scope.scene == Scene::Group && message.notification.is_none(),
        "今日老婆仅限群聊"
    );
    let command = partner_command(&message.text).unwrap_or(PartnerCommand::Help);
    if command == PartnerCommand::Help {
        return Ok(OutgoingMessage::text(PARTNER_HELP));
    }
    let mut members = Vec::new();
    if matches!(command, PartnerCommand::Draw | PartnerCommand::Change) {
        members = host
            .milky
            .get_group_members(&message.key.scope, true)
            .await?;
        ensure!(
            members
                .iter()
                .any(|member| member.user_id == message.sender_id),
            "当前发送者不在群成员列表"
        );
    } else if command == PartnerCommand::Reset {
        ensure!(
            host.config
                .plugins
                .native
                .superusers
                .contains(&message.sender_id),
            "重置今日老婆仅机器人管理员可用"
        );
    } else if !matches!(
        command,
        PartnerCommand::Info | PartnerCommand::Leave | PartnerCommand::Join
    ) && !host
        .config
        .plugins
        .native
        .superusers
        .contains(&message.sender_id)
    {
        let member = host
            .milky
            .get_member(&message.key.scope, message.sender_id, true)
            .await?;
        ensure!(
            matches!(member.role.as_str(), "owner" | "admin"),
            "此设置仅群主或管理员可修改"
        );
    }
    let scope = message.key.scope.key();
    let key = message.key.key();
    let actor = message.sender_id;
    let bot = message.key.scope.bot_id;
    let defaults = host.config.plugins.native.today_waifu.clone();
    let timezone: chrono_tz::Tz = host.config.app.timezone.parse()?;
    let day = timezone
        .timestamp_opt(now(), 0)
        .single()
        .context("invalid time")?
        .date_naive()
        .to_string();
    let epoch = host
        .epochs
        .get("daily_partner")
        .copied()
        .context("plugin unavailable")?;
    let result = host.store.call(move |db| {
        let tx = db.transaction()?;
        crate::host::check_plugin(&tx,"daily_partner",epoch)?;
        crate::catalog::check_message(&tx,&key,"daily_partner")?;
        let mut config = settings(&tx,&scope,&defaults)?;
        let output = match command {
            PartnerCommand::Draw | PartnerCommand::Change => {
                let result = draw_partner(&tx,&scope,&day,actor,bot,&config,&members,command == PartnerCommand::Change)?;
                partner_message(result,&members,bot)
            }
            PartnerCommand::Info => OutgoingMessage::text(format!(
                "本群今日老婆规则\n抽取模式：{}\n允许换人：{}；最多 {} 次\n自动双向：{}\n自动撤回：{}；{} 秒\n活跃窗口：{} 天",
                config.select_mode,config.allow_change,config.change_limit,config.auto_pair,config.auto_recall,config.recall_delay_secs,config.active_days)),
            PartnerCommand::Leave => {
                tx.execute("INSERT OR IGNORE INTO daily_partner_optout VALUES(?1,?2)",params![scope,actor])?;
                tx.execute("UPDATE daily_partner_choices SET lost=1 WHERE scope=?1 AND day=?2 AND (actor_id=?3 OR partner_id=?3)",params![scope,day,actor])?;
                OutgoingMessage::text("已退出本群今日老婆，不再被抽取。原记录不会因重新参与而恢复；可发送“参与今日老婆”重新参与下一次抽取。")
            }
            PartnerCommand::Join => {
                tx.execute("DELETE FROM daily_partner_optout WHERE scope=?1 AND user_id=?2",params![scope,actor])?;
                OutgoingMessage::text("已重新参与本群今日老婆。当天已经失效的配对保持失效，次日重新抽取。")
            }
            PartnerCommand::Reset => {
                tx.execute("DELETE FROM daily_partner_choices WHERE scope=?1 AND day=?2",params![scope,day])?;
                OutgoingMessage::text("已重置本群今日配对和换人次数；退出名单保持不变。")
            }
            change => {
                match change {
                    PartnerCommand::AllowChange(value) => config.allow_change=value,
                    PartnerCommand::ChangeLimit(value) => {ensure!(value<=100,"换人次数需为0–100");config.change_limit=value;}
                    PartnerCommand::AutoRecall(value) => config.auto_recall=value,
                    PartnerCommand::RecallDelay(value) => {ensure!((1..=300).contains(&value),"撤回延迟需为1–300秒");config.recall_delay_secs=value;}
                    PartnerCommand::AutoPair(value) => config.auto_pair=value,
                    PartnerCommand::Mode(value) => config.select_mode=value,
                    PartnerCommand::ActiveDays(value) => {ensure!((1..=365).contains(&value),"活跃天数需为1–365");config.active_days=value;}
                    _ => bail!("unknown partner setting"),
                }
                tx.execute("INSERT INTO daily_partner_settings(scope,settings) VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET settings=excluded.settings",params![scope,serde_json::to_string(&config)?])?;
                OutgoingMessage::text("本群今日老婆规则已更新。")
            }
        };
        // State is small and bounded by the current day plus a short audit window.
        tx.execute("DELETE FROM daily_partner_choices WHERE day<?1",[chrono::Utc::now().date_naive().checked_sub_days(chrono::Days::new(3)).context("invalid date")?.to_string()])?;
        tx.commit()?;
        Ok(output)
    }).await?;
    Ok(result)
}

#[derive(Debug, PartialEq, Eq)]
enum DrawResult {
    Selected(i64, bool),
    Notice(&'static str),
}

#[allow(clippy::too_many_arguments)] // Scope, day and actor remain explicit authorization/state keys.
fn draw_partner(
    db: &Connection,
    scope: &str,
    day: &str,
    actor: i64,
    bot: i64,
    config: &TodayWaifuConfig,
    members: &[GroupMember],
    change: bool,
) -> Result<DrawResult> {
    let opted_out: BTreeSet<i64> = db
        .prepare("SELECT user_id FROM daily_partner_optout WHERE scope=?1")?
        .query_map([scope], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if opted_out.contains(&actor) {
        return Ok(DrawResult::Notice(
            "你已退出今日老婆；发送“参与今日老婆”重新参与。",
        ));
    }
    let prior:Option<(i64,u32,bool)> = db.query_row("SELECT partner_id,draws,lost FROM daily_partner_choices WHERE scope=?1 AND day=?2 AND actor_id=?3",params![scope,day,actor],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if prior.is_some_and(|(partner, _, lost)| lost || opted_out.contains(&partner)) {
        return Ok(DrawResult::Notice(
            "今天的配对已失效，或换人次数已用尽；明天再来。",
        ));
    }
    if change {
        if !config.allow_change {
            return Ok(DrawResult::Notice("本群未开启换老婆。"));
        }
        let Some((partner, draws, _)) = prior else {
            return Ok(DrawResult::Notice("请先发送“今日老婆”抽取，再换老婆。"));
        };
        if draws > config.change_limit || partner == bot {
            db.execute(
                "UPDATE daily_partner_choices SET lost=1 WHERE scope=?1 AND day=?2 AND actor_id=?3",
                params![scope, day, actor],
            )?;
            return Ok(DrawResult::Notice("换人次数已用尽，你今天没有老婆了。"));
        }
    } else if let Some((partner, _, _)) = prior {
        if partner != bot && !members.iter().any(|member| member.user_id == partner) {
            db.execute(
                "UPDATE daily_partner_choices SET lost=1 WHERE scope=?1 AND day=?2 AND actor_id=?3",
                params![scope, day, actor],
            )?;
            return Ok(DrawResult::Notice("今天的人选已离开本群，此次配对失效。"));
        }
        return Ok(DrawResult::Selected(partner, true));
    }
    let taken: BTreeSet<i64> = db
        .prepare("SELECT partner_id FROM daily_partner_choices WHERE scope=?1 AND day=?2")?
        .query_map(params![scope, day], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let eligible: Vec<i64> = members
        .iter()
        .map(|member| member.user_id)
        .filter(|id| {
            *id != actor
                && !taken.contains(id)
                && !opted_out.contains(id)
                && !config.excluded_ids.contains(id)
        })
        .collect();
    let active: BTreeSet<i64> = if config.select_mode == "active" {
        db.prepare("SELECT user_id FROM daily_partner_active WHERE scope=?1 AND last_at>=?2")?
            .query_map(
                params![scope, now() - i64::from(config.active_days) * 86400],
                |r| r.get(0),
            )?
            .collect::<rusqlite::Result<_>>()?
    } else {
        BTreeSet::new()
    };
    let active: Vec<i64> = eligible
        .iter()
        .copied()
        .filter(|id| *id == bot || active.contains(id))
        .collect();
    let choices = if config.select_mode == "active" && active.len() >= 5 {
        &active
    } else {
        &eligible
    };
    let partner = if choices.is_empty() {
        bot
    } else {
        choices[random_index(choices.len())]
    };
    let draws = prior.map_or(1, |(_, draws, _)| draws.saturating_add(1));
    db.execute("INSERT INTO daily_partner_choices(scope,day,actor_id,partner_id,draws) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(scope,day,actor_id) DO UPDATE SET partner_id=excluded.partner_id,draws=excluded.draws,lost=0",params![scope,day,actor,partner,draws])?;
    if config.auto_pair && partner != bot {
        // Like upstream, only give an unassigned partner a reciprocal choice.
        db.execute("INSERT INTO daily_partner_choices(scope,day,actor_id,partner_id,draws) VALUES(?1,?2,?3,?4,0) ON CONFLICT(scope,day,actor_id) DO UPDATE SET partner_id=excluded.partner_id WHERE daily_partner_choices.draws=0 AND daily_partner_choices.lost=0",params![scope,day,partner,actor])?;
    }
    Ok(DrawResult::Selected(partner, false))
}

fn partner_message(result: DrawResult, members: &[GroupMember], bot: i64) -> OutgoingMessage {
    match result {
        DrawResult::Notice(text) => OutgoingMessage::text(text),
        DrawResult::Selected(id, repeated) => {
            let member = members.iter().find(|member| member.user_id == id);
            let name = member
                .map(|member| {
                    if member.card.is_empty() {
                        member.nickname.as_str()
                    } else {
                        member.card.as_str()
                    }
                })
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| id.to_string());
            let title = if id == bot {
                "你今天的群友老婆是我哦。"
            } else if repeated {
                "你今天已经有老婆了："
            } else {
                "你今天的群友老婆是："
            };
            OutgoingMessage {
                segments: vec![
                    OutgoingSegment::Text {
                        text: format!("{title}\n{name}（{id}）"),
                    },
                    OutgoingSegment::Image {
                        uri: format!("https://q1.qlogo.cn/g?b=qq&nk={id}&s=640"),
                        sub_type: ImageType::Normal,
                        summary: Some("今日老婆头像".into()),
                    },
                ],
            }
        }
    }
}

fn random_index(length: usize) -> usize {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    (u64::from_le_bytes(bytes[..8].try_into().expect("eight UUID bytes")) % (length as u64))
        as usize
}

pub struct McStatusPlugin;
const MC_MIGRATION: &str = "CREATE TABLE mc_bindings(scope TEXT NOT NULL,name TEXT NOT NULL,enabled INTEGER NOT NULL,PRIMARY KEY(scope,name));";
#[async_trait]
impl Plugin for McStatusPlugin {
    fn spec(&self) -> PluginSpec {
        PluginSpec {
            id: "mc_status",
            commands: &[
                "/mc",
                "/信息",
                "/信息数据",
                "添加服务器",
                "删除服务器",
                "测试服务器",
            ],
            tools: &[],
            dependencies: &["host"],
        }
    }
    async fn init(&self, host: &Host) -> Result<()> {
        host.store.migrate("mc_status", 1, MC_MIGRATION).await
    }
    async fn command(&self, host: &Host, message: &Incoming) -> Result<String> {
        Ok(mc_reply(host, message).await?.text_content())
    }
    async fn reply(&self, host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
        mc_reply(host, message).await
    }
}
async fn mc_reply(host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
    let servers = &host.config.plugins.native.mc_servers;
    let parts: Vec<_> = message.text.split_whitespace().collect();
    let scope = message.key.scope.key();
    if matches!(
        parts.as_slice(),
        ["/mc", "bind", _] | ["/mc", "unbind", _] | ["添加服务器", _, _] | ["删除服务器", _, _]
    ) {
        ensure!(
            host.config
                .plugins
                .native
                .superusers
                .contains(&message.sender_id),
            "绑定服务器仅机器人管理员可用"
        );
        ensure!(
            message.key.scope.scene == Scene::Group,
            "请在目标群中绑定服务器"
        );
        let (name, enabled) = if parts[0] == "/mc" {
            (parts[2], parts[1] == "bind")
        } else {
            ensure!(
                parts[1].parse::<i64>().ok() == Some(message.key.scope.peer_id),
                "只能修改当前群的服务器绑定"
            );
            (parts[2], parts[0] == "添加服务器")
        };
        ensure!(
            servers.iter().any(|server| server.name == name),
            "目标需先由管理员在Web配置地址、端口和类型"
        );
        let name = name.to_owned();
        let source = message.key.key();
        let epoch = *host.epochs.get("mc_status").context("plugin unavailable")?;
        host.store.call(move|db|{
                let tx=db.transaction()?;
                crate::host::check_plugin(&tx,"mc_status",epoch)?;
                crate::catalog::check_message(&tx,&source,"mc_status")?;
                tx.execute("INSERT INTO mc_bindings VALUES(?1,?2,?3) ON CONFLICT(scope,name) DO UPDATE SET enabled=excluded.enabled",params![scope,name,enabled])?;
                tx.commit()?;Ok(())
            }).await?;
        return Ok(OutgoingMessage::text(if enabled {
            "已绑定当前群服务器。"
        } else {
            "已解除当前群服务器绑定。"
        }));
    }
    if parts.as_slice() == ["/信息数据"] {
        ensure!(
            host.config
                .plugins
                .native
                .superusers
                .contains(&message.sender_id),
            "信息数据仅机器人管理员可用"
        );
        return Ok(OutgoingMessage::text(if servers.is_empty() {
            "尚无服务器配置。".into()
        } else {
            clip_bytes(
                &servers
                    .iter()
                    .map(|server| {
                        format!(
                            "{} {}:{} {} 群 {:?}",
                            server.name, server.host, server.port, server.kind, server.group_ids
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                host.config.limits.max_message_bytes,
            )
        }));
    }
    if parts.as_slice() == ["/mc"] || parts.as_slice() == ["/mc", "list"] {
        return Ok(OutgoingMessage::text(if servers.is_empty() {
            "尚未配置 MC 服务器，请管理员在 Web 配置中添加名称、地址、端口和类型。".into()
        } else {
            format!(
                "已配置服务器：{}\n/mc 名称：查询\n/信息：查询当前群已绑定服务器\n管理员：/mc bind|unbind 名称；也可Web设置group_ids。",
                servers
                    .iter()
                    .map(|server| server.name.as_str())
                    .collect::<Vec<_>>()
                    .join("、")
            )
        }));
    }
    let targets: Vec<McServerConfig> = if parts.as_slice() == ["/信息"] {
        ensure!(
            message.key.scope.scene == Scene::Group,
            "/信息 仅用于查询当前群绑定服务器"
        );
        let overrides: std::collections::BTreeMap<String, bool> = host
            .store
            .call(move |db| {
                Ok(db
                    .prepare("SELECT name,enabled FROM mc_bindings WHERE scope=?1")?
                    .query_map([scope], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?)
            })
            .await?;
        servers
            .iter()
            .filter(|server| {
                overrides
                    .get(&server.name)
                    .copied()
                    .unwrap_or_else(|| server.group_ids.contains(&message.key.scope.peer_id))
            })
            .cloned()
            .collect()
    } else {
        ensure!(
            parts.len() == 2 && matches!(parts[0], "/mc" | "测试服务器"),
            "用法：/信息、/mc 名称，管理员绑定：/mc bind 名称；地址仅在Web配置"
        );
        if parts[0] == "测试服务器" {
            ensure!(
                host.config
                    .plugins
                    .native
                    .superusers
                    .contains(&message.sender_id),
                "测试服务器仅机器人管理员可用"
            );
        }
        vec![
            servers
                .iter()
                .find(|server| server.name == parts[1])
                .context("没有这个服务器名称；只可查询管理员已配置的目标")?
                .clone(),
        ]
    };
    if targets.is_empty() {
        return Ok(OutgoingMessage::text("当前群尚未绑定服务器。"));
    }
    // At most four sockets at a time; each query has one total deadline.
    use futures_util::{StreamExt, stream};
    let cancel = host.cancel.clone();
    let statuses = stream::iter(targets.into_iter().map(move |target| {
        let cancel = cancel.clone();
        async move {
            let result = tokio::select! {
                _=cancel.cancelled()=>Err(anyhow::anyhow!("任务已取消")),
                result=query_mc(&target)=>result,
            };
            (target, result)
        }
    }))
    .buffered(4)
    .collect::<Vec<_>>()
    .await;
    let per_status = host
        .config
        .limits
        .max_message_bytes
        .saturating_sub(statuses.len().saturating_sub(1) * 2)
        / statuses.len().max(1);
    let mut segments = vec![];
    for (index, (target, result)) in statuses.into_iter().enumerate() {
        if index > 0 {
            segments.push(OutgoingSegment::Text {
                text: "\n\n".into(),
            });
        }
        match result {
            Ok(status) => {
                if let Some(icon) = status.icon
                    && let Ok(uri) = host.media.bytes(host, icon, "image/png").await
                {
                    segments.push(OutgoingSegment::Image {
                        uri,
                        sub_type: ImageType::Normal,
                        summary: None,
                    });
                }
                segments.push(OutgoingSegment::Text {
                    text: clip_bytes(
                        &format!(
                            "{} · {}/{} · {}ms\n版本：{}\n{}\n◤ {} ◢",
                            target.name,
                            status.online,
                            status.maximum,
                            status.latency_ms,
                            status.version,
                            status.motd,
                            status.players
                        ),
                        per_status,
                    ),
                });
            }
            Err(_) => segments.push(OutgoingSegment::Text {
                text: clip_bytes(
                    &format!("{} 查询失败；目标不可达或响应无效。", target.name),
                    per_status,
                ),
            }),
        }
    }
    Ok(OutgoingMessage { segments })
}

fn clip_bytes(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit.saturating_sub("…".len()).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if limit < "…".len() {
        return text[..end].to_owned();
    }
    format!("{}…", &text[..end])
}
#[derive(Debug)]
struct McStatus {
    motd: String,
    version: String,
    online: u64,
    maximum: u64,
    latency_ms: u128,
    players: String,
    icon: Option<Vec<u8>>,
}
async fn query_mc(server: &McServerConfig) -> Result<McStatus> {
    if server.kind == "bedrock" {
        return query_bedrock(server).await;
    }
    ensure!(server.kind == "java", "unsupported Minecraft edition");
    tokio::time::timeout(Duration::from_secs(5), async {
        let started = std::time::Instant::now();
        let mut socket =
            tokio::net::TcpStream::connect((server.host.as_str(), server.port)).await?;
        let mut handshake = vec![0];
        put_varint(&mut handshake, -1_i32 as u32);
        put_varint(&mut handshake, server.host.len() as u32);
        handshake.extend_from_slice(server.host.as_bytes());
        handshake.extend_from_slice(&server.port.to_be_bytes());
        handshake.push(1);
        let mut packet = Vec::new();
        put_varint(&mut packet, handshake.len() as u32);
        packet.extend(handshake);
        packet.extend([1, 0]);
        socket.write_all(&packet).await?;
        let length = read_varint(&mut socket).await? as usize;
        ensure!(
            (2..=65536).contains(&length),
            "invalid Minecraft status packet size"
        );
        let mut packet = vec![0; length];
        socket.read_exact(&mut packet).await?;
        parse_mc(&packet, started.elapsed().as_millis())
    })
    .await
    .context("Minecraft status timeout")?
}
fn put_varint(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 127) as u8;
        value >>= 7;
        out.push(byte | if value == 0 { 0 } else { 128 });
        if value == 0 {
            break;
        }
    }
}
async fn read_varint(socket: &mut tokio::net::TcpStream) -> Result<u32> {
    let mut value = 0_u32;
    for index in 0..5 {
        let byte = socket.read_u8().await?;
        ensure!(index < 4 || byte < 16, "invalid Minecraft VarInt");
        value |= u32::from(byte & 127) << (index * 7);
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    bail!("invalid Minecraft VarInt")
}
fn slice_varint(bytes: &[u8], position: &mut usize) -> Result<u32> {
    let mut value = 0_u32;
    for index in 0..5 {
        let byte = *bytes.get(*position).context("truncated Minecraft VarInt")?;
        *position += 1;
        ensure!(index < 4 || byte < 16, "invalid Minecraft VarInt");
        value |= u32::from(byte & 127) << (index * 7);
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    bail!("invalid Minecraft VarInt")
}
fn parse_mc(packet: &[u8], latency_ms: u128) -> Result<McStatus> {
    ensure!(packet.len() <= 65536, "Minecraft status too large");
    let mut position = 0;
    ensure!(
        slice_varint(packet, &mut position)? == 0,
        "unexpected Minecraft packet"
    );
    let length = slice_varint(packet, &mut position)? as usize;
    ensure!(
        length == packet.len() - position,
        "Minecraft JSON length mismatch"
    );
    let value: Value = serde_json::from_slice(&packet[position..])?;
    let mut motd = String::new();
    component_text(&value["description"], 0, &mut motd);
    let version = value["version"]["name"]
        .as_str()
        .context("missing Minecraft version")?;
    let online = value["players"]["online"]
        .as_u64()
        .context("missing Minecraft player count")?;
    let maximum = value["players"]["max"]
        .as_u64()
        .context("missing Minecraft player limit")?;
    let sample = value["players"]["sample"].as_array();
    let mut anonymous = 0;
    let mut players = Vec::new();
    if let Some(sample) = sample {
        for player in sample.iter().take(32) {
            if player["id"] == "00000000-0000-0000-0000-000000000000" {
                anonymous += 1;
            } else if let Some(name) = player["name"].as_str() {
                players.push(clean_mc_text(name, 32));
            }
        }
    }
    if anonymous > 0 {
        players.push(format!("[{anonymous}个匿名玩家]"));
    }
    let players = if online == 0 {
        "没人在线".into()
    } else if players.is_empty() {
        "没有返回玩家列表".into()
    } else {
        players.join(", ")
    };
    use base64::Engine;
    let icon = value["favicon"]
        .as_str()
        .and_then(|value| value.strip_prefix("data:image/png;base64,"))
        .filter(|value| value.len() <= 48 * 1024)
        .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok())
        .filter(|bytes| {
            bytes.starts_with(b"\x89PNG\r\n\x1a\n")
                && bytes.len() >= 24
                && u32::from_be_bytes(bytes[16..20].try_into().unwrap()) <= 256
                && u32::from_be_bytes(bytes[20..24].try_into().unwrap()) <= 256
        });
    Ok(McStatus {
        motd: clean_mc_text(&motd, 512),
        version: clean_mc_text(version, 120),
        online,
        maximum,
        latency_ms,
        players,
        icon,
    })
}
async fn query_bedrock(server: &McServerConfig) -> Result<McStatus> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let destination = tokio::net::lookup_host((server.host.as_str(), server.port))
            .await?
            .next()
            .context("no Minecraft address")?;
        let socket = tokio::net::UdpSocket::bind(if destination.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })
        .await?;
        socket.connect(destination).await?;
        const MAGIC: [u8; 16] = [
            0, 255, 255, 0, 254, 254, 254, 254, 253, 253, 253, 253, 18, 52, 86, 120,
        ];
        let started = std::time::Instant::now();
        let nonce = chrono::Utc::now().timestamp_millis().to_be_bytes();
        let mut ping = vec![1];
        ping.extend(nonce);
        ping.extend(MAGIC);
        ping.extend(&uuid::Uuid::new_v4().as_bytes()[..8]);
        socket.send(&ping).await?;
        let mut packet = vec![0; 4097];
        let size = socket.recv(&mut packet).await?;
        ensure!((35..=4096).contains(&size), "invalid Bedrock pong length");
        packet.truncate(size);
        ensure!(
            packet[0] == 0x1c && packet[1..9] == nonce && packet[17..33] == MAGIC,
            "invalid Bedrock pong identity"
        );
        let length = u16::from_be_bytes([packet[33], packet[34]]) as usize;
        ensure!(length == packet.len() - 35, "invalid Bedrock status length");
        let text = std::str::from_utf8(&packet[35..])?;
        let parts: Vec<_> = text.split(';').collect();
        ensure!(
            parts.len() >= 6 && parts[0] == "MCPE",
            "invalid Bedrock status"
        );
        Ok(McStatus {
            motd: clean_mc_text(parts[1], 512),
            version: clean_mc_text(parts[3], 120),
            online: parts[4].parse()?,
            maximum: parts[5].parse()?,
            latency_ms: started.elapsed().as_millis(),
            players: "基岩协议不返回玩家名单".into(),
            icon: None,
        })
    })
    .await
    .context("Bedrock status timeout")?
}
fn component_text(value: &Value, depth: usize, out: &mut String) {
    if depth > 8 || out.len() >= 4096 {
        return;
    }
    match value {
        Value::String(text) => out.extend(text.chars().take(512)),
        Value::Array(values) => {
            for value in values.iter().take(64) {
                component_text(value, depth + 1, out);
            }
        }
        Value::Object(map) => {
            if let Some(text) = map.get("text") {
                component_text(text, depth + 1, out);
            }
            if let Some(extra) = map.get("extra") {
                component_text(extra, depth + 1, out);
            }
        }
        _ => {}
    }
}
fn clean_mc_text(text: &str, max: usize) -> String {
    let mut skip = false;
    text.chars()
        .filter(|ch| {
            if skip {
                skip = false;
                false
            } else if *ch == '§' {
                skip = true;
                false
            } else {
                !ch.is_control() || *ch == '\n'
            }
        })
        .take(max)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(MIGRATION).unwrap();
        db
    }
    fn members(ids: &[i64]) -> Vec<GroupMember> {
        ids.iter()
            .map(|id| GroupMember {
                user_id: *id,
                group_id: 20,
                nickname: id.to_string(),
                card: String::new(),
                role: "member".into(),
                raw: Value::Null,
            })
            .collect()
    }
    #[test]
    fn partner_commands_match_full_phrases_and_compact_numeric_settings() {
        assert_eq!(
            partner_command("设置换老婆次数2"),
            Some(PartnerCommand::ChangeLimit(2))
        );
        assert_eq!(
            partner_command("设置抽取模式 活跃模式"),
            Some(PartnerCommand::Mode("active".into()))
        );
        assert_eq!(partner_command("有人说今日老婆"), None);
        assert_eq!(partner_command("设置换老婆次数abc"), None);
    }
    #[test]
    fn daily_choice_is_stable_and_scoped_and_taken_members_are_excluded() {
        let db = database();
        let cfg = TodayWaifuConfig::default();
        let list = members(&[1, 2, 3]);
        let first =
            draw_partner(&db, "10:group:20", "2026-10-20", 1, 10, &cfg, &list, false).unwrap();
        let DrawResult::Selected(partner, false) = first else {
            panic!("expected draw")
        };
        assert_eq!(
            draw_partner(&db, "10:group:20", "2026-10-20", 1, 10, &cfg, &list, false).unwrap(),
            DrawResult::Selected(partner, true)
        );
        let DrawResult::Selected(other, false) =
            draw_partner(&db, "10:group:20", "2026-10-20", 2, 10, &cfg, &list, false).unwrap()
        else {
            panic!("expected draw")
        };
        assert_ne!(other, partner);
        assert!(matches!(
            draw_partner(&db, "11:group:20", "2026-10-20", 1, 11, &cfg, &list, false).unwrap(),
            DrawResult::Selected(_, false)
        ));
        assert!(matches!(
            draw_partner(&db, "10:group:20", "2026-10-21", 1, 10, &cfg, &list, false).unwrap(),
            DrawResult::Selected(_, false)
        ));
    }
    #[test]
    fn reciprocal_choice_and_optout_and_change_limit_are_persistent() {
        let db = database();
        let cfg = TodayWaifuConfig {
            auto_pair: true,
            ..Default::default()
        };
        let list = members(&[1, 2]);
        assert_eq!(
            draw_partner(&db, "a", "day", 1, 10, &cfg, &list, false).unwrap(),
            DrawResult::Selected(2, false)
        );
        assert_eq!(
            draw_partner(&db, "a", "day", 2, 10, &cfg, &list, false).unwrap(),
            DrawResult::Selected(1, true)
        );
        db.execute("INSERT INTO daily_partner_optout VALUES('a',2)", [])
            .unwrap();
        assert!(matches!(
            draw_partner(&db, "a", "day", 1, 10, &cfg, &list, false).unwrap(),
            DrawResult::Notice(_)
        ));
        let cfg = TodayWaifuConfig {
            change_limit: 0,
            ..Default::default()
        };
        let list = members(&[1, 2, 3]);
        draw_partner(&db, "b", "day", 1, 10, &cfg, &list, false).unwrap();
        assert!(matches!(
            draw_partner(&db, "b", "day", 1, 10, &cfg, &list, true).unwrap(),
            DrawResult::Notice(_)
        ));
        assert!(matches!(
            draw_partner(&db, "b", "day", 1, 10, &cfg, &list, false).unwrap(),
            DrawResult::Notice(_)
        ));
    }
    #[test]
    fn active_mode_uses_active_pool_only_when_at_least_five_are_eligible() {
        let db = database();
        let cfg = TodayWaifuConfig {
            select_mode: "active".into(),
            ..Default::default()
        };
        for id in 2..=6 {
            db.execute(
                "INSERT INTO daily_partner_active VALUES('s',?1,?2)",
                params![id, now()],
            )
            .unwrap();
        }
        let list = members(&[1, 2, 3, 4, 5, 6, 99]);
        let DrawResult::Selected(id, _) =
            draw_partner(&db, "s", "day", 1, 10, &cfg, &list, false).unwrap()
        else {
            panic!("expected draw")
        };
        assert!((2..=6).contains(&id));
    }
    #[test]
    fn minecraft_status_parser_is_bounded_and_cleans_legacy_formatting() {
        let body=serde_json::json!({"description":{"text":"§aHello","extra":[{"text":" world"}]},"version":{"name":"1.21"},"players":{"online":3,"max":20}}).to_string();
        let mut packet = vec![0];
        put_varint(&mut packet, body.len() as u32);
        packet.extend(body.as_bytes());
        let status = parse_mc(&packet, 15).unwrap();
        assert_eq!(status.motd, "Hello world");
        assert_eq!(status.online, 3);
        packet.push(0);
        assert!(parse_mc(&packet, 1).is_err());
        assert!(parse_mc(&[255; 6], 1).is_err());
    }
    #[test]
    fn multiple_minecraft_statuses_fit_the_shared_message_budget() {
        let long = "中文服务器名称与详细玩家列表".repeat(512);
        for limit in [1024, 16384, 32768] {
            let per = (limit - 31 * 2) / 32;
            let body = (0..32)
                .map(|_| clip_bytes(&long, per))
                .collect::<Vec<_>>()
                .join("\n\n");
            assert!(body.len() <= limit);
            assert_eq!(body.matches('…').count(), 32);
            assert!(body.is_char_boundary(body.len()));
        }
    }
    #[tokio::test]
    async fn minecraft_query_uses_configured_target_and_real_status_wire_format() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let length = read_varint(&mut stream).await.unwrap() as usize;
            let mut handshake = vec![0; length];
            stream.read_exact(&mut handshake).await.unwrap();
            assert_eq!(handshake[0], 0);
            assert_eq!(*handshake.last().unwrap(), 1);
            let mut status_request = [0; 2];
            stream.read_exact(&mut status_request).await.unwrap();
            assert_eq!(status_request, [1, 0]);
            let json=br#"{"description":"test","version":{"name":"test"},"players":{"online":1,"max":9}}"#;
            let mut body = vec![0];
            put_varint(&mut body, json.len() as u32);
            body.extend(json);
            let mut packet = vec![];
            put_varint(&mut packet, body.len() as u32);
            packet.extend(body);
            stream.write_all(&packet).await.unwrap();
        });
        let status = query_mc(&McServerConfig {
            name: "test".into(),
            host: "127.0.0.1".into(),
            port: address.port(),
            kind: "java".into(),
            group_ids: vec![],
        })
        .await
        .unwrap();
        assert_eq!(status.online, 1);
        task.await.unwrap();
    }
}
