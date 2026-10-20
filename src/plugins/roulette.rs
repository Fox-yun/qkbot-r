//! Shared six-chamber game and the upstream administrator mute/unmute helpers.
//! Every moderation target has a durable intent; unknown results are not retried.
use crate::{
    host::Host,
    milky::ActionOutcome,
    plugin::{Plugin, PluginSpec},
    types::{Incoming, Scene, now},
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeSet;

pub struct MuteRoulettePlugin;
const MIGRATION: &str = r#"
CREATE TABLE roulette_games(
 scope TEXT PRIMARY KEY, free INTEGER NOT NULL DEFAULT 0,
 chamber INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL
);
CREATE TABLE roulette_actions(
 source_key TEXT PRIMARY KEY,scope TEXT NOT NULL,actor_id INTEGER NOT NULL,
 duration INTEGER NOT NULL,state TEXT NOT NULL,created_at INTEGER NOT NULL
);
"#;
const ADMIN_MIGRATION: &str = r#"
CREATE TABLE roulette_moderation(
 source_key TEXT NOT NULL,target_id INTEGER NOT NULL,scope TEXT NOT NULL,
 actor_id INTEGER NOT NULL,duration INTEGER NOT NULL,state TEXT NOT NULL,
 created_at INTEGER NOT NULL,PRIMARY KEY(source_key,target_id)
);
CREATE TABLE roulette_amnesty_pending(
 scope TEXT NOT NULL,actor_id INTEGER NOT NULL,source_key TEXT NOT NULL,
 candidates TEXT NOT NULL,expires_at INTEGER NOT NULL,plugin_epoch INTEGER NOT NULL,
 PRIMARY KEY(scope,actor_id)
);
"#;
const MAX_TARGETS: usize = 32;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Administration {
    Mute(u32),
    Unmute,
}
fn admin_root(text: &str) -> bool {
    let text = text.trim().trim_start_matches('/');
    ["禁言", "解封", "解禁", "解除禁言"]
        .iter()
        .any(|prefix| text.starts_with(prefix))
}
fn administration(text: &str) -> Result<Option<Administration>> {
    let text = text.trim().trim_start_matches('/');
    if ["解封", "解禁", "解除禁言"]
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        return Ok(Some(Administration::Unmute));
    }
    let Some(rest) = text.strip_prefix("禁言") else {
        return Ok(None);
    };
    let rest = rest.trim_start();
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (amount, unit) = if digits > 0 {
        (
            rest[..digits].parse::<u64>().context("禁言时长过大")?,
            &rest[digits..],
        )
    } else if let Some((ch, value)) = [
        ('一', 1),
        ('二', 2),
        ('三', 3),
        ('四', 4),
        ('五', 5),
        ('六', 6),
        ('七', 7),
        ('八', 8),
        ('九', 9),
        ('十', 10),
    ]
    .into_iter()
    .find(|(ch, _)| rest.starts_with(*ch))
    {
        (value, &rest[ch.len_utf8()..])
    } else {
        (5, rest)
    };
    let amount = if amount == 0 { 5 } else { amount };
    let multiplier = match unit.trim() {
        "秒" | "s" => 1,
        "小时" | "h" => 3600,
        "天" | "d" => 86400,
        "月" | "个月" | "M" => 30 * 86400,
        _ => 60,
    };
    let duration = amount
        .checked_mul(multiplier)
        .filter(|value| *value <= 30 * 86400)
        .context("管理员禁言时长不能超过30天")?;
    Ok(Some(Administration::Mute(duration as u32)))
}
fn selection_ids(text: &str) -> Option<Vec<i64>> {
    let mut ids = BTreeSet::new();
    for value in text.split_whitespace() {
        if !value.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let id = value.parse::<i64>().ok().filter(|id| *id > 0)?;
        ids.insert(id);
        if ids.len() > MAX_TARGETS {
            return None;
        }
    }
    (!ids.is_empty()).then(|| ids.into_iter().collect())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Start,
    Reload,
    Shot,
    Enable,
    Disable,
    Help,
}
fn parse(text: &str) -> Option<Command> {
    Some(match text.trim() {
        "/roulette" | "/roulette help" => Command::Help,
        "/roulette start" | "自由轮盘" | "轮盘禁言" | "/自由轮盘" | "/轮盘禁言" => {
            Command::Start
        }
        "/roulette reload" | "重新装弹" | "拨动滚轮" | "/重新装弹" | "/拨动滚轮" => {
            Command::Reload
        }
        "/roulette shot" | "开枪" | "/开枪" => Command::Shot,
        "/roulette enable"
        | "开启自由轮盘"
        | "开启轮盘禁言"
        | "/开启自由轮盘"
        | "/开启轮盘禁言" => Command::Enable,
        "/roulette disable"
        | "关闭自由轮盘"
        | "关闭轮盘禁言"
        | "/关闭自由轮盘"
        | "/关闭轮盘禁言" => Command::Disable,
        _ => return None,
    })
}
const HELP: &str = "自由轮盘 / /roulette start：群共享六发弹仓，一颗子弹。\n开枪 / /roulette shot：命中禁言本次开枪者2–6分钟（受游戏上限约束），游戏结束；空仓后剩余弹仓递减。\n重新装弹 / 拨动滚轮 / /roulette reload：重新随机子弹位置。\n开启/关闭自由轮盘：群管理员决定普通成员能否开始游戏。\n管理员：禁言5分钟 @成员（默认5分钟，支持秒/分钟/小时/天/月，最多30天）；解封/解禁/解除禁言 @成员。无@时先列出被禁言成员，120秒内回复QQ号，可用空格分隔；一次最多32人。机器人管理员可全局在Web启停本插件。";
#[async_trait]
impl Plugin for MuteRoulettePlugin {
    fn spec(&self) -> PluginSpec {
        PluginSpec {
            id: "mute_roulette",
            commands: &[
                "/roulette",
                "自由轮盘",
                "轮盘禁言",
                "重新装弹",
                "拨动滚轮",
                "开枪",
                "开启自由轮盘",
                "开启轮盘禁言",
                "关闭自由轮盘",
                "关闭轮盘禁言",
                "/自由轮盘",
                "/轮盘禁言",
                "/重新装弹",
                "/拨动滚轮",
                "/开枪",
                "/开启自由轮盘",
                "/开启轮盘禁言",
                "/关闭自由轮盘",
                "/关闭轮盘禁言",
                "禁言",
                "解封",
                "解禁",
                "解除禁言",
                "/禁言",
                "/解封",
                "/解禁",
                "/解除禁言",
            ],
            tools: &[],
            dependencies: &["host"],
        }
    }
    fn accepts(&self, message: &Incoming) -> bool {
        message.key.scope.scene == Scene::Group
    }
    fn claims(&self, message: &Incoming) -> bool {
        message.notification.is_none()
            && (parse(&message.text).is_some() || admin_root(&message.text))
    }
    async fn handles_message(&self, host: &Host, message: &Incoming) -> Result<bool> {
        if self.claims(message) {
            return Ok(true);
        }
        if message.key.scope.scene != Scene::Group
            || message.notification.is_some()
            || selection_ids(&message.text).is_none()
        {
            return Ok(false);
        }
        let scope = message.key.scope.key();
        let actor = message.sender_id;
        let epoch = host.epochs.get("mute_roulette").copied().unwrap_or(0);
        host.store.call(move|db| {
            let pending:Option<String>=db.query_row("SELECT source_key FROM roulette_amnesty_pending WHERE scope=?1 AND actor_id=?2 AND expires_at>?3 AND plugin_epoch=?4",params![scope,actor,now(),epoch],|r|r.get(0)).optional()?;
            Ok(pending.is_some_and(|source|crate::catalog::check_message(db,&source,"mute_roulette").is_ok()))
        }).await
    }
    async fn init(&self, host: &Host) -> Result<()> {
        host.store.migrate("mute_roulette", 1, MIGRATION).await?;
        host.store
            .migrate("mute_roulette", 2, ADMIN_MIGRATION)
            .await?;
        host.store
            .call(|db| {
                db.execute(
                    "UPDATE roulette_actions SET state='unknown' WHERE state='submitted'",
                    [],
                )?;
                db.execute(
                    "UPDATE roulette_moderation SET state='unknown' WHERE state='submitted'",
                    [],
                )?;
                db.execute(
                    "UPDATE roulette_moderation SET state='failed' WHERE state='pending'",
                    [],
                )?;
                db.execute("DELETE FROM roulette_amnesty_pending", [])?;
                db.execute(
                    "UPDATE roulette_actions SET state='failed' WHERE state='pending'",
                    [],
                )?;
                Ok(())
            })
            .await
    }
    async fn command(&self, host: &Host, message: &Incoming) -> Result<String> {
        ensure!(
            message.key.scope.scene == Scene::Group && message.notification.is_none(),
            "轮盘游戏仅限群聊"
        );
        if let Some(command) = administration(&message.text)? {
            return administer(host, message, command, false).await;
        }
        if parse(&message.text).is_none() && selection_ids(&message.text).is_some() {
            return administer(host, message, Administration::Unmute, true).await;
        }
        let command = parse(&message.text).unwrap_or(Command::Help);
        if command == Command::Help {
            return Ok(HELP.into());
        }
        let administrator =
            if matches!(command, Command::Start | Command::Enable | Command::Disable) {
                if host
                    .config
                    .plugins
                    .native
                    .superusers
                    .contains(&message.sender_id)
                {
                    true
                } else {
                    let member = host
                        .milky
                        .get_member(&message.key.scope, message.sender_id, true)
                        .await?;
                    matches!(member.role.as_str(), "owner" | "admin")
                }
            } else {
                false
            };
        let source = message.key.key();
        let scope = message.key.scope.key();
        let actor = message.sender_id;
        let epoch = *host
            .epochs
            .get("mute_roulette")
            .context("plugin unavailable")?;
        let duration = ((random(5) + 2) * 60).min(host.config.plugins.native.mute_max_secs as u32);
        let chamber = random(6) + 1;
        let result=host.store.call(move|db|{
            let tx=db.transaction()?;
            crate::host::check_plugin(&tx,"mute_roulette",epoch)?;
            crate::catalog::check_message(&tx,&source,"mute_roulette")?;
            let result=step(&tx,&scope,command,administrator,chamber)?;
            if result.hit {
                ensure!(tx.execute("INSERT OR IGNORE INTO roulette_actions VALUES(?1,?2,?3,?4,'pending',?5)",params![source,scope,actor,duration,now()])?==1,"这次开枪已经处理");
            }
            tx.execute("DELETE FROM roulette_actions WHERE created_at<?1 AND state!='submitted'",[now()-7*86400])?;
            tx.commit()?;Ok(result)
        }).await?;
        if !result.hit {
            return Ok(result.text);
        }
        let action = apply_mute(host, message, duration, epoch).await;
        let status = match action {
            Ok(ActionOutcome::Confirmed) => {
                format!("命中！游戏结束。已禁言本次开枪者 {duration} 秒。")
            }
            Ok(ActionOutcome::Deferred) => "命中！游戏结束；任务取消，未提交禁言。".into(),
            Ok(ActionOutcome::Unknown { .. }) => {
                "命中！游戏结束；禁言结果暂时未知，不会自动重试。".into()
            }
            Ok(ActionOutcome::Failed { .. }) | Err(_) => {
                "命中！游戏结束；机器人权限不足、群状态改变或请求失败，本次禁言未确认。".into()
            }
        };
        Ok(status)
    }
}

async fn administer(
    host: &Host,
    message: &Incoming,
    command: Administration,
    followup: bool,
) -> Result<String> {
    let actor = host
        .milky
        .get_member(&message.key.scope, message.sender_id, true)
        .await?;
    let superuser = host
        .config
        .plugins
        .native
        .superusers
        .contains(&message.sender_id);
    ensure!(
        superuser || matches!(actor.role.as_str(), "owner" | "admin"),
        "只有群管理员、群主或机器人管理员可禁言和解禁成员"
    );
    let epoch = *host.epochs.get("mute_roulette").context("插件未开启")?;
    let mut targets: Vec<i64> = message
        .mentions
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if followup {
        let requested = selection_ids(&message.text).context("请用空格分隔要解除禁言的QQ号")?;
        let scope = message.key.scope.key();
        let sender = message.sender_id;
        let source = message.key.key();
        targets = host
            .store
            .call(move |db| {
                let tx = db.transaction()?;
                crate::host::check_plugin(&tx, "mute_roulette", epoch)?;
                crate::catalog::check_message(&tx, &source, "mute_roulette")?;
                let targets = consume_selection(&tx, &scope, sender, epoch, &requested)?;
                tx.commit()?;
                Ok(targets)
            })
            .await?;
    } else if targets.is_empty() {
        if matches!(command, Administration::Mute(_)) {
            return Ok("请在禁言时长后真正@要禁言的成员，例如：禁言5分钟 @成员。".into());
        }
        return list_muted(host, message, epoch).await;
    }
    ensure!(
        !targets.is_empty() && targets.len() <= MAX_TARGETS && targets.iter().all(|id| *id > 0),
        "一次最多处理32位实际成员"
    );
    let duration = match command {
        Administration::Mute(seconds) => seconds,
        Administration::Unmute => 0,
    };
    let (source, scope, actor) = (
        message.key.key(),
        message.key.scope.key(),
        message.sender_id,
    );
    let recorded = targets.clone();
    host.store
        .call(move |db| {
            let tx = db.transaction()?;
            crate::host::check_plugin(&tx, "mute_roulette", epoch)?;
            crate::catalog::check_message(&tx, &source, "mute_roulette")?;
            record_admin_intents(&tx, &source, &scope, actor, duration, &recorded)?;
            tx.execute(
                "DELETE FROM roulette_moderation WHERE created_at<?1 AND state!='submitted'",
                [now() - 7 * 86400],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    use futures_util::{StreamExt, stream};
    let results = stream::iter(targets)
        .map(|target| async move {
            (
                target,
                apply_admin_mute(host, message, target, duration, superuser, epoch).await,
            )
        })
        .buffered(4)
        .collect::<Vec<_>>()
        .await;
    let mut lines = vec![];
    let mut counts = [0usize; 4];
    for (target, result) in results {
        let (index, status) = match result {
            Ok(ActionOutcome::Confirmed) => (0, "已确认"),
            Ok(ActionOutcome::Deferred) => (1, "未提交"),
            Ok(ActionOutcome::Unknown { .. }) => (2, "结果未知，不会自动重试"),
            Ok(ActionOutcome::Failed { .. }) | Err(_) => {
                (3, "未确认（权限、成员状态或协议请求失败）")
            }
        };
        counts[index] += 1;
        lines.push(format!("{target}：{status}"));
    }
    let action = if duration == 0 {
        "解除禁言".to_owned()
    } else {
        format!("禁言 {duration} 秒")
    };
    let mut text = format!(
        "{action}：成功{}、未提交{}、未知{}、未确认{}。未知结果不会自动重试。",
        counts[0], counts[1], counts[2], counts[3]
    );
    for (index, line) in lines.iter().enumerate() {
        if text.len() + line.len() + 80 > host.config.limits.max_message_bytes {
            text.push_str(&format!(
                "\n其余{}位的统计已计入上方。",
                lines.len() - index
            ));
            break;
        }
        text.push('\n');
        text.push_str(line);
    }
    Ok(text)
}

fn record_admin_intents(
    db: &Connection,
    source: &str,
    scope: &str,
    actor: i64,
    duration: u32,
    targets: &[i64],
) -> Result<()> {
    for target in targets {
        ensure!(
            db.execute(
                "INSERT OR IGNORE INTO roulette_moderation VALUES(?1,?2,?3,?4,?5,'pending',?6)",
                params![source, target, scope, actor, duration, now()]
            )? == 1,
            "这条管理指令已有处理记录，不重复执行"
        );
    }
    Ok(())
}

fn consume_selection(
    db: &Connection,
    scope: &str,
    actor: i64,
    epoch: i64,
    requested: &[i64],
) -> Result<Vec<i64>> {
    let prior:Option<(String,String)>=db.query_row(
        "SELECT source_key,candidates FROM roulette_amnesty_pending WHERE scope=?1 AND actor_id=?2 AND expires_at>?3 AND plugin_epoch=?4",
        params![scope,actor,now(),epoch],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let (source, candidates) = prior.context("解禁选择已过期，请重新发送“解禁”")?;
    crate::catalog::check_message(db, &source, "mute_roulette")?;
    let candidates: BTreeSet<i64> = serde_json::from_str(&candidates)?;
    let targets: Vec<i64> = requested
        .iter()
        .copied()
        .filter(|id| candidates.contains(id))
        .collect();
    ensure!(!targets.is_empty(), "所选QQ号不在本次列出的禁言成员中");
    db.execute(
        "DELETE FROM roulette_amnesty_pending WHERE scope=?1 AND actor_id=?2",
        params![scope, actor],
    )?;
    Ok(targets)
}

async fn list_muted(host: &Host, message: &Incoming, epoch: i64) -> Result<String> {
    let members = host
        .milky
        .get_group_members(&message.key.scope, true)
        .await?;
    let missing = members
        .iter()
        .any(|member| member.raw.get("shut_up_end_time").is_none());
    let mut candidates = vec![];
    let mut text = String::from("以下成员正在被禁言：\n");
    let mut omitted = 0usize;
    let limit = host.config.limits.max_message_bytes.saturating_sub(350);
    for member in members {
        let Some(end) = member.raw["shut_up_end_time"]
            .as_i64()
            .filter(|end| *end > now())
        else {
            continue;
        };
        let name = if member.card.is_empty() {
            &member.nickname
        } else {
            &member.card
        };
        let line = format!(
            "{name} {} · 剩余{}秒\n",
            member.user_id,
            end.saturating_sub(now())
        );
        if text.len() + line.len() > limit {
            omitted += 1;
            continue;
        }
        candidates.push(member.user_id);
        text.push_str(&line);
    }
    if candidates.is_empty() {
        text = if missing {
            "协议端未返回可确认的禁言记录；可直接@成员发送“解禁”。".into()
        } else {
            "当前没有成员被禁言。".into()
        };
    } else {
        text.push_str("请在120秒内回复要解禁的QQ号，多个用空格隔开（一次最多32人）。");
        if omitted > 0 {
            text.push_str(&format!("\n另有{omitted}位未显示，可直接@目标发送“解禁”。"));
        }
        if missing {
            text.push_str("\n部分成员未返回禁言时间，列表可能不完整。");
        }
    }
    let scope = message.key.scope.key();
    let actor = message.sender_id;
    let source = message.key.key();
    let encoded = serde_json::to_string(&candidates)?;
    host.store.call(move|db| {
        let tx=db.transaction()?;
        crate::host::check_plugin(&tx,"mute_roulette",epoch)?;
        crate::catalog::check_message(&tx,&source,"mute_roulette")?;
        tx.execute("DELETE FROM roulette_amnesty_pending WHERE expires_at<=?1 OR (scope=?2 AND actor_id=?3)",params![now(),scope,actor])?;
        if !candidates.is_empty() {
            let count:i64=tx.query_row("SELECT COUNT(*) FROM roulette_amnesty_pending",[],|r|r.get(0))?;
            ensure!(count<1024,"解禁选择队列已满，请直接@成员解禁");
            tx.execute("INSERT INTO roulette_amnesty_pending VALUES(?1,?2,?3,?4,?5,?6)",params![scope,actor,source,encoded,now()+120,epoch])?;
        }
        tx.commit()?;Ok(())
    }).await?;
    Ok(text)
}

async fn apply_admin_mute(
    host: &Host,
    message: &Incoming,
    target: i64,
    duration: u32,
    superuser: bool,
    epoch: i64,
) -> Result<ActionOutcome> {
    let _gate = host.access.delivery_guard(&message.key.scope).await?;
    let source = message.key.key();
    host.store.call(move|db| {
        let tx=db.transaction()?;
        crate::host::check_plugin(&tx,"mute_roulette",epoch)?;
        crate::catalog::check_message(&tx,&source,"mute_roulette")?;
        ensure!(tx.execute("UPDATE roulette_moderation SET state='submitted' WHERE source_key=?1 AND target_id=?2 AND state='pending'",params![source,target])?==1,"这次成员管理操作已有记录");
        tx.commit()?;Ok(())
    }).await?;
    let outcome = host
        .milky
        .set_group_member_mute_as_admin(
            &message.key.scope,
            message.sender_id,
            target,
            duration,
            superuser,
            &host.cancel,
        )
        .await;
    let state = match &outcome {
        Ok(ActionOutcome::Confirmed) => "confirmed",
        Ok(ActionOutcome::Deferred) => "not_submitted",
        Ok(ActionOutcome::Unknown { .. }) => "unknown",
        Ok(ActionOutcome::Failed { .. }) | Err(_) => "failed",
    };
    let source = message.key.key();
    if host.store.call(move|db| {
        db.execute("UPDATE roulette_moderation SET state=?3 WHERE source_key=?1 AND target_id=?2 AND state='submitted'",params![source,target,state])?;
        Ok(())
    }).await.is_err(){tracing::warn!("administrative mute result could not be persisted; no retry");}
    outcome
}

struct Step {
    text: String,
    hit: bool,
}
fn step(db: &Connection, scope: &str, command: Command, admin: bool, chamber: u32) -> Result<Step> {
    ensure!((1..=6).contains(&chamber), "invalid chamber");
    let prior: Option<(bool, u32)> = db
        .query_row(
            "SELECT free,chamber FROM roulette_games WHERE scope=?1",
            [scope],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (free, current) = prior.unwrap_or((false, 0));
    let (next_free, next_chamber, text, hit) = match command {
        Command::Enable | Command::Disable => {
            ensure!(admin, "只有群管理员或机器人管理员可修改自由轮盘开关");
            let enabled = command == Command::Enable;
            (
                enabled,
                if enabled { current } else { 0 },
                if enabled {
                    "自由轮盘已开启；普通成员可以开始游戏。"
                } else {
                    "自由轮盘已关闭，当前游戏已结束。"
                }
                .to_owned(),
                false,
            )
        }
        Command::Start => {
            ensure!(
                admin || free,
                "本群尚未开启自由轮盘，请管理员先发送“开启自由轮盘”"
            );
            (
                free,
                chamber,
                format!(
                    "{} 群共享六发弹仓，一颗子弹；发送“开枪”即自愿接受命中后本人禁言2–6分钟（受Web上限约束）。可“拨动滚轮”重新随机。",
                    if current == 0 {
                        "游戏开始！"
                    } else {
                        "重新装弹！"
                    }
                ),
                false,
            )
        }
        Command::Reload => {
            ensure!(current > 0, "当前没有进行中的轮盘游戏");
            (
                free,
                chamber,
                "重新装弹！子弹位置已随机重置为六个位置之一。".into(),
                false,
            )
        }
        Command::Shot => {
            ensure!(current > 0, "当前没有进行中的轮盘游戏，请先开始自由轮盘");
            if current == 1 {
                (free, 0, "命中！游戏结束。".into(), true)
            } else {
                (
                    free,
                    current - 1,
                    "空仓，继续！下一位可以开枪，也可以重新拨动滚轮。".into(),
                    false,
                )
            }
        }
        Command::Help => {
            return Ok(Step {
                text: HELP.into(),
                hit: false,
            });
        }
    };
    db.execute("INSERT INTO roulette_games VALUES(?1,?2,?3,?4) ON CONFLICT(scope) DO UPDATE SET free=excluded.free,chamber=excluded.chamber,updated_at=excluded.updated_at",params![scope,next_free,next_chamber,now()])?;
    Ok(Step { text, hit })
}

async fn apply_mute(
    host: &Host,
    message: &Incoming,
    duration: u32,
    epoch: i64,
) -> Result<ActionOutcome> {
    let _gate = host.access.delivery_guard(&message.key.scope).await?;
    let source = message.key.key();
    host.store.call(move|db|{
        let tx=db.transaction()?;
        crate::host::check_plugin(&tx,"mute_roulette",epoch)?;
        crate::catalog::check_message(&tx,&source,"mute_roulette")?;
        ensure!(tx.execute("UPDATE roulette_actions SET state='submitted' WHERE source_key=?1 AND state='pending'",[source])?==1,"禁言操作已处理");
        tx.commit()?;Ok(())
    }).await?;
    let outcome = host
        .milky
        .set_group_member_mute(
            &message.key.scope,
            message.sender_id,
            message.sender_id,
            duration,
            &host.cancel,
        )
        .await;
    let state = match &outcome {
        Ok(ActionOutcome::Confirmed) => "confirmed",
        Ok(ActionOutcome::Deferred) => "not_submitted",
        Ok(ActionOutcome::Failed { .. }) | Err(_) => "failed",
        Ok(ActionOutcome::Unknown { .. }) => "unknown",
    };
    let source = message.key.key();
    if host
        .store
        .call(move |db| {
            db.execute(
                "UPDATE roulette_actions SET state=?2 WHERE source_key=?1 AND state='submitted'",
                params![source, state],
            )?;
            Ok(())
        })
        .await
        .is_err()
    {
        tracing::warn!("roulette moderation result could not be stored; no retry");
    }
    outcome
}
fn random(max: u32) -> u32 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    u32::from_le_bytes(bytes[..4].try_into().expect("four UUID bytes")) % max
}

#[cfg(test)]
mod tests {
    use super::*;
    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(MIGRATION).unwrap();
        db.execute_batch(ADMIN_MIGRATION).unwrap();
        db
    }
    #[test]
    fn shared_chamber_decrements_until_hit_and_roll_resets_six_positions() {
        let db = database();
        step(&db, "1:group:20", Command::Start, true, 3).unwrap();
        assert!(
            !step(&db, "1:group:20", Command::Shot, false, 6)
                .unwrap()
                .hit
        );
        assert!(
            !step(&db, "1:group:20", Command::Shot, false, 6)
                .unwrap()
                .hit
        );
        assert!(
            step(&db, "1:group:20", Command::Shot, false, 6)
                .unwrap()
                .hit
        );
        assert!(step(&db, "1:group:20", Command::Shot, false, 6).is_err());
        step(&db, "1:group:20", Command::Start, true, 2).unwrap();
        step(&db, "1:group:20", Command::Reload, false, 6).unwrap();
        let value: u32 = db
            .query_row("SELECT chamber FROM roulette_games", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 6);
    }
    #[test]
    fn normal_start_requires_group_enable_and_scopes_never_share_game() {
        let db = database();
        assert!(step(&db, "1:group:20", Command::Start, false, 1).is_err());
        step(&db, "1:group:20", Command::Enable, true, 1).unwrap();
        step(&db, "1:group:20", Command::Start, false, 1).unwrap();
        assert!(step(&db, "2:group:20", Command::Shot, false, 1).is_err());
        assert!(step(&db, "1:group:21", Command::Shot, false, 1).is_err());
        assert!(
            step(&db, "1:group:20", Command::Shot, false, 1)
                .unwrap()
                .hit
        );
        // Completing a game preserves the administrator's switch.
        assert!(step(&db, "1:group:20", Command::Start, false, 6).is_ok());
        step(&db, "1:group:20", Command::Disable, true, 1).unwrap();
        assert!(step(&db, "1:group:20", Command::Shot, false, 1).is_err());
    }
    #[test]
    fn only_exact_voluntary_shot_commands_are_claimed() {
        assert_eq!(parse("开枪"), Some(Command::Shot));
        assert_eq!(parse("/roulette shot"), Some(Command::Shot));
        assert_eq!(parse("帮我禁言123"), None);
        assert_eq!(parse("/roulette shot 123"), None);
    }
    #[test]
    fn administrator_time_units_and_protocol_limit_match_upstream() {
        for (command, seconds) in [
            ("禁言", 300),
            ("禁言0秒", 5),
            ("禁言三分钟", 180),
            ("禁言2h", 7200),
            ("禁言1天", 86400),
            ("禁言1个月", 2592000),
            ("禁言十秒", 10),
            ("/禁言5s", 5),
        ] {
            assert_eq!(
                administration(command).unwrap(),
                Some(Administration::Mute(seconds))
            );
        }
        for command in ["解封", "解禁", "解除禁言", "/解禁"] {
            assert_eq!(
                administration(command).unwrap(),
                Some(Administration::Unmute)
            );
        }
        assert!(administration("禁言31天").is_err());
        assert!(administration("禁言999999999999999999999999999月").is_err());
        assert_eq!(administration("全群禁言").unwrap(), None);
        assert_eq!(selection_ids("123 456 123"), Some(vec![123, 456]));
        assert!(selection_ids("123 other").is_none());
    }
    #[test]
    fn administrator_ledger_never_replays_unknown_or_reuses_a_target_intent() {
        let db = database();
        record_admin_intents(&db, "1:group:20:1", "1:group:20", 7, 0, &[8, 9]).unwrap();
        db.execute(
            "UPDATE roulette_moderation SET state='unknown' WHERE target_id=8",
            [],
        )
        .unwrap();
        assert!(record_admin_intents(&db, "1:group:20:1", "1:group:20", 7, 60, &[8]).is_err());
        assert_eq!(
            db.query_row(
                "SELECT duration,state FROM roulette_moderation WHERE target_id=8",
                [],
                |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?))
            )
            .unwrap(),
            (0, "unknown".into())
        );
        // A different account has a distinct full source key.
        record_admin_intents(&db, "2:group:20:1", "2:group:20", 7, 0, &[8]).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM roulette_moderation", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            3
        );
    }
    #[test]
    fn amnesty_selection_is_bound_to_account_group_actor_epoch_and_original_permissions() {
        let db = database();
        db.execute_batch("CREATE TABLE messages(key TEXT PRIMARY KEY,scope TEXT,command_plugin TEXT,plugin_revision INTEGER,access_revision INTEGER);
            CREATE TABLE scope_access(scope TEXT PRIMARY KEY,enabled INTEGER,revision INTEGER);
            CREATE TABLE scope_plugin_access(scope TEXT,plugin TEXT,enabled INTEGER,revision INTEGER);
            INSERT INTO messages VALUES('1:group:20:1','1:group:20','mute_roulette',0,0);").unwrap();
        let create = || {
            db.execute("INSERT INTO roulette_amnesty_pending VALUES('1:group:20',7,'1:group:20:1','[8,9]',?1,12)",[now()+120]).unwrap()
        };
        create();
        assert!(consume_selection(&db, "2:group:20", 7, 12, &[8]).is_err());
        assert!(consume_selection(&db, "1:group:21", 7, 12, &[8]).is_err());
        assert!(consume_selection(&db, "1:group:20", 8, 12, &[8]).is_err());
        assert!(consume_selection(&db, "1:group:20", 7, 13, &[8]).is_err());
        assert_eq!(
            consume_selection(&db, "1:group:20", 7, 12, &[8, 99]).unwrap(),
            vec![8]
        );
        assert!(consume_selection(&db, "1:group:20", 7, 12, &[9]).is_err());
        create();
        db.execute(
            "INSERT INTO scope_plugin_access VALUES('1:group:20','mute_roulette',1,2)",
            [],
        )
        .unwrap();
        assert!(consume_selection(&db, "1:group:20", 7, 12, &[8]).is_err());
    }
}
