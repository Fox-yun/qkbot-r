//! Independent native implementations of the explicitly selected upstream plugin behaviors.
//! See docs/native-reference.md and assets/native for pinned data provenance.
use crate::{
    host::Host,
    plugin::{Plugin, PluginSpec},
    store::Store,
    types::{Incoming, OutgoingMessage, OutgoingSegment, Scene, now},
};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use chrono::{Datelike, NaiveDate, TimeZone};
use chrono_tz::Tz;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

const SCHEMA: &str = r#"
CREATE TABLE native_checkins (
 scope TEXT NOT NULL, actor_id INTEGER NOT NULL, day TEXT NOT NULL,
 timezone TEXT NOT NULL, created_at INTEGER NOT NULL,
 PRIMARY KEY(scope,actor_id,day)
);
CREATE TABLE native_wordle (
 scope TEXT NOT NULL, actor_id INTEGER NOT NULL, answer TEXT NOT NULL,
 guesses TEXT NOT NULL, status TEXT NOT NULL, started_at INTEGER NOT NULL,
 expires_at INTEGER NOT NULL,
 PRIMARY KEY(scope,actor_id)
);
CREATE TABLE native_kfc (
 scope TEXT NOT NULL, actor_id INTEGER NOT NULL, last_at INTEGER NOT NULL,
 PRIMARY KEY(scope,actor_id)
);
CREATE TABLE native_meals (
 scope TEXT NOT NULL, actor_id INTEGER NOT NULL, last_meal TEXT NOT NULL,
 PRIMARY KEY(scope,actor_id)
);
"#;

const COMPAT_SCHEMA: &str = r#"
CREATE TABLE deer_users(
 scope TEXT NOT NULL,user_id INTEGER NOT NULL,name TEXT NOT NULL,
 allow_help INTEGER NOT NULL DEFAULT 1,banned_until INTEGER,
 PRIMARY KEY(scope,user_id));
CREATE TABLE deer_days(
 scope TEXT NOT NULL,user_id INTEGER NOT NULL,day TEXT NOT NULL,count INTEGER NOT NULL,
 PRIMARY KEY(scope,user_id,day));
CREATE TABLE wordle_games(
 scope TEXT PRIMARY KEY,answer TEXT NOT NULL,meaning TEXT NOT NULL,dictionary TEXT NOT NULL,
 guesses TEXT NOT NULL,status TEXT NOT NULL,expires_at INTEGER NOT NULL,
 source TEXT NOT NULL,generation TEXT NOT NULL);
CREATE INDEX wordle_expiry ON wordle_games(status,expires_at);
CREATE TABLE native_receipts(
 plugin TEXT NOT NULL,source_key TEXT NOT NULL,reply TEXT NOT NULL,PRIMARY KEY(plugin,source_key));
CREATE TABLE meal_menu(
 bot_id INTEGER NOT NULL,kind TEXT NOT NULL,name TEXT NOT NULL,resource TEXT NOT NULL,
 local INTEGER NOT NULL DEFAULT 0,deleted INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(bot_id,kind,name));
CREATE TABLE meal_cooldown(bot_id INTEGER PRIMARY KEY,last_at INTEGER NOT NULL);
CREATE TABLE meal_usage(bot_id INTEGER NOT NULL,user_id INTEGER NOT NULL,day TEXT NOT NULL,count INTEGER NOT NULL,
 PRIMARY KEY(bot_id,user_id,day));
CREATE TABLE meal_pending(scope TEXT NOT NULL,user_id INTEGER NOT NULL,operation TEXT NOT NULL,
 name TEXT,kind TEXT,generation TEXT NOT NULL,expires_at INTEGER NOT NULL,PRIMARY KEY(scope,user_id));
"#;

pub async fn migrate(store: &Store) -> Result<()> {
    store.migrate("native", 1, SCHEMA).await?;
    store.migrate("native", 2, COMPAT_SCHEMA).await
}

/// Bounded housekeeping follows the host's raw-message retention. Historical
/// deer counts are user data and deliberately do not participate in this sweep.
pub async fn maintain(store: &Store, cutoff: i64) -> Result<()> {
    let usage_before = chrono::DateTime::from_timestamp(cutoff.saturating_sub(2 * 86400), 0)
        .context("清理日期超出范围")?
        .date_naive()
        .to_string();
    store.call(move |db| {
        let tx = db.transaction()?;
        tx.execute("DELETE FROM native_receipts WHERE rowid IN (SELECT r.rowid FROM native_receipts r WHERE NOT EXISTS(SELECT 1 FROM messages m WHERE m.key=r.source_key) LIMIT 500)", [])?;
        tx.execute("DELETE FROM meal_usage WHERE rowid IN (SELECT rowid FROM meal_usage WHERE day<?1 LIMIT 500)", [usage_before])?;
        tx.execute("DELETE FROM meal_pending WHERE rowid IN (SELECT rowid FROM meal_pending WHERE expires_at<=?1 LIMIT 500)", [now()])?;
        tx.commit()?;
        Ok(())
    }).await
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NativeView {
    Calendar {
        year: i32,
        month: u32,
        subject_id: i64,
        name: String,
        days: Vec<(u32, i64)>,
    },
    Rank {
        title: String,
        entries: Vec<(i64, String, i64)>,
    },
    Wordle {
        length: usize,
        attempts: usize,
        guesses: Vec<String>,
        marks: Vec<Vec<u8>>,
        hint: Option<String>,
    },
    Menu {
        kind: String,
        items: Vec<String>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Reply {
    text: String,
    view: Option<NativeView>,
    image: Option<String>,
    local_image: bool,
}
impl Reply {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            view: None,
            image: None,
            local_image: false,
        }
    }
    fn with_view(text: impl Into<String>, view: NativeView) -> Self {
        Self {
            text: text.into(),
            view: Some(view),
            image: None,
            local_image: false,
        }
    }
    async fn outgoing(self, host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
        let mut segments = Vec::new();
        if !self.text.is_empty() {
            segments.push(OutgoingSegment::Text { text: self.text });
        }
        if let Some(view) = self.view {
            segments.extend(crate::plugins::render::native(host, &view).await?.segments);
        }
        if let Some(image) = self.image {
            let uri = if self.local_image {
                host.media.url(host, message, "meal", &image).await?
            } else {
                // Existing menu rows retain their upstream URL as the stable
                // identifier. QQ receives a guarded local URL for the bundled
                // image, so a blocked external image host cannot break meals.
                host.media.bundled_meal(host, message, &image).await?
            };
            segments.push(OutgoingSegment::Image {
                uri,
                sub_type: Default::default(),
                summary: None,
            });
        }
        ensure!(!segments.is_empty(), "插件没有产生回复");
        Ok(OutgoingMessage { segments })
    }
}

fn spec(id: &'static str, commands: &'static [&'static str]) -> PluginSpec {
    PluginSpec {
        id,
        commands,
        tools: &[],
        dependencies: &["host"],
    }
}
fn command(text: &str) -> (&str, &str) {
    let text = text.trim().trim_start_matches('/');
    text.split_once(char::is_whitespace)
        .map(|(a, b)| (a, b.trim()))
        .unwrap_or((text, ""))
}
fn random_index(length: usize) -> usize {
    (uuid::Uuid::new_v4().as_u128() % length as u128) as usize
}
fn local_date(timestamp: i64, timezone: Tz) -> Result<NaiveDate> {
    Ok(timezone
        .timestamp_opt(timestamp, 0)
        .single()
        .context("时间超出范围")?
        .date_naive())
}

#[derive(Clone)]
struct Invocation {
    scope: String,
    actor: i64,
    source_key: String,
    plugin: &'static str,
    plugin_epoch: i64,
}
impl Invocation {
    fn from_message(host: &Host, message: &Incoming, plugin: &'static str) -> Result<Self> {
        Ok(Self {
            scope: message.key.scope.key(),
            actor: message.sender_id,
            source_key: message.key.key(),
            plugin,
            plugin_epoch: *host.epochs.get(plugin).context("插件尚未启用")?,
        })
    }
    fn validate(&self, tx: &Transaction<'_>) -> Result<()> {
        crate::catalog::check_message(tx, &self.source_key, self.plugin)?;
        crate::host::check_plugin(tx, self.plugin, self.plugin_epoch)?;
        let matches:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE key=?1 AND scope=?2 AND sender_id=?3 AND revoked=0)",params![self.source_key,self.scope,self.actor],|r|r.get(0))?;
        ensure!(matches, "指令来源已失效");
        Ok(())
    }
}
async fn transact<F>(store: &Store, inv: Invocation, operation: F) -> Result<Reply>
where
    F: FnOnce(&Transaction<'_>, &Invocation) -> Result<Reply> + Send + 'static,
{
    store
        .call(move |db| {
            let tx = db.transaction()?;
            inv.validate(&tx)?;
            let prior: Option<String> = tx
                .query_row(
                    "SELECT reply FROM native_receipts WHERE plugin=?1 AND source_key=?2",
                    params![inv.plugin, inv.source_key],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(prior) = prior {
                return Ok(serde_json::from_str(&prior)?);
            }
            let reply = operation(&tx, &inv)?;
            tx.execute(
                "INSERT INTO native_receipts(plugin,source_key,reply) VALUES(?1,?2,?3)",
                params![inv.plugin, inv.source_key, serde_json::to_string(&reply)?],
            )?;
            tx.commit()?;
            Ok(reply)
        })
        .await
}

#[derive(Default)]
pub struct CheckinPlugin;
const DEER_COMMANDS: &[&str] = &[
    "/checkin",
    "/签到",
    "/🦌",
    "/鹿",
    "/补🦌",
    "/补鹿",
    "/🦌历",
    "/鹿历",
    "/🦌榜",
    "/鹿榜",
    "/帮🦌",
    "/帮鹿",
    "/禁🦌",
    "/禁鹿",
    "/🦌帮助",
    "/鹿帮助",
];
const DEER_HELP: &str = "🦌 [@成员]：记录一次，可在同一天重复。\n补🦌 日期：补本月未记录的过去日期。\n🦌历 [@成员]：月历；🦌榜：本群前五。\n帮🦌 on/off [@成员]：允许或拒绝帮签，修改他人需群管理员。\n禁🦌 @成员 [时长]：群管理员设置，最长30天；省略时长解除。支持 30m、2h、1d、1h30m 或 HH:MM:SS。";
#[derive(Debug, Clone)]
enum DeerAction {
    Record,
    Backfill(u32),
    Calendar,
    Rank,
    HelpPolicy(bool),
    Ban(Option<i64>),
    Help,
}
fn duration_secs(value: &str) -> Result<i64> {
    if value.contains(':') {
        let parts = value
            .split(':')
            .map(str::parse::<i64>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            (2..=3).contains(&parts.len())
                && parts.iter().all(|n| *n >= 0)
                && parts.iter().skip(1).all(|n| *n < 60),
            "时长格式无效"
        );
        let result = parts
            .into_iter()
            .try_fold(0i64, |a, b| {
                a.checked_mul(60).and_then(|n| n.checked_add(b))
            })
            .context("时长过大")?;
        ensure!((1..=30 * 86400).contains(&result), "时长须为1秒到30天");
        return Ok(result);
    }
    let value = value.to_ascii_lowercase();
    let mut value = value.as_str();
    let mut total = 0i64;
    while !value.is_empty() {
        value = value.trim_start();
        let n = value.bytes().take_while(u8::is_ascii_digit).count();
        ensure!(n > 0, "时长格式无效");
        let amount = value[..n].parse::<i64>()?;
        value = &value[n..];
        let unit_len = value
            .chars()
            .take_while(|c| c.is_alphabetic())
            .map(char::len_utf8)
            .sum::<usize>();
        let unit = &value[..unit_len];
        value = &value[unit_len..];
        let factor = match unit {
            "s" | "sec" | "secs" | "second" | "seconds" | "秒" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" | "分钟" | "分" => 60,
            "h" | "hr" | "hour" | "hours" | "小时" | "时" => 3600,
            "d" | "day" | "days" | "天" => 86400,
            "w" | "week" | "weeks" | "周" => 604800,
            _ => bail!("时长格式无效"),
        };
        total = total
            .checked_add(amount.checked_mul(factor).context("时长过大")?)
            .context("时长过大")?;
    }
    ensure!((1..=30 * 86400).contains(&total), "时长须为1秒到30天");
    Ok(total)
}
fn deer_action(message: &Incoming) -> Result<DeerAction> {
    let (cmd, args) = command(&message.text);
    // Milky mentions are separate segments; numeric textual @ is never authority.
    let args = args
        .split_whitespace()
        .filter(|part| !part.starts_with('@'))
        .collect::<Vec<_>>();
    Ok(match cmd {
        "checkin" | "签到" | "🦌" | "鹿" => {
            ensure!(args.is_empty(), "用法：🦌 [@成员]");
            DeerAction::Record
        }
        "补🦌" | "补鹿" => {
            ensure!(args.len() == 1, "用法：补🦌 日期");
            DeerAction::Backfill(args[0].parse().context("日期应为数字")?)
        }
        "🦌历" | "鹿历" => DeerAction::Calendar,
        "🦌榜" | "鹿榜" => DeerAction::Rank,
        "帮🦌" | "帮鹿" => {
            ensure!(args.len() == 1, "用法：帮🦌 on/off [@成员]");
            DeerAction::HelpPolicy(match args[0] {
                "on" => true,
                "off" => false,
                _ => bail!("请使用 on 或 off"),
            })
        }
        "禁🦌" | "禁鹿" => DeerAction::Ban(if args.is_empty() {
            None
        } else {
            Some(duration_secs(&args.join(""))?)
        }),
        _ => DeerAction::Help,
    })
}
// Keep authorization and the injected calendar/clock explicit at the one
// transactional mutation boundary, including deterministic replay tests.
#[allow(clippy::too_many_arguments)]
fn deer_apply(
    tx: &Transaction<'_>,
    inv: &Invocation,
    action: DeerAction,
    subject: i64,
    name: &str,
    admin: bool,
    group: bool,
    date: NaiveDate,
    timestamp: i64,
) -> Result<Reply> {
    if matches!(action, DeerAction::Help) {
        return Ok(Reply::text(DEER_HELP));
    }
    ensure!(subject == inv.actor || group, "只能在群聊中指定其他成员");
    if matches!(action, DeerAction::Rank) {
        ensure!(group, "排行榜仅在群聊使用");
        let prefix = date.format("%Y-%m").to_string();
        let entries=tx.prepare("SELECT d.user_id,COALESCE(u.name,CAST(d.user_id AS TEXT)),SUM(d.count) total FROM deer_days d LEFT JOIN deer_users u ON u.scope=d.scope AND u.user_id=d.user_id WHERE d.scope=?1 AND substr(d.day,1,7)=?2 GROUP BY d.user_id ORDER BY total DESC,d.user_id LIMIT 5")?
            .query_map(params![inv.scope,prefix],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        return Ok(Reply::with_view(
            "",
            NativeView::Rank {
                title: format!("{prefix} 本群🦌榜"),
                entries,
            },
        ));
    }
    tx.execute("INSERT INTO deer_users(scope,user_id,name) VALUES(?1,?2,?3) ON CONFLICT(scope,user_id) DO UPDATE SET name=excluded.name",params![inv.scope,subject,name])?;
    let (allow_help, banned): (bool, Option<i64>) = tx.query_row(
        "SELECT allow_help,banned_until FROM deer_users WHERE scope=?1 AND user_id=?2",
        params![inv.scope, subject],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let text = match action {
        DeerAction::HelpPolicy(allowed) => {
            ensure!(group, "帮签开关仅在群聊使用");
            ensure!(
                subject == inv.actor || admin,
                "修改他人帮签开关需要群管理员"
            );
            tx.execute(
                "UPDATE deer_users SET allow_help=?3 WHERE scope=?1 AND user_id=?2",
                params![inv.scope, subject, allowed],
            )?;
            return Ok(Reply::text(format!(
                "已{}其他人帮 {name} 🦌。",
                if allowed { "允许" } else { "禁止" }
            )));
        }
        DeerAction::Ban(seconds) => {
            ensure!(group && admin, "禁签管理需要群管理员");
            let until = seconds.map(|s| timestamp.saturating_add(s));
            tx.execute(
                "UPDATE deer_users SET banned_until=?3 WHERE scope=?1 AND user_id=?2",
                params![inv.scope, subject, until],
            )?;
            return Ok(Reply::text(match until {
                Some(until) => format!("已限制 {name} 的🦌签到，截止 Unix 时间 {until}。"),
                None => format!("已解除 {name} 的🦌签到限制。"),
            }));
        }
        DeerAction::Record => {
            ensure!(subject == inv.actor || allow_help, "该成员不允许别人帮签");
            ensure!(
                !group || banned.is_none_or(|until| until <= timestamp),
                "该成员正在禁签期"
            );
            tx.execute("INSERT INTO deer_days(scope,user_id,day,count) VALUES(?1,?2,?3,1) ON CONFLICT(scope,user_id,day) DO UPDATE SET count=count+1",params![inv.scope,subject,date.to_string()])?;
            if subject == inv.actor {
                "成功🦌了".to_owned()
            } else {
                format!("成功帮 {name} 🦌了")
            }
        }
        DeerAction::Backfill(day) => {
            ensure!(subject == inv.actor, "补签只操作本人");
            ensure!(day >= 1 && day < date.day(), "只能补本月早于今天的日期");
            let past = date.with_day(day).context("日期无效")?;
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO deer_days(scope,user_id,day,count) VALUES(?1,?2,?3,1)",
                params![inv.scope, subject, past.to_string()],
            )?;
            if inserted == 1 {
                "成功补🦌".into()
            } else {
                "这个日期已经有记录，不能重复补签".into()
            }
        }
        DeerAction::Calendar => String::new(),
        DeerAction::Rank | DeerAction::Help => unreachable!(),
    };
    let prefix = date.format("%Y-%m").to_string();
    let days=tx.prepare("SELECT CAST(substr(day,9,2) AS INTEGER),count FROM deer_days WHERE scope=?1 AND user_id=?2 AND substr(day,1,7)=?3 ORDER BY day")?
        .query_map(params![inv.scope,subject,prefix],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Reply::with_view(
        text,
        NativeView::Calendar {
            year: date.year(),
            month: date.month(),
            subject_id: subject,
            name: name.into(),
            days,
        },
    ))
}
impl CheckinPlugin {
    async fn run(&self, host: &Host, message: &Incoming) -> Result<Reply> {
        let action = deer_action(message)?;
        let targets = message
            .mentions
            .iter()
            .copied()
            .filter(|id| *id != message.key.scope.bot_id)
            .collect::<Vec<_>>();
        ensure!(targets.len() <= 1, "一次只能指定一个成员");
        if matches!(action, DeerAction::Ban(_)) {
            ensure!(targets.len() == 1, "禁🦌需要 @ 一名成员");
        }
        let subject = targets.first().copied().unwrap_or(message.sender_id);
        let group = message.key.scope.scene == Scene::Group;
        let mut name = message
            .sender_card
            .as_ref()
            .or(message.sender_nickname.as_ref())
            .cloned()
            .unwrap_or_else(|| subject.to_string());
        if subject != message.sender_id {
            ensure!(group, "指定成员需要群聊");
            let member = host
                .milky
                .get_member(&message.key.scope, subject, false)
                .await?;
            name = if member.card.is_empty() {
                member.nickname
            } else {
                member.card
            };
        }
        let needs_admin = matches!(action, DeerAction::Ban(_))
            || matches!(action, DeerAction::HelpPolicy(_)) && subject != message.sender_id;
        let admin = if needs_admin {
            ensure!(group, "此操作需要群聊");
            matches!(
                host.milky
                    .get_member(&message.key.scope, message.sender_id, true)
                    .await?
                    .role
                    .as_str(),
                "owner" | "admin"
            )
        } else {
            false
        };
        let date = local_date(message.received_at, host.config.app.timezone.parse()?)?;
        let timestamp = message.received_at;
        transact(
            &host.store,
            Invocation::from_message(host, message, "checkin")?,
            move |tx, inv| {
                deer_apply(
                    tx, inv, action, subject, &name, admin, group, date, timestamp,
                )
            },
        )
        .await
    }
}
#[async_trait]
impl Plugin for CheckinPlugin {
    fn spec(&self) -> PluginSpec {
        spec("checkin", DEER_COMMANDS)
    }
    fn claims(&self, message: &Incoming) -> bool {
        let (cmd, _) = command(&message.text);
        DEER_COMMANDS
            .iter()
            .any(|c| c.trim_start_matches('/') == cmd)
    }
    async fn command(&self, host: &Host, message: &Incoming) -> Result<String> {
        Ok(self.run(host, message).await?.text)
    }
    async fn reply(&self, host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
        self.run(host, message).await?.outgoing(host, message).await
    }
}

#[derive(Default)]
pub struct WordlePlugin;
const WORDLE_COMMANDS: &[&str] = &[
    "/wordle",
    "/猜词",
    "/猜单词",
    "/wordle_hint",
    "/提示",
    "/猜单词提示",
    "/wordle_stop",
    "/结束",
    "/结束游戏",
    "/结束猜单词",
];
const DICTIONARIES: &[(&str, &str)] = &[
    ("CET4", include_str!("../../assets/native/wordle/CET4.json")),
    ("CET6", include_str!("../../assets/native/wordle/CET6.json")),
    ("GRE", include_str!("../../assets/native/wordle/GRE.json")),
    ("GMAT", include_str!("../../assets/native/wordle/GMAT.json")),
    (
        "TOEFL",
        include_str!("../../assets/native/wordle/TOEFL.json"),
    ),
    ("SAT", include_str!("../../assets/native/wordle/SAT.json")),
    (
        "IELTS",
        include_str!("../../assets/native/wordle/IELTS.json"),
    ),
    ("专四", include_str!("../../assets/native/wordle/专四.json")),
    ("专八", include_str!("../../assets/native/wordle/专八.json")),
    ("考研", include_str!("../../assets/native/wordle/考研.json")),
];
const WORDLE_HELP: &str = "wordle [-l 3..8] [-d CET4]：本群共同猜词（私聊独立），默认五字母、六次机会；次数为长度加一。\n直接发送英文单词猜测；提示 / 猜单词提示；结束 / 结束猜单词。\n每次猜测或提示续期300秒；非法、重复词不扣次数。\n词库：CET4、CET6、GRE、GMAT、TOEFL、SAT、IELTS、专四、专八、考研。";
fn legal_word(word: &str) -> bool {
    static LEGAL: OnceLock<Vec<&'static str>> = OnceLock::new();
    LEGAL
        .get_or_init(|| {
            include_str!("../../assets/native/wordle/legal.txt")
                .lines()
                .collect()
        })
        .binary_search(&word)
        .is_ok()
}
fn random_word(dictionary: &str, length: usize) -> Result<(String, String)> {
    let text = DICTIONARIES
        .iter()
        .find(|(name, _)| *name == dictionary)
        .map(|(_, text)| *text)
        .context("词库不存在；支持 CET4、CET6、GRE、GMAT、TOEFL、SAT、IELTS、专四、专八、考研")?;
    let words: BTreeMap<String, String> = serde_json::from_str(text)?;
    let words = words
        .into_iter()
        .filter(|(word, _)| word.len() == length)
        .collect::<Vec<_>>();
    ensure!(!words.is_empty(), "此词库没有对应长度的单词");
    Ok(words[random_index(words.len())].clone())
}
#[derive(Debug)]
enum WordleAction {
    Open { length: usize, dictionary: String },
    Guess(String),
    Hint,
    Stop,
    Help,
}
fn wordle_action(text: &str) -> Result<WordleAction> {
    let (cmd, args) = command(text);
    if matches!(cmd, "wordle_hint" | "提示" | "猜单词提示") {
        return Ok(WordleAction::Hint);
    }
    if matches!(cmd, "wordle_stop" | "结束" | "结束游戏" | "结束猜单词") {
        return Ok(WordleAction::Stop);
    }
    if !matches!(cmd, "wordle" | "猜词" | "猜单词") {
        return Ok(WordleAction::Guess(text.trim().to_ascii_lowercase()));
    }
    if matches!(args, "help" | "帮助") {
        return Ok(WordleAction::Help);
    }
    if matches!(args, "end" | "stop" | "giveup" | "放弃" | "结束") {
        return Ok(WordleAction::Stop);
    }
    if matches!(args, "hint" | "提示") {
        return Ok(WordleAction::Hint);
    }
    if !args.is_empty()
        && args.bytes().all(|b| b.is_ascii_alphabetic())
        && !matches!(args, "start" | "new")
    {
        return Ok(WordleAction::Guess(args.to_ascii_lowercase()));
    }
    let mut length = 5;
    let mut dictionary = "CET4".to_owned();
    let mut args = args.split_whitespace();
    while let Some(arg) = args.next() {
        match arg {
            "start" | "new" | "开始" => {}
            "-l" | "--length" => {
                length = args
                    .next()
                    .context("请提供单词长度")?
                    .parse()
                    .context("单词长度应为3到8")?
            }
            "-d" | "--dictionary" => dictionary = args.next().context("请提供词库名")?.to_owned(),
            _ => bail!("未知参数；用法：wordle -l 5 -d CET4"),
        }
    }
    ensure!((3..=8).contains(&length), "单词长度应为3到8");
    Ok(WordleAction::Open { length, dictionary })
}
#[derive(Debug)]
struct Game {
    answer: String,
    meaning: String,
    guesses: Vec<String>,
    expires_at: i64,
    status: String,
}
fn game(tx: &Transaction<'_>, scope: &str) -> Result<Option<Game>> {
    let row: Option<(String, String, String, i64, String)> = tx
        .query_row(
            "SELECT answer,meaning,guesses,expires_at,status FROM wordle_games WHERE scope=?1",
            [scope],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    row.map(|(answer, meaning, guesses, expires_at, status)| {
        let guesses: Vec<String> = serde_json::from_str(&guesses)?;
        ensure!(
            (3..=8).contains(&answer.len())
                && answer.bytes().all(|b| b.is_ascii_lowercase())
                && guesses.len() <= answer.len() + 1
                && guesses
                    .iter()
                    .all(|w| w.len() == answer.len() && w.bytes().all(|b| b.is_ascii_lowercase())),
            "游戏记录损坏"
        );
        Ok(Game {
            answer,
            meaning,
            guesses,
            expires_at,
            status,
        })
    })
    .transpose()
}
fn marks(answer: &str, guess: &str) -> Vec<u8> {
    let answer = answer.as_bytes();
    let guess = guess.as_bytes();
    let mut result = vec![0; answer.len()];
    let mut remaining = [0usize; 26];
    for i in 0..answer.len() {
        if answer[i] == guess[i] {
            result[i] = 2
        } else {
            remaining[(answer[i] - b'a') as usize] += 1
        }
    }
    for i in 0..guess.len() {
        let slot = &mut remaining[(guess[i] - b'a') as usize];
        if result[i] != 2 && *slot > 0 {
            result[i] = 1;
            *slot -= 1
        }
    }
    result
}
fn board(game: &Game, hint: Option<String>) -> NativeView {
    NativeView::Wordle {
        length: game.answer.len(),
        attempts: game.answer.len() + 1,
        guesses: game.guesses.clone(),
        marks: game
            .guesses
            .iter()
            .map(|guess| marks(&game.answer, guess))
            .collect(),
        hint,
    }
}
fn answer(game: &Game) -> String {
    format!("答案：{}\n{}", game.answer, game.meaning)
}
fn wordle_apply(
    tx: &Transaction<'_>,
    inv: &Invocation,
    action: WordleAction,
    timestamp: i64,
    source: &str,
    selected: Option<(String, String)>,
) -> Result<Reply> {
    if matches!(action, WordleAction::Help) {
        return Ok(Reply::text(WORDLE_HELP));
    }
    let current = game(tx, &inv.scope)?;
    let active = current.filter(|game| game.status == "active" && game.expires_at > timestamp);
    if let WordleAction::Open { length, dictionary } = action {
        if let Some(game) = active {
            return Ok(Reply::with_view(
                "本会话已有进行中的游戏，直接发送单词猜测。",
                board(&game, None),
            ));
        }
        let (answer, meaning) = selected.context("未选择答案")?;
        ensure!(answer.len() == length, "答案长度不匹配");
        tx.execute("INSERT INTO wordle_games(scope,answer,meaning,dictionary,guesses,status,expires_at,source,generation) VALUES(?1,?2,?3,?4,'[]','active',?5,?6,?7) ON CONFLICT(scope) DO UPDATE SET answer=excluded.answer,meaning=excluded.meaning,dictionary=excluded.dictionary,guesses='[]',status='active',expires_at=excluded.expires_at,source=excluded.source,generation=excluded.generation",
            params![inv.scope,answer,meaning,dictionary,timestamp+300,source,uuid::Uuid::new_v4().to_string()])?;
        let game = Game {
            answer,
            meaning,
            guesses: vec![],
            expires_at: timestamp + 300,
            status: "active".into(),
        };
        return Ok(Reply::with_view(
            format!(
                "有 {} 次机会，单词长度 {}；本会话共同猜词，直接发送英文单词。",
                length + 1,
                length
            ),
            board(&game, None),
        ));
    }
    let Some(mut game) = active else {
        return Ok(Reply::text("当前没有进行中的游戏；发送 /wordle 开始。"));
    };
    match action {
        WordleAction::Stop => {
            tx.execute(
                "UPDATE wordle_games SET status='stopped' WHERE scope=?1",
                [&inv.scope],
            )?;
            Ok(Reply::text(if game.guesses.is_empty() {
                "游戏已结束".into()
            } else {
                format!("游戏已结束\n{}", answer(&game))
            }))
        }
        WordleAction::Hint => {
            tx.execute(
                "UPDATE wordle_games SET expires_at=?2,source=?3 WHERE scope=?1",
                params![inv.scope, timestamp + 300, source],
            )?;
            let hint: String = game
                .answer
                .chars()
                .map(|ch| {
                    if game.guesses.iter().any(|word| word.contains(ch)) {
                        ch
                    } else {
                        '*'
                    }
                })
                .collect();
            if hint.chars().all(|ch| ch == '*') {
                Ok(Reply::text("还没有猜中过任何字母，再试试吧。"))
            } else {
                Ok(Reply::with_view("", board(&game, Some(hint))))
            }
        }
        WordleAction::Guess(guess) => {
            ensure!(
                guess.len() == game.answer.len() && guess.bytes().all(|b| b.is_ascii_lowercase()),
                "请输入长度匹配的英文单词"
            );
            tx.execute(
                "UPDATE wordle_games SET expires_at=?2,source=?3 WHERE scope=?1",
                params![inv.scope, timestamp + 300, source],
            )?;
            if game.guesses.contains(&guess) {
                return Ok(Reply::text("已经猜过这个单词；本次不扣次数。"));
            }
            if guess != game.answer && !legal_word(&guess) {
                return Ok(Reply::text(format!(
                    "{guess} 不在合法英文词典中；本次不扣次数。"
                )));
            }
            let won = guess == game.answer;
            game.guesses.push(guess);
            let lost = !won && game.guesses.len() == game.answer.len() + 1;
            let status = if won {
                "won"
            } else if lost {
                "lost"
            } else {
                "active"
            };
            tx.execute(
                "UPDATE wordle_games SET guesses=?2,status=?3 WHERE scope=?1",
                params![inv.scope, serde_json::to_string(&game.guesses)?, status],
            )?;
            let text = if won {
                format!("恭喜 {} 猜对了！\n{}", inv.actor, answer(&game))
            } else if lost {
                format!("机会已用完。\n{}", answer(&game))
            } else {
                format!(
                    "剩余 {} 次机会。",
                    game.answer.len() + 1 - game.guesses.len()
                )
            };
            Ok(Reply::with_view(text, board(&game, None)))
        }
        WordleAction::Open { .. } | WordleAction::Help => unreachable!(),
    }
}
impl WordlePlugin {
    async fn run(&self, host: &Host, message: &Incoming) -> Result<Reply> {
        let action = wordle_action(&message.text)?;
        let selected = if let WordleAction::Open { length, dictionary } = &action {
            Some(random_word(dictionary, *length)?)
        } else {
            None
        };
        let source = serde_json::to_string(message)?;
        let timestamp = now();
        transact(
            &host.store,
            Invocation::from_message(host, message, "wordle")?,
            move |tx, inv| wordle_apply(tx, inv, action, timestamp, &source, selected),
        )
        .await
    }
}
#[async_trait]
impl Plugin for WordlePlugin {
    fn spec(&self) -> PluginSpec {
        spec("wordle", WORDLE_COMMANDS)
    }
    fn claims(&self, message: &Incoming) -> bool {
        let (cmd, _) = command(&message.text);
        if matches!(cmd, "wordle" | "猜词" | "猜单词")
            && message.key.scope.scene == Scene::Group
            && !message.text.trim_start().starts_with('/')
            && !message.mentions.contains(&message.key.scope.bot_id)
        {
            return false;
        }
        WORDLE_COMMANDS
            .iter()
            .any(|c| c.trim_start_matches('/') == cmd)
    }
    async fn handles_message(&self, host: &Host, message: &Incoming) -> Result<bool> {
        if self.claims(message) {
            return Ok(true);
        }
        let word = message.text.trim();
        if !(3..=8).contains(&word.len()) || !word.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Ok(false);
        }
        let scope = message.key.scope.key();
        let length = word.len() as i64;
        host.store.call(move|db|Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM wordle_games WHERE scope=?1 AND status='active' AND expires_at>?2 AND length(answer)=?3)",params![scope,now(),length],|r|r.get(0))?)).await
    }
    async fn command(&self, host: &Host, message: &Incoming) -> Result<String> {
        Ok(self.run(host, message).await?.text)
    }
    async fn reply(&self, host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
        self.run(host, message).await?.outgoing(host, message).await
    }
    async fn tick(&self, host: &Arc<Host>) -> Result<()> {
        let Some(bot_id) = host.milky.verified_bot_id() else {
            return Ok(());
        };
        let expired=host.store.call(move|db|{
            let tx=db.transaction()?;
            let rows=tx.prepare("SELECT scope,answer,meaning,guesses,source,generation FROM wordle_games WHERE status='active' AND expires_at<=?1 AND scope LIKE ?2 ORDER BY expires_at LIMIT 16")?
                .query_map(params![now(),format!("{bot_id}:%")],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for row in &rows{tx.execute("UPDATE wordle_games SET status='expired' WHERE scope=?1 AND generation=?2",params![row.0,row.5])?;}
            tx.commit()?;Ok(rows)
        }).await?;
        for (scope, answer, meaning, guesses, source, generation) in expired {
            let result = async {
                let source: Incoming = serde_json::from_str(&source)?;
                ensure!(source.key.scope.key() == scope, "游戏来源会话不匹配");
                let text = if guesses == "[]" {
                    "猜单词超时，游戏结束。".into()
                } else {
                    format!("猜单词超时，游戏结束。\n答案：{answer}\n{meaning}")
                };
                host.send_plugin_reply(
                    &source,
                    "wordle",
                    OutgoingMessage::text(text),
                    format!("wordle-timeout:{generation}"),
                )
                .await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if result.is_err() {
                tracing::warn!(scope, "Wordle timeout notification could not be delivered");
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct KfcPlugin;
fn kfc_day(text: &str) -> Option<Result<usize>> {
    let (cmd, args) = command(text);
    if cmd == "kfc" {
        return Some(if args.is_empty() {
            Ok(3)
        } else {
            weekday(args)
        });
    }
    let value = text.trim().trim_start_matches('/');
    if let Some(day) = value
        .strip_prefix("疯狂星期")
        .filter(|s| s.chars().count() == 1)
    {
        return Some(weekday(day));
    }
    if let Some(day) = value
        .strip_prefix("狂乱")
        .and_then(|s| s.strip_suffix("曜日"))
        .filter(|s| s.chars().count() == 1)
    {
        return Some(weekday(day));
    }
    None
}
fn weekday(day: &str) -> Result<usize> {
    match day {
        "一" | "月" => Ok(0),
        "二" | "火" => Ok(1),
        "三" | "水" => Ok(2),
        "四" | "木" => Ok(3),
        "五" | "金" => Ok(4),
        "六" | "土" => Ok(5),
        "日" | "天" => Ok(6),
        _ => bail!("请指定准确星期：一到日，或月火水木金土日。"),
    }
}
fn kfc_post(day: usize, text: &str) -> String {
    let jp = ["月", "火", "水", "木", "金", "土", "日"];
    let cn = ["一", "二", "三", "四", "五", "六", "日"];
    let en = [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];
    text.replace("木曜日", &format!("{}曜日", jp[day]))
        .replace("Thursday", en[day])
        .replace("thursday", en[day])
        .replace("星期四", &format!("星期{}", cn[day]))
        .replace("周四", &format!("周{}", cn[day]))
        .replace("礼拜四", &format!("礼拜{}", cn[day]))
}
#[async_trait]
impl Plugin for KfcPlugin {
    fn spec(&self) -> PluginSpec {
        spec("kfc", &["/kfc", "/疯狂星期四"])
    }
    fn claims(&self, message: &Incoming) -> bool {
        kfc_day(&message.text).is_some()
    }
    async fn command(&self, host: &Host, message: &Incoming) -> Result<String> {
        let day = kfc_day(&message.text).context("用法：疯狂星期四 / 狂乱木曜日")??;
        let data: serde_json::Value =
            serde_json::from_str(include_str!("../../assets/native/kfc/post.json"))?;
        let posts = data["post"].as_array().context("文案库格式错误")?;
        ensure!(!posts.is_empty(), "文案库为空");
        let text = kfc_post(
            day,
            posts[random_index(posts.len())]
                .as_str()
                .context("文案库格式错误")?,
        );
        Ok(transact(
            &host.store,
            Invocation::from_message(host, message, "kfc")?,
            move |_, _| Ok(Reply::text(text)),
        )
        .await?
        .text)
    }
}

pub struct MealPlugin;
impl MealPlugin {
    /// The old text-only menu is accepted only when empty. Images are managed
    /// through the selected plugin's own add/delete commands.
    pub fn new(legacy_items: Vec<String>) -> Result<Self> {
        ensure!(
            legacy_items.is_empty(),
            "图片餐单不接受纯文字 meal_items；请使用添加菜单指令"
        );
        Ok(Self)
    }
}
pub fn default_meals() -> Vec<String> {
    vec![]
}
const MEAL_COMMANDS: &[&str] = &[
    "/meal",
    "/吃什么",
    "/今天吃什么",
    "/今天喝什么",
    "/全部菜单",
    "/查看菜单",
    "/查看菜品",
    "/添加菜单",
    "/删除菜单",
    "/早上吃什么",
    "/早饭吃什么",
    "/早餐吃什么",
    "/中午吃什么",
    "/午饭吃什么",
    "/午餐吃什么",
    "/晚上吃什么",
    "/晚饭吃什么",
    "/晚餐吃什么",
    "/今晚吃什么",
];
#[derive(Debug, Clone, Deserialize)]
struct MealEntry {
    kind: String,
    name: String,
    url: String,
}
fn default_menu() -> Result<Vec<MealEntry>> {
    Ok(serde_json::from_str(include_str!(
        "../../assets/native/meal/menu.json"
    ))?)
}
fn meal_kind(value: &str) -> Result<String> {
    match value.trim() {
        "eat" | "菜品" | "菜单" => Ok("eat".into()),
        "drink" | "饮料" | "饮品" => Ok("drink".into()),
        _ => bail!("菜单类型应为 菜品 或 饮品"),
    }
}
#[derive(Debug, Clone)]
struct PendingMeal {
    operation: String,
    name: Option<String>,
    kind: Option<String>,
    generation: String,
}
fn read_pending(
    db: &rusqlite::Connection,
    scope: &str,
    actor: i64,
    timestamp: i64,
) -> Result<Option<PendingMeal>> {
    Ok(db.query_row("SELECT operation,name,kind,generation FROM meal_pending WHERE scope=?1 AND user_id=?2 AND expires_at>?3",params![scope,actor,timestamp],|r|Ok(PendingMeal{operation:r.get(0)?,name:r.get(1)?,kind:r.get(2)?,generation:r.get(3)?})).optional()?)
}
fn meal_command(text: &str) -> Option<&'static str> {
    let (cmd, _) = command(text);
    match cmd {
        "meal" => Some("eat"),
        "全部菜单" | "查看菜单" | "查看菜品" => Some("view"),
        "添加菜单" => Some("add"),
        "删除菜单" => Some("delete"),
        _ => {
            // Explicit slash commands retain ownership of invalid arguments so
            // the handler can show its usage. Bare requests must match in full:
            // ordinary conversation beginning with "今天吃什么" is not a command.
            let value = if text.trim_start().starts_with('/') {
                cmd
            } else {
                text.trim()
            };
            match value {
                "吃什么" | "今天吃什么" => return Some("eat"),
                "今天喝什么" => return Some("drink"),
                _ => {}
            }
            // Match the entire anchored upstream shortcut. Splitting at the
            // first verb would also claim prose such as "早上吃完再想吃什么".
            for (suffix, operation) in [
                ("吃什么", "eat"),
                ("吃啥", "eat"),
                ("吃点啥", "eat"),
                ("喝什么", "drink"),
                ("喝啥", "drink"),
                ("喝点啥", "drink"),
            ] {
                if let Some(prefix) = value.strip_suffix(suffix)
                    && meal_time_prefix(prefix)
                {
                    return Some(operation);
                }
            }
            None
        }
    }
}
fn meal_time_prefix(prefix: &str) -> bool {
    // Equivalent to the selected plugin's anchored shortcut prefix:
    // [今|明|后]?[天|日]?(早|中|晚)?(上|午|餐|饭|夜宵|宵夜|早|晚)
    // Test both optional branches (the required alternative may itself be 早/晚).
    let mut prefixes = vec![prefix];
    if let Some(first) = prefix.chars().next()
        && matches!(first, '今' | '明' | '后' | '|')
    {
        prefixes.push(&prefix[first.len_utf8()..]);
    }
    let ends = ["上", "午", "餐", "饭", "夜宵", "宵夜", "早", "晚"];
    for prefix in prefixes {
        let mut days = vec![prefix];
        if let Some(first) = prefix.chars().next()
            && matches!(first, '天' | '日' | '|')
        {
            days.push(&prefix[first.len_utf8()..]);
        }
        for prefix in days {
            // Also accept the common lunch names absent from the upstream
            // regex's optional 早/中/晚 branch.
            if ends.contains(&prefix) || matches!(prefix, "午饭" | "午餐") {
                return true;
            }
            if let Some(first) = prefix.chars().next()
                && matches!(first, '早' | '中' | '晚')
                && ends.contains(&&prefix[first.len_utf8()..])
            {
                return true;
            }
        }
    }
    false
}
fn menu_seed(tx: &Transaction<'_>, bot_id: i64) -> Result<()> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM meal_menu WHERE bot_id=?1)",
        [bot_id],
        |r| r.get(0),
    )?;
    if !exists {
        for entry in default_menu()? {
            tx.execute(
                "INSERT OR IGNORE INTO meal_menu(bot_id,kind,name,resource) VALUES(?1,?2,?3,?4)",
                params![bot_id, entry.kind, entry.name, entry.url],
            )?;
        }
    }
    Ok(())
}
#[derive(Debug)]
struct MealRequest {
    pending: PendingMeal,
    expected: Option<String>,
    bot_id: i64,
    timestamp: i64,
    usage_day: String,
    image: Option<String>,
    cooldown: i64,
    daily_limit: i64,
}
fn meal_apply(tx: &Transaction<'_>, inv: &Invocation, request: MealRequest) -> Result<Reply> {
    let MealRequest {
        pending,
        expected,
        bot_id,
        timestamp,
        usage_day,
        image,
        cooldown,
        daily_limit,
    } = request;
    if let Some(expected) = expected {
        ensure!(
            read_pending(tx, &inv.scope, inv.actor, timestamp)?
                .is_some_and(|p| p.generation == expected),
            "菜单会话已变化，请重新发起指令"
        );
    }
    menu_seed(tx, bot_id)?;
    let operation = pending.operation.as_str();
    if matches!(operation, "eat" | "drink") {
        let last: Option<i64> = tx
            .query_row(
                "SELECT last_at FROM meal_cooldown WHERE bot_id=?1",
                [bot_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(remaining) = last
            .map(|last| last.saturating_add(cooldown).saturating_sub(timestamp))
            .filter(|r| *r > 0)
        {
            return Ok(Reply::text(format!("冷却中，请 {remaining} 秒后再试。")));
        }
        let used: i64 = tx
            .query_row(
                "SELECT count FROM meal_usage WHERE bot_id=?1 AND user_id=?2 AND day=?3",
                params![bot_id, inv.actor, usage_day],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if daily_limit > 0 && used >= daily_limit {
            return Ok(Reply::text("今天的吃喝推荐次数已用完，早上八点重置。"));
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM meal_menu WHERE bot_id=?1 AND kind=?2 AND deleted=0",
            params![bot_id, operation],
            |r| r.get(0),
        )?;
        ensure!(count > 0, "此类型菜单为空，请管理员添加带图片的条目");
        let (name,resource,local):(String,String,bool)=tx.query_row("SELECT name,resource,local FROM meal_menu WHERE bot_id=?1 AND kind=?2 AND deleted=0 ORDER BY name LIMIT 1 OFFSET ?3",params![bot_id,operation,random_index(count as usize)as i64],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        tx.execute("INSERT INTO meal_cooldown(bot_id,last_at) VALUES(?1,?2) ON CONFLICT(bot_id) DO UPDATE SET last_at=excluded.last_at",params![bot_id,timestamp])?;
        tx.execute("INSERT INTO meal_usage(bot_id,user_id,day,count) VALUES(?1,?2,?3,1) ON CONFLICT(bot_id,user_id,day) DO UPDATE SET count=count+1",params![bot_id,inv.actor,usage_day])?;
        return Ok(Reply {
            text: format!(
                "小鱼建议你{}：{name}",
                if operation == "eat" { "吃" } else { "喝" }
            ),
            view: None,
            image: Some(resource),
            local_image: local,
        });
    }
    let need_name = matches!(operation, "add" | "delete") && pending.name.is_none();
    let prompt = if need_name {
        Some("请发送菜品或饮品名称。")
    } else if pending.kind.is_none() {
        Some("请发送菜单类型：菜品 或 饮品。")
    } else if operation == "add" && image.is_none() {
        Some("请发送要添加的图片。")
    } else {
        None
    };
    if let Some(prompt) = prompt {
        tx.execute("INSERT INTO meal_pending(scope,user_id,operation,name,kind,generation,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(scope,user_id) DO UPDATE SET operation=excluded.operation,name=excluded.name,kind=excluded.kind,generation=excluded.generation,expires_at=excluded.expires_at",params![inv.scope,inv.actor,pending.operation,pending.name,pending.kind,uuid::Uuid::new_v4().to_string(),timestamp+600])?;
        return Ok(Reply::text(format!(
            "{prompt}\n十分钟内有效，发送“取消菜单”可取消。"
        )));
    }
    tx.execute(
        "DELETE FROM meal_pending WHERE scope=?1 AND user_id=?2",
        params![inv.scope, inv.actor],
    )?;
    let kind = pending.kind.context("缺少菜单类型")?;
    match operation {
        "view" => {
            let items=tx.prepare("SELECT name FROM meal_menu WHERE bot_id=?1 AND kind=?2 AND deleted=0 ORDER BY name")?.query_map(params![bot_id,kind],|r|r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(Reply::with_view("", NativeView::Menu { kind, items }))
        }
        "add" => {
            let name = pending.name.context("缺少名称")?;
            let resource = image.context("缺少图片")?;
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM meal_menu WHERE bot_id=?1 AND deleted=0",
                [bot_id],
                |r| r.get(0),
            )?;
            let existing:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM meal_menu WHERE bot_id=?1 AND kind=?2 AND name=?3 AND deleted=0)",params![bot_id,kind,name],|r|r.get(0))?;
            ensure!(
                existing || count < 2000,
                "菜单最多2000项，请先删除不用的条目"
            );
            tx.execute("INSERT INTO meal_menu(bot_id,kind,name,resource,local,deleted) VALUES(?1,?2,?3,?4,1,0) ON CONFLICT(bot_id,kind,name) DO UPDATE SET resource=excluded.resource,local=1,deleted=0",params![bot_id,kind,name,resource])?;
            Ok(Reply::text(format!("成功添加 {name}。")))
        }
        "delete" => {
            let name = pending.name.context("缺少名称")?;
            let deleted=tx.execute("UPDATE meal_menu SET deleted=1 WHERE bot_id=?1 AND kind=?2 AND name=?3 AND deleted=0",params![bot_id,kind,name])?;
            Ok(Reply::text(if deleted == 1 {
                format!("已删除 {name}。")
            } else {
                "菜单中没有这个条目。".into()
            }))
        }
        _ => bail!("菜单指令无效"),
    }
}
fn validate_meal_name(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty()
            && name.chars().count() <= 80
            && !name.chars().any(char::is_control),
        "名称须为1到80字符且不含控制字符"
    );
    Ok(())
}
impl MealPlugin {
    async fn run(&self, host: &Host, message: &Incoming) -> Result<Reply> {
        let inv = Invocation::from_message(host, message, "meal")?;
        let timestamp = now();
        let operation = meal_command(&message.text);
        let old = if operation.is_none() {
            let scope = inv.scope.clone();
            let actor = inv.actor;
            host.store
                .call(move |db| read_pending(db, &scope, actor, timestamp))
                .await?
        } else {
            None
        };
        let expected = old.as_ref().map(|p| p.generation.clone());
        if message.text.trim() == "取消菜单" {
            return transact(&host.store, inv, |tx, inv| {
                tx.execute(
                    "DELETE FROM meal_pending WHERE scope=?1 AND user_id=?2",
                    params![inv.scope, inv.actor],
                )?;
                Ok(Reply::text("已取消菜单操作。"))
            })
            .await;
        }
        let mut pending = if let Some(operation) = operation {
            let (_, args) = command(&message.text);
            let mut args = args.split_whitespace();
            let name = if matches!(operation, "add" | "delete") {
                args.next().map(str::to_owned)
            } else {
                None
            };
            let kind = if matches!(operation, "add" | "delete" | "view") {
                args.next().map(meal_kind).transpose()?
            } else {
                None
            };
            ensure!(
                args.next().is_none(),
                "名称请不要包含空格；用法：添加菜单 名称 菜品/饮品，并附带图片"
            );
            PendingMeal {
                operation: operation.into(),
                name,
                kind,
                generation: uuid::Uuid::new_v4().to_string(),
            }
        } else {
            let mut pending = old.context("没有进行中的菜单操作")?;
            if pending.name.is_none() && matches!(pending.operation.as_str(), "add" | "delete") {
                pending.name = Some(message.text.trim().to_owned());
            } else if pending.kind.is_none() {
                pending.kind = Some(meal_kind(&message.text)?);
            }
            pending
        };
        if let Some(name) = &mut pending.name {
            *name = name.trim().into();
            validate_meal_name(name)?;
        }
        if matches!(pending.operation.as_str(), "add" | "delete") {
            ensure!(
                host.config
                    .plugins
                    .native
                    .superusers
                    .contains(&message.sender_id),
                "只有配置的机器人管理员可以增删菜单"
            );
        }
        let mut image = None;
        if pending.operation == "add" && pending.name.is_some() && pending.kind.is_some() {
            let image_segment = message.raw_message["segments"]
                .as_array()
                .and_then(|segments| segments.iter().find(|s| s["type"] == "image"));
            if let Some(segment) = image_segment {
                let resource_id = segment["data"]["resource_id"]
                    .as_str()
                    .context("图片缺少资源ID")?;
                let url = host
                    .milky
                    .get_resource_temp_url(&message.key, resource_id)
                    .await?;
                image = Some(host.media.persist_url(host, message, "meal", &url).await?);
            }
        }
        let timezone: Tz = host.config.app.timezone.parse()?;
        let local = timezone
            .timestamp_opt(timestamp, 0)
            .single()
            .context("日期无效")?
            .naive_local()
            - chrono::Duration::hours(8);
        let request = MealRequest {
            pending,
            expected,
            bot_id: message.key.scope.bot_id,
            timestamp,
            usage_day: local.date().to_string(),
            image,
            cooldown: host.config.plugins.native.meal_cooldown_secs as i64,
            daily_limit: host.config.plugins.native.meal_daily_limit as i64,
        };
        transact(&host.store, inv, move |tx, inv| {
            meal_apply(tx, inv, request)
        })
        .await
    }
}
#[async_trait]
impl Plugin for MealPlugin {
    fn spec(&self) -> PluginSpec {
        spec("meal", MEAL_COMMANDS)
    }
    fn claims(&self, message: &Incoming) -> bool {
        meal_command(&message.text).is_some()
    }
    async fn handles_message(&self, host: &Host, message: &Incoming) -> Result<bool> {
        if self.claims(message) {
            return Ok(true);
        }
        if message.text.trim_start().starts_with('/') {
            return Ok(false);
        }
        let scope = message.key.scope.key();
        let actor = message.sender_id;
        host.store
            .call(move |db| Ok(read_pending(db, &scope, actor, now())?.is_some()))
            .await
    }
    async fn command(&self, host: &Host, message: &Incoming) -> Result<String> {
        Ok(self.run(host, message).await?.text)
    }
    async fn reply(&self, host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
        self.run(host, message).await?.outgoing(host, message).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MessageKey, Scope};
    use std::path::Path;

    async fn setup(path: &Path) -> Store {
        let store = Store::open(path, 64).await.unwrap();
        store
            .activate_plugins(
                ["checkin", "wordle", "meal", "kfc"]
                    .map(str::to_owned)
                    .to_vec(),
            )
            .await
            .unwrap();
        migrate(&store).await.unwrap();
        store
    }
    async fn invocation(
        store: &Store,
        plugin: &'static str,
        scope: &str,
        actor: i64,
        sequence: i64,
    ) -> Invocation {
        let inv = Invocation {
            scope: scope.into(),
            actor,
            source_key: format!("{scope}:{sequence}"),
            plugin,
            plugin_epoch: store.epoch(plugin).await.unwrap(),
        };
        let saved = inv.clone();
        store.call(move|db|{
            let access=crate::access::require_enabled(db,&saved.scope)?;
            let revision=crate::catalog::allowed_revision(db,&saved.scope,saved.plugin)?.unwrap();
            db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,command_plugin,plugin_revision,access_revision) VALUES(?1,?2,?2,1,?3,'control','test command',1,?4,?5,?6)",params![saved.source_key,saved.scope,saved.actor,saved.plugin,revision,access])?;Ok(())
        }).await.unwrap();
        inv
    }
    fn date(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap()
    }
    async fn deer(
        store: &Store,
        inv: Invocation,
        action: DeerAction,
        subject: i64,
        day: u32,
        admin: bool,
        timestamp: i64,
    ) -> Result<Reply> {
        transact(store, inv, move |tx, inv| {
            deer_apply(
                tx,
                inv,
                action,
                subject,
                &subject.to_string(),
                admin,
                true,
                date(day),
                timestamp,
            )
        })
        .await
    }
    fn calendar_count(reply: &Reply, day: u32) -> i64 {
        let Some(NativeView::Calendar { days, .. }) = &reply.view else {
            panic!("calendar expected")
        };
        days.iter()
            .find(|entry| entry.0 == day)
            .map(|entry| entry.1)
            .unwrap_or(0)
    }
    async fn wordle(
        store: &Store,
        inv: Invocation,
        action: WordleAction,
        timestamp: i64,
    ) -> Result<Reply> {
        transact(store, inv, move |tx, inv| {
            wordle_apply(
                tx,
                inv,
                action,
                timestamp,
                "{}",
                Some(("apple".into(), "苹果".into())),
            )
        })
        .await
    }
    fn open() -> WordleAction {
        WordleAction::Open {
            length: 5,
            dictionary: "CET4".into(),
        }
    }
    fn meal_request(operation: &str, timestamp: i64) -> MealRequest {
        MealRequest {
            pending: PendingMeal {
                operation: operation.into(),
                name: None,
                kind: None,
                generation: "test".into(),
            },
            expected: None,
            bot_id: 1,
            timestamp,
            usage_day: "2026-10-20".into(),
            image: None,
            cooldown: 10,
            daily_limit: 0,
        }
    }
    async fn meal(store: &Store, inv: Invocation, request: MealRequest) -> Result<Reply> {
        transact(store, inv, move |tx, inv| meal_apply(tx, inv, request)).await
    }

    #[test]
    fn real_dictionaries_and_legal_guesses_cover_all_lengths() {
        let mut total = 0;
        for (name, text) in DICTIONARIES {
            let words: BTreeMap<String, String> = serde_json::from_str(text).unwrap();
            total += words.len();
            assert!(words.len() > 1000, "{name}");
            for length in 3..=8 {
                let (word, meaning) = random_word(name, length).unwrap();
                assert_eq!(word.len(), length);
                assert!(!meaning.is_empty());
            }
        }
        assert!(total > 30_000);
        assert!(legal_word("hello") && legal_word("aardvark") && !legal_word("zzzzz"));
        let words: BTreeMap<String, String> = serde_json::from_str(DICTIONARIES[0].1).unwrap();
        assert!(!words.contains_key("aardvark")); // valid guesses are not restricted to CET4.
        assert!(words.contains_key("christ") && words.contains_key("bible")); // Preserve playable capitalized upstream answers.
        assert_eq!(marks("apple", "allee"), vec![2, 1, 0, 0, 2]);
        assert_eq!(marks("apple", "paper"), vec![1, 1, 2, 1, 0]);
        assert_eq!(marks("level", "level"), vec![2; 5]);
        assert!(matches!(
            wordle_action("/猜单词 -d 专八 -l 8").unwrap(),
            WordleAction::Open { length: 8, .. }
        ));
        assert!(wordle_action("/wordle -l 9").is_err());
    }

    #[test]
    fn upstream_kfc_corpus_and_calendar_rules_are_not_simplified() {
        let data: serde_json::Value =
            serde_json::from_str(include_str!("../../assets/native/kfc/post.json")).unwrap();
        assert!(data["post"].as_array().unwrap().len() > 100);
        assert_eq!(kfc_day("疯狂星期天").unwrap().unwrap(), 6);
        assert_eq!(kfc_day("狂乱火曜日").unwrap().unwrap(), 1);
        assert!(kfc_day("疯狂星期八").unwrap().is_err());
        assert!(kfc_day("请来一段疯狂星期四").is_none());
        assert_eq!(
            kfc_post(0, "星期四 周四 礼拜四 Thursday thursday 木曜日"),
            "星期一 周一 礼拜一 Monday Monday 月曜日"
        );
        assert_eq!(duration_secs("1h30m").unwrap(), 5400);
        assert_eq!(duration_secs("01:30:00").unwrap(), 5400);
        assert_eq!(duration_secs("2天").unwrap(), 172800);
        assert!(duration_secs("31d").is_err());
        assert!(duration_secs("-1h").is_err());
        let instant = chrono::DateTime::parse_from_rfc3339("2026-10-20T16:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(
            local_date(instant, chrono_tz::Asia::Shanghai)
                .unwrap()
                .day(),
            21
        );
    }

    #[tokio::test]
    async fn deer_repeated_counts_idempotency_concurrency_and_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("native.db");
        let store = setup(&path).await;
        let first = invocation(&store, "checkin", "1:group:2", 7, 1).await;
        let second = invocation(&store, "checkin", "1:group:2", 7, 2).await;
        let (a, b) = tokio::join!(
            deer(&store, first.clone(), DeerAction::Record, 7, 20, false, 100),
            deer(&store, second, DeerAction::Record, 7, 20, false, 100)
        );
        let mut counts = [
            calendar_count(&a.unwrap(), 20),
            calendar_count(&b.unwrap(), 20),
        ];
        counts.sort();
        assert_eq!(counts, [1, 2]);
        let replay = deer(&store, first, DeerAction::Record, 7, 20, false, 100)
            .await
            .unwrap();
        assert_eq!(calendar_count(&replay, 20), 1);
        let third = invocation(&store, "checkin", "1:group:2", 7, 3).await;
        assert_eq!(
            calendar_count(
                &deer(&store, third, DeerAction::Calendar, 7, 20, false, 100)
                    .await
                    .unwrap(),
                20
            ),
            2
        );
        let other = invocation(&store, "checkin", "2:group:2", 7, 4).await;
        assert_eq!(
            calendar_count(
                &deer(&store, other, DeerAction::Record, 7, 20, false, 100)
                    .await
                    .unwrap(),
                20
            ),
            1
        );
        store.stop().await.unwrap();
        let store = setup(&path).await;
        let next = invocation(&store, "checkin", "1:group:2", 7, 5).await;
        assert_eq!(
            calendar_count(
                &deer(&store, next, DeerAction::Record, 7, 20, false, 101)
                    .await
                    .unwrap(),
                20
            ),
            3
        );
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn deer_help_permissions_backfill_ban_and_rank() {
        let temp = tempfile::tempdir().unwrap();
        let store = setup(&temp.path().join("native.db")).await;
        let inv = invocation(&store, "checkin", "1:group:2", 8, 1).await;
        deer(
            &store,
            inv,
            DeerAction::HelpPolicy(false),
            8,
            20,
            false,
            100,
        )
        .await
        .unwrap();
        let inv = invocation(&store, "checkin", "1:group:2", 7, 2).await;
        assert!(
            deer(&store, inv, DeerAction::Record, 8, 20, false, 100)
                .await
                .is_err()
        );
        let inv = invocation(&store, "checkin", "1:group:2", 7, 3).await;
        assert!(
            deer(&store, inv, DeerAction::HelpPolicy(true), 8, 20, false, 100)
                .await
                .is_err()
        );
        let inv = invocation(&store, "checkin", "1:group:2", 7, 4).await;
        deer(&store, inv, DeerAction::HelpPolicy(true), 8, 20, true, 100)
            .await
            .unwrap();
        let inv = invocation(&store, "checkin", "1:group:2", 7, 5).await;
        deer(&store, inv, DeerAction::Ban(Some(10)), 8, 20, true, 100)
            .await
            .unwrap();
        let inv = invocation(&store, "checkin", "1:group:2", 8, 6).await;
        assert!(
            deer(&store, inv, DeerAction::Record, 8, 20, false, 109)
                .await
                .is_err()
        );
        let inv = invocation(&store, "checkin", "1:group:2", 8, 7).await;
        assert_eq!(
            calendar_count(
                &deer(&store, inv, DeerAction::Record, 8, 20, false, 110)
                    .await
                    .unwrap(),
                20
            ),
            1
        );
        for sequence in 8..10 {
            let inv = invocation(&store, "checkin", "1:group:2", 8, sequence).await;
            assert_eq!(
                calendar_count(
                    &deer(&store, inv, DeerAction::Backfill(19), 8, 20, false, 111)
                        .await
                        .unwrap(),
                    19
                ),
                1
            );
        }
        let inv = invocation(&store, "checkin", "1:group:2", 8, 10).await;
        assert!(
            deer(&store, inv, DeerAction::Backfill(20), 8, 20, false, 111)
                .await
                .is_err()
        );
        let inv = invocation(&store, "checkin", "1:group:2", 7, 11).await;
        let rank = deer(&store, inv, DeerAction::Rank, 7, 20, false, 112)
            .await
            .unwrap();
        let Some(NativeView::Rank { entries, .. }) = rank.view else {
            panic!()
        };
        assert_eq!(entries, vec![(8, "8".into(), 2)]);
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn wordle_is_group_shared_has_hints_renewal_and_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("native.db");
        let store = setup(&path).await;
        let inv = invocation(&store, "wordle", "1:group:2", 7, 1).await;
        wordle(&store, inv, open(), 100).await.unwrap();
        let a = invocation(&store, "wordle", "1:group:2", 7, 2).await;
        let b = invocation(&store, "wordle", "1:group:2", 8, 3).await;
        let (a, b) = tokio::join!(
            wordle(&store, a, WordleAction::Guess("paper".into()), 200),
            wordle(&store, b, WordleAction::Guess("lemon".into()), 201)
        );
        assert!(a.is_ok() && b.is_ok());
        let inv = invocation(&store, "wordle", "1:group:2", 8, 4).await;
        let hint = wordle(&store, inv, WordleAction::Hint, 499).await.unwrap();
        let Some(NativeView::Wordle {
            hint: Some(hint),
            guesses,
            ..
        }) = hint.view
        else {
            panic!()
        };
        assert_eq!(hint, "apple");
        assert_eq!(guesses.len(), 2);
        let inv = invocation(&store, "wordle", "1:group:2", 8, 5).await;
        assert!(
            wordle(&store, inv, WordleAction::Guess("paper".into()), 500)
                .await
                .unwrap()
                .text
                .contains("已经")
        );
        let inv = invocation(&store, "wordle", "1:group:2", 8, 6).await;
        assert!(
            wordle(&store, inv, WordleAction::Guess("zzzzz".into()), 501)
                .await
                .unwrap()
                .text
                .contains("不在合法")
        );
        let inv = invocation(&store, "wordle", "2:group:2", 8, 7).await;
        assert!(
            wordle(&store, inv, WordleAction::Hint, 502)
                .await
                .unwrap()
                .text
                .contains("没有")
        );
        store.stop().await.unwrap();
        let store = setup(&path).await;
        let inv = invocation(&store, "wordle", "1:group:2", 9, 8).await;
        let won = wordle(&store, inv, WordleAction::Guess("apple".into()), 800)
            .await
            .unwrap();
        assert!(won.text.contains("恭喜 9") && won.text.contains("苹果"));
        let Some(NativeView::Wordle { guesses, .. }) = won.view else {
            panic!()
        };
        assert_eq!(guesses.len(), 3);
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn wordle_length_plus_one_attempts_and_exact_expiry() {
        let temp = tempfile::tempdir().unwrap();
        let store = setup(&temp.path().join("native.db")).await;
        let inv = invocation(&store, "wordle", "1:group:2", 7, 1).await;
        wordle(&store, inv, open(), 100).await.unwrap();
        for (index, guess) in ["paper", "lemon", "green", "house", "world", "hello"]
            .into_iter()
            .enumerate()
        {
            let inv = invocation(&store, "wordle", "1:group:2", 7, index as i64 + 2).await;
            let result = wordle(
                &store,
                inv,
                WordleAction::Guess(guess.into()),
                101 + index as i64,
            )
            .await
            .unwrap();
            assert_eq!(result.text.contains("用完"), index == 5);
        }
        let inv = invocation(&store, "wordle", "1:group:2", 7, 8).await;
        wordle(&store, inv, open(), 1000).await.unwrap();
        let inv = invocation(&store, "wordle", "1:group:2", 7, 9).await;
        assert!(
            wordle(&store, inv, WordleAction::Guess("apple".into()), 1300)
                .await
                .unwrap()
                .text
                .contains("没有")
        );
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn meal_real_images_shared_cooldown_daily_limit_and_menu_management() {
        let temp = tempfile::tempdir().unwrap();
        let store = setup(&temp.path().join("native.db")).await;
        assert_eq!(default_menu().unwrap().len(), 151);
        let inv = invocation(&store, "meal", "1:group:2", 7, 1).await;
        let first = meal(&store, inv, meal_request("eat", 100)).await.unwrap();
        assert!(
            first
                .image
                .unwrap()
                .starts_with("https://raw.githubusercontent.com/")
        );
        let inv = invocation(&store, "meal", "1:group:3", 8, 2).await;
        assert!(
            meal(&store, inv, meal_request("drink", 109))
                .await
                .unwrap()
                .text
                .contains("冷却")
        );
        let inv = invocation(&store, "meal", "1:group:3", 7, 3).await;
        let mut request = meal_request("drink", 110);
        request.daily_limit = 1;
        assert!(
            meal(&store, inv, request)
                .await
                .unwrap()
                .text
                .contains("次数已用完")
        );
        let inv = invocation(&store, "meal", "1:group:2", 7, 4).await;
        let mut request = meal_request("add", 120);
        request.pending.name = Some("测试饮品".into());
        request.pending.kind = Some("drink".into());
        request.image = Some("local-media-id".into());
        assert!(
            meal(&store, inv, request)
                .await
                .unwrap()
                .text
                .contains("成功添加")
        );
        let inv = invocation(&store, "meal", "1:group:2", 7, 5).await;
        let mut request = meal_request("view", 121);
        request.pending.kind = Some("drink".into());
        let result = meal(&store, inv, request).await.unwrap();
        let Some(NativeView::Menu { items, .. }) = result.view else {
            panic!()
        };
        assert!(items.contains(&"测试饮品".into()));
        let inv = invocation(&store, "meal", "1:group:2", 7, 6).await;
        let mut request = meal_request("delete", 122);
        request.pending.name = Some("测试饮品".into());
        request.pending.kind = Some("drink".into());
        assert!(
            meal(&store, inv, request)
                .await
                .unwrap()
                .text
                .contains("已删除")
        );
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn pending_menu_steps_are_scoped_expiring_and_compare_and_swap() {
        let temp = tempfile::tempdir().unwrap();
        let store = setup(&temp.path().join("native.db")).await;
        let inv = invocation(&store, "meal", "1:group:2", 7, 1).await;
        assert!(
            meal(&store, inv, meal_request("add", 100))
                .await
                .unwrap()
                .text
                .contains("名称")
        );
        let generation = store
            .call(|db| {
                assert!(read_pending(db, "1:group:2", 8, 100)?.is_none());
                assert!(read_pending(db, "2:group:2", 7, 100)?.is_none());
                assert!(read_pending(db, "1:group:2", 7, 700)?.is_none());
                Ok(read_pending(db, "1:group:2", 7, 100)?.unwrap().generation)
            })
            .await
            .unwrap();
        let inv = invocation(&store, "meal", "1:group:2", 7, 2).await;
        let mut request = meal_request("add", 101);
        request.pending.name = Some("测试".into());
        request.expected = Some(generation.clone());
        assert!(
            meal(&store, inv, request)
                .await
                .unwrap()
                .text
                .contains("菜单类型")
        );
        let inv = invocation(&store, "meal", "1:group:2", 7, 3).await;
        let mut stale = meal_request("add", 102);
        stale.pending.name = Some("覆盖".into());
        stale.expected = Some(generation);
        assert!(meal(&store, inv, stale).await.is_err());
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn disabling_scope_or_plugin_rejects_queued_native_writes() {
        let temp = tempfile::tempdir().unwrap();
        let store = setup(&temp.path().join("native.db")).await;
        let inv = invocation(&store, "checkin", "1:group:2", 7, 1).await;
        let scope = Scope {
            bot_id: 1,
            scene: Scene::Group,
            peer_id: 2,
        };
        crate::catalog::set_enabled(&store, scope.clone(), "checkin".into(), false)
            .await
            .unwrap();
        crate::catalog::set_enabled(&store, scope.clone(), "checkin".into(), true)
            .await
            .unwrap();
        assert!(
            deer(&store, inv, DeerAction::Record, 7, 20, false, 100)
                .await
                .is_err()
        );
        let inv = invocation(&store, "checkin", "1:group:2", 7, 2).await;
        let access = crate::access::AccessStore::new(store.clone());
        access.set_enabled(scope.clone(), false).await.unwrap();
        access.set_enabled(scope, true).await.unwrap();
        assert!(
            deer(&store, inv, DeerAction::Record, 7, 20, false, 100)
                .await
                .is_err()
        );
        let count = store
            .call(|db| {
                Ok(db.query_row("SELECT count(*) FROM deer_days", [], |r| r.get::<_, i64>(0))?)
            })
            .await
            .unwrap();
        assert_eq!(count, 0);
        store.stop().await.unwrap();
    }

    fn meal_message(text: &str) -> Incoming {
        Incoming {
            key: MessageKey {
                scope: Scope {
                    bot_id: 1,
                    scene: Scene::Group,
                    peer_id: 2,
                },
                message_seq: 1,
            },
            sender_id: 7,
            sender_nickname: None,
            sender_card: None,
            text: text.into(),
            mentions: vec![],
            quote_seq: None,
            received_at: 1,
            raw_message: serde_json::Value::Null,
            notification: None,
        }
    }

    const COMMON_MEAL_REQUESTS: &[&str] = &[
        "早上吃什么",
        "早饭吃什么",
        "早餐吃什么",
        "中午吃什么",
        "午饭吃什么",
        "午餐吃什么",
        "晚上吃什么",
        "晚饭吃什么",
        "晚餐吃什么",
        "今晚吃什么",
        "今天吃什么",
    ];

    #[test]
    fn meal_common_requests_support_slash_and_bare_forms_and_registered_help() {
        for request in COMMON_MEAL_REQUESTS {
            let registered = format!("/{request}");
            assert!(MealPlugin.spec().commands.contains(&registered.as_str()));
            for text in [request.to_string(), registered, format!(" \t{request}\n")] {
                assert_eq!(meal_command(&text), Some("eat"), "{text}");
                assert!(MealPlugin.claims(&meal_message(&text)), "{text}");
            }
        }
        for prefix in ["午饭", "午餐", "今天午饭", "明天午餐", "后日午饭"] {
            for (verb, operation) in [("吃", "eat"), ("喝", "drink")] {
                for ending in ["什么", "啥", "点啥"] {
                    assert_eq!(
                        meal_command(&format!("{prefix}{verb}{ending}")),
                        Some(operation)
                    );
                }
            }
        }
    }

    #[test]
    fn meal_shortcuts_do_not_claim_prose_but_keep_explicit_command_arguments() {
        for text in [
            "你觉得今天吃什么比较好",
            "请告诉我今天早上吃什么",
            "今天吃什么 我们一起讨论一下",
            "今天喝什么 我还没想好",
            "早上吃完再想吃什么",
            "午饭吃过了晚上吃什么",
            "晚上喝完再想喝什么",
            "晚上吃什么比较好",
            "早餐吃什么都可以",
            "我想知道今晚吃什么",
        ] {
            assert_eq!(meal_command(text), None, "{text}");
            assert!(!MealPlugin.claims(&meal_message(text)), "{text}");
        }
        for (text, operation) in [
            ("/meal", "eat"),
            ("吃什么", "eat"),
            ("今天喝什么", "drink"),
            ("/今天吃什么 多余参数", "eat"),
            ("/午餐吃什么 多余参数", "eat"),
            ("全部菜单 菜品", "view"),
            ("/查看菜单 饮品", "view"),
            ("添加菜单 豆浆 饮品", "add"),
            ("/删除菜单 豆浆 饮品", "delete"),
        ] {
            assert_eq!(meal_command(text), Some(operation), "{text}");
        }
    }

    #[tokio::test]
    async fn meal_shortcuts_share_image_business_and_keep_disabled_command_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.plugins.ai.enabled = false;
        config.plugins.native.enabled = vec!["meal".into()];
        config.plugins.native.meal_cooldown_secs = 0;
        config.app.database = temp.path().join("meal.db").to_string_lossy().into();
        config.nonebot.media_dir = temp.path().join("media").to_string_lossy().into();
        let host = Host::create(config).await.unwrap();
        let mut disabled = host.plugins.clone();
        disabled
            .validate(&["host".to_owned()].into_iter().collect())
            .unwrap();
        let mut sequence = 0;
        for request in COMMON_MEAL_REQUESTS {
            for text in [request.to_string(), format!("/{request}")] {
                sequence += 1;
                let mut message = meal_message(&text);
                message.key.message_seq = sequence;
                let (owner, handler) = host.plugins.select(&host, &message).await.unwrap().unwrap();
                assert_eq!(owner, "meal");
                assert!(handler.is_some());
                let (owner, handler) = disabled.select(&host, &message).await.unwrap().unwrap();
                assert_eq!(owner, "meal");
                assert!(
                    handler.is_none(),
                    "disabled commands must not fall through to AI"
                );
                invocation(&host.store, "meal", "1:group:2", 7, sequence).await;
                let reply = MealPlugin.run(&host, &message).await.unwrap();
                let resource = reply
                    .image
                    .expect("every meal request still selects a real image");
                let is_food = host.store.call(move |db| {
                    Ok(db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM meal_menu WHERE bot_id=1 AND kind='eat' AND resource=?1 AND deleted=0)",
                        [resource],
                        |row| row.get::<_, bool>(0),
                    )?)
                }).await.unwrap();
                assert!(is_food);
            }
        }
        let ordinary = meal_message("午饭吃过了晚上吃什么");
        assert!(
            host.plugins
                .select(&host, &ordinary)
                .await
                .unwrap()
                .is_none()
        );
        let usage = host
            .store
            .call(|db| {
                Ok(db.query_row(
                    "SELECT SUM(count) FROM meal_usage WHERE bot_id=1 AND user_id=7",
                    [],
                    |row| row.get::<_, i64>(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(usage, sequence);
        host.cancel.cancel();
        host.store.stop().await.unwrap();
    }

    #[test]
    fn command_claims_only_intended_patterns() {
        let mut message = meal_message("今天喝什么");
        assert!(MealPlugin.claims(&message));
        message.text = "你觉得今天吃什么比较好".into();
        assert!(!MealPlugin.claims(&message));
        message.text = "🦌 @someone".into();
        assert!(CheckinPlugin.claims(&message));
        message.text = "apple".into();
        assert!(!WordlePlugin.claims(&message));
        message.text = "wordle".into();
        assert!(!WordlePlugin.claims(&message));
        message.mentions.push(1);
        assert!(WordlePlugin.claims(&message));
        message.mentions.clear();
        message.text = "/wordle -l 8".into();
        assert!(WordlePlugin.claims(&message));
        message.text = "wordle".into();
        message.key.scope.scene = Scene::Friend;
        assert!(WordlePlugin.claims(&message));
        for first in ["", "今", "明", "后", "|"] {
            for second in ["", "天", "日", "|"] {
                for period in ["", "早", "中", "晚"] {
                    for time in ["上", "午", "餐", "饭", "夜宵", "宵夜", "早", "晚"] {
                        assert_eq!(
                            meal_command(&format!("{first}{second}{period}{time}吃啥")),
                            Some("eat")
                        );
                        assert_eq!(
                            meal_command(&format!("{first}{second}{period}{time}喝点啥")),
                            Some("drink")
                        );
                    }
                }
            }
        }
        assert_eq!(meal_command("请告诉我今天早上吃什么"), None);
    }

    #[tokio::test]
    async fn cleanup_expires_only_receipts_and_operational_menu_state() {
        let temp = tempfile::tempdir().unwrap();
        let store = setup(&temp.path().join("native.db")).await;
        let inv = invocation(&store, "checkin", "1:group:2", 7, 1).await;
        deer(&store, inv, DeerAction::Record, 7, 20, false, 100)
            .await
            .unwrap();
        let pending = invocation(&store, "meal", "1:group:2", 7, 2).await;
        meal(&store, pending, meal_request("add", 100))
            .await
            .unwrap();
        store
            .call(|db| {
                db.execute("DELETE FROM messages WHERE key='1:group:2:1'", [])?;
                db.execute("INSERT INTO meal_usage VALUES(1,7,'2000-01-01',5)", [])?;
                Ok(())
            })
            .await
            .unwrap();
        maintain(&store, now() - 86400).await.unwrap();
        store
            .call(|db| {
                assert_eq!(
                    db.query_row("SELECT count(*) FROM deer_days", [], |r| r.get::<_, i64>(0))?,
                    1
                );
                assert_eq!(
                    db.query_row("SELECT count(*) FROM native_receipts", [], |r| r
                        .get::<_, i64>(0))?,
                    1
                );
                assert_eq!(
                    db.query_row("SELECT count(*) FROM meal_pending", [], |r| r
                        .get::<_, i64>(0))?,
                    0
                );
                assert_eq!(
                    db.query_row("SELECT count(*) FROM meal_usage", [], |r| r
                        .get::<_, i64>(0))?,
                    0
                );
                Ok(())
            })
            .await
            .unwrap();
        store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn wordle_timer_survives_offline_restart_and_never_replays_disabled_sources() {
        use axum::{Json, Router, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let sends = Arc::new(AtomicUsize::new(0));
        let observed = sends.clone();
        let app=Router::new()
            .route("/api/get_login_info",post(||async{Json(serde_json::json!({"status":"ok","retcode":0,"data":{"uin":123}}))}))
            .route("/api/send_group_message",post(move||{let observed=observed.clone();async move{
                let seq=observed.fetch_add(1,Ordering::SeqCst)as i64+10000;
                Json(serde_json::json!({"status":"ok","retcode":0,"data":{"message_seq":seq}}))
            }}));
        let stop = tokio_util::sync::CancellationToken::new();
        let shutdown = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap()
        });
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.app.database = temp.path().join("timer.db").to_string_lossy().into();
        config.milky.url = url;
        config.plugins.ai.enabled = false;
        config.plugins.native.enabled = vec!["wordle".into()];
        let host = Host::create(config.clone()).await.unwrap();
        async fn expired(host: &Host, bot: i64, peer: i64, seq: i64) {
            let message = Incoming {
                key: MessageKey {
                    scope: Scope {
                        bot_id: bot,
                        scene: Scene::Group,
                        peer_id: peer,
                    },
                    message_seq: seq,
                },
                sender_id: 7,
                sender_nickname: None,
                sender_card: None,
                text: "/wordle".into(),
                mentions: vec![],
                quote_seq: None,
                received_at: now() - 1000,
                raw_message: serde_json::Value::Null,
                notification: None,
            };
            let inv = invocation(&host.store, "wordle", &message.key.scope.key(), 7, seq).await;
            let source = serde_json::to_string(&message).unwrap();
            transact(&host.store, inv, move |tx, inv| {
                wordle_apply(
                    tx,
                    inv,
                    open(),
                    now() - 1000,
                    &source,
                    Some(("apple".into(), "苹果".into())),
                )
            })
            .await
            .unwrap();
        }
        expired(&host, 123, 456, 1).await;
        expired(&host, 999, 456, 1).await;
        WordlePlugin.tick(&host).await.unwrap(); // Unverified login cannot consume timers.
        assert_eq!(sends.load(Ordering::SeqCst), 0);
        host.store
            .call(|db| {
                assert_eq!(
                    db.query_row(
                        "SELECT count(*) FROM wordle_games WHERE status='active'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    2
                );
                Ok(())
            })
            .await
            .unwrap();
        host.cancel.cancel();
        host.store.stop().await.unwrap();
        drop(host);
        let host = Host::create(config).await.unwrap();
        host.milky.login().await.unwrap();
        WordlePlugin.tick(&host).await.unwrap();
        WordlePlugin.tick(&host).await.unwrap();
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        host.store
            .call(|db| {
                assert_eq!(
                    db.query_row(
                        "SELECT status FROM wordle_games WHERE scope='999:group:456'",
                        [],
                        |r| r.get::<_, String>(0)
                    )?,
                    "active"
                );
                Ok(())
            })
            .await
            .unwrap();
        expired(&host, 123, 789, 2).await;
        let scope = Scope {
            bot_id: 123,
            scene: Scene::Group,
            peer_id: 789,
        };
        crate::catalog::set_enabled(&host.store, scope.clone(), "wordle".into(), false)
            .await
            .unwrap();
        crate::catalog::set_enabled(&host.store, scope, "wordle".into(), true)
            .await
            .unwrap();
        WordlePlugin.tick(&host).await.unwrap();
        WordlePlugin.tick(&host).await.unwrap();
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        host.cancel.cancel();
        host.store.stop().await.unwrap();
        stop.cancel();
        task.await.unwrap();
    }
}
