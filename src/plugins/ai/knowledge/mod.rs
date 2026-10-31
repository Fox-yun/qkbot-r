//! Unified store for knowledge types that came after personal facts and group
//! learning notes: shared episodes, the bot's own statements, derived person
//! impressions and group interaction notes. Personal facts and learning notes
//! keep their authoritative tables; everything new shares one model here for
//! provenance, audience, versions, statistics and invalidation.
//!
//! Stored items are material for the model, never rules. They cannot change a
//! persona, permissions, tool access or the standard of help.
pub mod behavior;
pub mod extract;
pub mod outcomes;
pub mod reflection;
pub mod stats;

use super::{
    context::{continuity, policy},
    memory,
};
use crate::{
    config::{CrossScenePolicy, KnowledgeConfig},
    store::Store,
    types::{Scene, Scope, now},
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

const SCHEMA: &str = "
CREATE TABLE ai_knowledge_settings(id INTEGER PRIMARY KEY CHECK(id=1),settings TEXT NOT NULL,character_id TEXT NOT NULL);
CREATE TABLE ai_knowledge(
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 bot_id INTEGER NOT NULL,
 kind TEXT NOT NULL CHECK(kind IN('episode','self','impression','norm','style','joke','avoid')),
 subject TEXT NOT NULL CHECK(subject IN('user','scope','bot')),
 subject_id INTEGER NOT NULL,
 scope TEXT NOT NULL,
 character_id TEXT NOT NULL,
 domain TEXT NOT NULL,
 text TEXT NOT NULL,
 origin TEXT NOT NULL CHECK(origin IN('auto','derived','manual')),
 confidence REAL NOT NULL,
 importance INTEGER NOT NULL CHECK(importance BETWEEN 1 AND 5),
 status TEXT NOT NULL CHECK(status IN('active','pending','rejected')),
 locked INTEGER NOT NULL DEFAULT 0,
 version INTEGER NOT NULL DEFAULT 1,
 created_at INTEGER NOT NULL,
 updated_at INTEGER NOT NULL,
 expires_at INTEGER);
CREATE INDEX ai_knowledge_lookup ON ai_knowledge(bot_id,kind,subject,subject_id,status);
CREATE INDEX ai_knowledge_scope ON ai_knowledge(bot_id,scope,kind,status);
CREATE TABLE ai_knowledge_sources(
 knowledge_id INTEGER NOT NULL,
 source_key TEXT NOT NULL,
 role TEXT NOT NULL CHECK(role IN('evidence','context','reply','memory')),
 quote TEXT NOT NULL DEFAULT '',
 source_hash TEXT NOT NULL DEFAULT '',
 memory_version INTEGER,
 PRIMARY KEY(knowledge_id,source_key,role));
CREATE INDEX ai_knowledge_source ON ai_knowledge_sources(source_key);
CREATE TABLE ai_knowledge_stats(
 item TEXT PRIMARY KEY,
 importance INTEGER,
 recall_count INTEGER NOT NULL DEFAULT 0,
 last_recalled_at INTEGER,
 reinforced_count INTEGER NOT NULL DEFAULT 0,
 reinforced_at INTEGER);
CREATE TABLE ai_knowledge_jobs(job TEXT NOT NULL,key TEXT NOT NULL,cursor INTEGER NOT NULL DEFAULT 0,
 last_attempt INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(job,key));
CREATE TABLE ai_knowledge_operations(admin_id TEXT NOT NULL,session_id TEXT NOT NULL,request_id TEXT NOT NULL,
 request_hash TEXT NOT NULL,result TEXT NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(admin_id,session_id,request_id));
CREATE TRIGGER ai_knowledge_deleted AFTER DELETE ON ai_knowledge BEGIN
 DELETE FROM ai_knowledge_sources WHERE knowledge_id=OLD.id;
 DELETE FROM ai_knowledge_stats WHERE item='knowledge:'||OLD.id;
END;
CREATE TRIGGER ai_knowledge_memory_deleted AFTER DELETE ON ai_memories BEGIN
 DELETE FROM ai_knowledge_stats WHERE item='memory:'||OLD.id;
END;
";

/// Recall limits per kind and per turn.
const IMPRESSION_LIMIT: usize = 3;
const EPISODE_LIMIT: usize = 4;
const SELF_LIMIT: usize = 3;
const BEHAVIOR_LIMIT: usize = 6;
const CANDIDATES: i64 = 48;

pub async fn init(store: &Store) -> Result<()> {
    store.migrate("ai_knowledge", 1, SCHEMA).await?;
    outcomes::init(store).await
}

pub(crate) fn installed(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='ai_knowledge')",
        [],
        |r| r.get(0),
    )?)
}

/// Persist the switches that database-side code (batch preparation, ranking,
/// recall) needs without access to the runtime configuration.
pub async fn configure(store: &Store, config: &KnowledgeConfig, character_id: &str) -> Result<()> {
    config.validate()?;
    let settings = serde_json::to_string(config)?;
    let character = character_id.to_owned();
    store
        .call(move |db| {
            db.execute("INSERT INTO ai_knowledge_settings(id,settings,character_id) VALUES(1,?1,?2) ON CONFLICT(id) DO UPDATE SET settings=excluded.settings,character_id=excluded.character_id",params![settings,character])?;
            Ok(())
        })
        .await
}

/// The stored configuration, or None before configuration or migration.
pub(crate) fn settings(db: &Connection) -> Result<Option<(KnowledgeConfig, String)>> {
    if !installed(db)? {
        return Ok(None);
    }
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT settings,character_id FROM ai_knowledge_settings WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(s, c)| Ok((serde_json::from_str(&s)?, c)))
        .transpose()
}

pub(crate) fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Episode,
    #[serde(rename = "self")]
    Statement,
    Impression,
    Norm,
    Style,
    Joke,
    Avoid,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Episode => "episode",
            Self::Statement => "self",
            Self::Impression => "impression",
            Self::Norm => "norm",
            Self::Style => "style",
            Self::Joke => "joke",
            Self::Avoid => "avoid",
        }
    }
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "episode" => Self::Episode,
            "self" => Self::Statement,
            "impression" => Self::Impression,
            "norm" => Self::Norm,
            "style" => Self::Style,
            "joke" => Self::Joke,
            "avoid" => Self::Avoid,
            _ => bail!("invalid knowledge kind"),
        })
    }
    pub fn behavior(self) -> bool {
        matches!(self, Self::Norm | Self::Style | Self::Joke | Self::Avoid)
    }
    pub fn max_chars(self) -> usize {
        match self {
            Self::Episode => 160,
            _ => 120,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Episode => "共同经历",
            Self::Statement => "你说过的话",
            Self::Impression => "人物印象",
            Self::Norm => "本群习惯",
            Self::Style => "本群有效的说法",
            Self::Joke => "本群的梗",
            Self::Avoid => "本群应避免",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Subject {
    User,
    Scope,
    Bot,
}

impl Subject {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Scope => "scope",
            Self::Bot => "bot",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "user" => Self::User,
            "scope" => Self::Scope,
            "bot" => Self::Bot,
            _ => bail!("invalid knowledge subject"),
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Item {
    pub id: i64,
    pub bot_id: i64,
    pub kind: Kind,
    pub subject: Subject,
    pub subject_id: i64,
    pub scope: String,
    pub character_id: String,
    pub domain: String,
    pub text: String,
    pub origin: String,
    pub confidence: f64,
    pub importance: i64,
    pub status: String,
    pub locked: bool,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub expires_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Source {
    pub source_key: String,
    pub role: String,
    pub quote: String,
    pub source_hash: String,
    pub memory_version: Option<i64>,
}

impl Source {
    pub fn evidence(key: &str, quote: &str, text: &str) -> Self {
        Self {
            source_key: key.into(),
            role: "evidence".into(),
            quote: quote.into(),
            source_hash: hash(text),
            memory_version: None,
        }
    }
    pub fn context(key: &str, text: &str) -> Self {
        Self {
            source_key: key.into(),
            role: "context".into(),
            quote: String::new(),
            source_hash: hash(text),
            memory_version: None,
        }
    }
    /// A confirmed bot reply. It proves what the bot said, never a person's fact.
    pub fn reply(key: &str, quote: &str, text: &str) -> Self {
        Self {
            source_key: key.into(),
            role: "reply".into(),
            quote: quote.into(),
            source_hash: hash(text),
            memory_version: None,
        }
    }
    pub fn memory(id: i64, version: i64) -> Self {
        Self {
            source_key: format!("memory:{id}"),
            role: "memory".into(),
            quote: String::new(),
            source_hash: String::new(),
            memory_version: Some(version),
        }
    }
}

const ITEM_COLUMNS: &str = "id,bot_id,kind,subject,subject_id,scope,character_id,domain,text,origin,confidence,importance,status,locked,version,created_at,updated_at,expires_at";

fn item_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(Item, String, String)> {
    Ok((
        Item {
            id: r.get(0)?,
            bot_id: r.get(1)?,
            kind: Kind::Episode,
            subject: Subject::User,
            subject_id: r.get(4)?,
            scope: r.get(5)?,
            character_id: r.get(6)?,
            domain: r.get(7)?,
            text: r.get(8)?,
            origin: r.get(9)?,
            confidence: r.get(10)?,
            importance: r.get(11)?,
            status: r.get(12)?,
            locked: r.get(13)?,
            version: r.get(14)?,
            created_at: r.get(15)?,
            updated_at: r.get(16)?,
            expires_at: r.get(17)?,
        },
        r.get(2)?,
        r.get(3)?,
    ))
}

fn decode(raw: (Item, String, String)) -> Result<Item> {
    let (mut item, kind, subject) = raw;
    item.kind = Kind::parse(&kind)?;
    item.subject = Subject::parse(&subject)?;
    Ok(item)
}

pub(crate) fn get(db: &Connection, id: i64) -> Result<Option<Item>> {
    db.query_row(
        &format!("SELECT {ITEM_COLUMNS} FROM ai_knowledge WHERE id=?1"),
        [id],
        item_row,
    )
    .optional()?
    .map(decode)
    .transpose()
}

pub(crate) fn sources(db: &Connection, id: i64) -> Result<Vec<Source>> {
    Ok(db
        .prepare("SELECT source_key,role,quote,source_hash,memory_version FROM ai_knowledge_sources WHERE knowledge_id=?1 ORDER BY role,source_key")?
        .query_map([id], |r| {
            Ok(Source {
                source_key: r.get(0)?,
                role: r.get(1)?,
                quote: r.get(2)?,
                source_hash: r.get(3)?,
                memory_version: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// A new item, before it has an identity.
pub(crate) struct NewItem<'a> {
    pub bot_id: i64,
    pub kind: Kind,
    pub subject: Subject,
    pub subject_id: i64,
    pub scope: &'a str,
    pub character_id: &'a str,
    pub domain: &'a str,
    pub text: &'a str,
    pub origin: &'a str,
    pub confidence: f64,
    pub importance: i64,
    pub status: &'a str,
    pub expires_at: Option<i64>,
}

pub(crate) fn validate_text(kind: Kind, text: &str) -> Result<()> {
    let text = text.trim();
    ensure!(
        !text.is_empty()
            && text.chars().count() <= kind.max_chars()
            && !text.chars().any(char::is_control),
        "knowledge text is empty, too long or contains control characters"
    );
    Ok(())
}

/// Structural rules shared by every writer. Type-specific evidence rules are
/// checked by the writer before calling this.
pub(crate) fn insert(db: &Connection, item: &NewItem<'_>, sources: &[Source]) -> Result<i64> {
    validate_text(item.kind, item.text)?;
    ensure!(
        item.confidence.is_finite() && (0.0..=1.0).contains(&item.confidence),
        "knowledge confidence must be 0..1"
    );
    ensure!(
        (1..=5).contains(&item.importance),
        "knowledge importance must be 1..5"
    );
    ensure!(
        item.bot_id > 0 && !sources.is_empty() && sources.len() <= 64,
        "knowledge needs 1..64 sources"
    );
    match item.subject {
        Subject::User => ensure!(
            item.subject_id > 0 && item.subject_id != item.bot_id,
            "invalid knowledge subject"
        ),
        Subject::Bot => ensure!(item.subject_id == item.bot_id, "invalid bot subject"),
        Subject::Scope => ensure!(item.subject_id == 0, "invalid scope subject"),
    }
    if !item.scope.is_empty() {
        let scope = continuity::parse_scope(item.scope)?;
        ensure!(
            scope.bot_id == item.bot_id,
            "knowledge scope belongs to another account"
        );
    }
    ensure!(
        !matches!(item.subject, Subject::Scope | Subject::Bot) || !item.scope.is_empty(),
        "scope knowledge needs a source scope"
    );
    let time = now();
    db.execute(
        "INSERT INTO ai_knowledge(bot_id,kind,subject,subject_id,scope,character_id,domain,text,origin,confidence,importance,status,created_at,updated_at,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?13,?14)",
        params![item.bot_id,item.kind.as_str(),item.subject.as_str(),item.subject_id,item.scope,item.character_id,item.domain,item.text.trim(),item.origin,item.confidence,item.importance,item.status,time,item.expires_at],
    )?;
    let id = db.last_insert_rowid();
    for source in sources {
        db.execute("INSERT OR IGNORE INTO ai_knowledge_sources(knowledge_id,source_key,role,quote,source_hash,memory_version) VALUES(?1,?2,?3,?4,?5,?6)",
            params![id,source.source_key,source.role,source.quote,source.source_hash,source.memory_version])?;
    }
    Ok(id)
}

/// Replace text and sources of an unlocked automatic item, as a new version.
pub(crate) fn replace(
    db: &Connection,
    id: i64,
    version: i64,
    text: &str,
    confidence: f64,
    expires_at: Option<i64>,
    sources: &[Source],
) -> Result<()> {
    let item = get(db, id)?.context("knowledge item missing")?;
    ensure!(
        item.version == version && !item.locked && item.origin != "manual",
        "knowledge item changed or locked"
    );
    validate_text(item.kind, text)?;
    ensure!(
        confidence.is_finite() && (0.0..=1.0).contains(&confidence) && !sources.is_empty(),
        "invalid knowledge replacement"
    );
    db.execute("UPDATE ai_knowledge SET text=?2,confidence=?3,expires_at=?4,version=version+1,updated_at=?5 WHERE id=?1",
        params![id,text.trim(),confidence,expires_at,now()])?;
    db.execute(
        "DELETE FROM ai_knowledge_sources WHERE knowledge_id=?1",
        [id],
    )?;
    for source in sources {
        db.execute("INSERT OR IGNORE INTO ai_knowledge_sources(knowledge_id,source_key,role,quote,source_hash,memory_version) VALUES(?1,?2,?3,?4,?5,?6)",
            params![id,source.source_key,source.role,source.quote,source.source_hash,source.memory_version])?;
    }
    Ok(())
}

/// Invoke inside the caller's recall/forget transaction. Items built on any of
/// these sources are deleted with their text; versions cannot bring them back.
pub fn invalidate_sources(db: &Connection, keys: &[String]) -> Result<()> {
    if keys.is_empty() || !installed(db)? {
        return Ok(());
    }
    let encoded = serde_json::to_string(keys)?;
    db.execute("DELETE FROM ai_knowledge WHERE id IN(SELECT knowledge_id FROM ai_knowledge_sources WHERE source_key IN(SELECT value FROM json_each(?1)))", [&encoded])?;
    outcomes::invalidate_sources(db, &encoded)
}

/// Drop expired items and keep each account's store bounded.
pub(crate) fn maintain(db: &Connection) -> Result<()> {
    if !installed(db)? {
        return Ok(());
    }
    db.execute(
        "DELETE FROM ai_knowledge WHERE expires_at IS NOT NULL AND expires_at<=?1",
        [now()],
    )?;
    db.execute(
        "DELETE FROM ai_knowledge WHERE status='rejected' AND updated_at<?1",
        [now() - 30 * 86400],
    )?;
    // Derived impressions depend on facts that may have been forgotten or
    // deleted while their evidence keys survived; remove orphans.
    db.execute("DELETE FROM ai_knowledge WHERE kind='impression' AND id IN(SELECT s.knowledge_id FROM ai_knowledge_sources s WHERE s.role='memory' AND NOT EXISTS(SELECT 1 FROM ai_memories m WHERE 'memory:'||m.id=s.source_key))", [])?;
    db.execute("DELETE FROM ai_knowledge_stats WHERE item LIKE 'memory:%' AND NOT EXISTS(SELECT 1 FROM ai_memories m WHERE 'memory:'||m.id=item)", [])?;
    outcomes::maintain(db)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dependency {
    pub id: i64,
    pub version: i64,
}

impl Dependency {
    pub fn validate(&self, db: &Connection) -> Result<()> {
        let item = get(db, self.id)?.context("上下文中的知识已删除")?;
        ensure!(item.version == self.version, "上下文中的知识已变化");
        usable(db, &item)
    }
    pub fn allows_audience(
        &self,
        db: &Connection,
        policy: CrossScenePolicy,
        target: &Scope,
        actor: i64,
    ) -> Result<bool> {
        let Some(item) = get(db, self.id)? else {
            return Ok(false);
        };
        audience(db, &item, policy, target, actor)
    }
    /// Human message sources; bot replies stay behind this dependency.
    pub fn source_keys(&self, db: &Connection) -> Result<Vec<String>> {
        Ok(sources(db, self.id)?
            .into_iter()
            .filter(|s| matches!(s.role.as_str(), "evidence" | "context"))
            .map(|s| s.source_key)
            .collect())
    }
}

/// Current-state checks that do not depend on the reader.
fn usable(db: &Connection, item: &Item) -> Result<()> {
    ensure!(
        item.status == "active" && item.expires_at.is_none_or(|t| t > now()),
        "知识已停用、待审核或过期"
    );
    for source in sources(db, item.id)? {
        if source.role == "memory" {
            let id: i64 = source
                .source_key
                .strip_prefix("memory:")
                .context("invalid memory source")?
                .parse()?;
            let valid: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM ai_memories WHERE id=?1 AND version=?2 AND active=1 AND (expires_at IS NULL OR expires_at>?3))",
                params![id, source.memory_version, now()],
                |r| r.get(0),
            )?;
            ensure!(valid, "知识依据的个人记忆已变化");
            continue;
        }
        let revoked: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM ai_revoked_sources WHERE source_key=?1)",
            [&source.source_key],
            |r| r.get(0),
        )?;
        ensure!(!revoked, "知识来源已撤回");
        // A reply whose own upstream material was revoked, or which a policy
        // change retired, cannot carry its content forward here either.
        if invalidation_installed(db)? {
            let invalidated: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM ai_context_invalidated WHERE source_key=?1)",
                [&source.source_key],
                |r| r.get(0),
            )?;
            ensure!(!invalidated, "知识依据的回复已失效");
        }
        // Normal retention may remove the raw row. When it exists, it must be
        // the same unrevoked text that was used.
        if let Some((text, gone)) = db
            .query_row(
                "SELECT content,revoked FROM messages WHERE key=?1",
                [&source.source_key],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
            )
            .optional()?
        {
            ensure!(
                !gone && (source.source_hash.is_empty() || hash(&text) == source.source_hash),
                "知识来源已变化"
            );
        }
    }
    Ok(())
}

fn invalidation_installed(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='ai_context_invalidated')",
        [],
        |r| r.get(0),
    )?)
}

/// Whether a reply that used this item may be shown to `actor` in `target`.
/// Personal items follow the cross-scene policy of the facts and messages they
/// came from. Group and bot items were public in their scope, so the policy
/// applies to that scope as it does to group messages.
fn audience(
    db: &Connection,
    item: &Item,
    policy: CrossScenePolicy,
    target: &Scope,
    actor: i64,
) -> Result<bool> {
    if matches!(item.subject, Subject::Scope | Subject::Bot) {
        return Ok(item.bot_id == target.bot_id
            && policy::allows_scope_key(policy, &item.scope, target, actor));
    }
    recall_audience(db, item, policy, target, actor)
}

/// Who may have an item recalled for them. Group and bot items stay in their
/// own scope unless sharing across groups was explicitly enabled.
fn recall_audience(
    db: &Connection,
    item: &Item,
    policy: CrossScenePolicy,
    target: &Scope,
    actor: i64,
) -> Result<bool> {
    if item.bot_id != target.bot_id || actor <= 0 || actor == target.bot_id {
        return Ok(false);
    }
    let keys: Vec<Source> = sources(db, item.id)?;
    match item.subject {
        Subject::User => {
            if actor != item.subject_id
                || !item.scope.is_empty()
                    && !policy::allows_scope_key(policy, &item.scope, target, actor)
            {
                return Ok(false);
            }
            for source in &keys {
                if source.role != "memory" {
                    continue;
                }
                let id: i64 = source.source_key["memory:".len()..].parse()?;
                let origin: Option<String> = db.query_row("SELECT c.scope FROM ai_memories m JOIN ai_conversations c ON c.id=m.conversation_id WHERE m.id=?1", [id], |r| r.get(0)).optional()?;
                if !origin.is_some_and(|o| policy::allows_scope_key(policy, &o, target, actor)) {
                    return Ok(false);
                }
            }
            let messages: Vec<String> = keys
                .iter()
                .filter(|s| s.role != "memory")
                .map(|s| s.source_key.clone())
                .collect();
            policy::allows_sources(db, policy, &messages, target, actor)
        }
        Subject::Scope | Subject::Bot => {
            if item.scope == target.key() {
                return Ok(true);
            }
            let source = continuity::parse_scope(&item.scope)?;
            Ok(policy == CrossScenePolicy::Shared
                && source.scene == Scene::Group
                && target.scene == Scene::Group)
        }
    }
}

pub struct Recalled {
    pub text: String,
    pub dependency: Dependency,
    pub sources: Vec<String>,
}

pub struct RecallRequest<'a> {
    pub scope: &'a Scope,
    pub actor: i64,
    pub policy: CrossScenePolicy,
    /// Personal long-term memory domain of the current conversation.
    pub domain: &'a str,
    pub query: &'a str,
    pub budget: usize,
    /// Request time; relative ages are computed from here.
    pub reference: i64,
    /// False for multi-author automatic batches: no person's items are read.
    pub personal: bool,
}

fn days_ago(reference: i64, at: i64) -> i64 {
    reference.saturating_sub(at).max(0) / 86400
}

fn render(item: &Item, reference: i64, evidence: usize) -> String {
    let age = days_ago(reference, item.updated_at);
    let when = if age == 0 {
        "一天内".to_owned()
    } else {
        format!("约{age}天前")
    };
    match item.kind {
        Kind::Impression => format!(
            "{}（低置信归纳，只用来把握语气，不是事实）/ 关于用户 {}：{}（据 {} 条记忆，{}整理）",
            item.kind.label(),
            item.subject_id,
            item.text,
            evidence,
            when
        ),
        Kind::Episode if item.subject == Subject::User => format!(
            "{} / 与用户 {}：{}（{}）",
            item.kind.label(),
            item.subject_id,
            item.text,
            when
        ),
        Kind::Episode => format!("{} / 本群：{}（{}）", item.kind.label(), item.text, when),
        Kind::Statement => format!(
            "{}（保持前后一致即可，当前人设优先，不必复述）：{}（{}）",
            item.kind.label(),
            item.text,
            when
        ),
        _ => format!(
            "{}（相处经验，只调节语气和参与方式）：{}（可信度 {:.1}，依据 {} 条真人反应，{}更新）",
            item.kind.label(),
            item.text,
            item.confidence,
            evidence,
            when
        ),
    }
}

/// Lexical overlap with the request, as a small ranking signal only.
fn overlap(query: &HashSet<String>, text: &str) -> usize {
    memory::lexical_terms(text)
        .into_iter()
        .filter(|t| t.chars().count() >= 2 && query.contains(t))
        .count()
}

pub fn recall(db: &Connection, request: &RecallRequest<'_>) -> Result<Vec<Recalled>> {
    let Some((config, character)) = settings(db)? else {
        return Ok(vec![]);
    };
    if !config.enabled || request.budget == 0 {
        return Ok(vec![]);
    }
    let bot = request.scope.bot_id;
    let scope = request.scope.key();
    let query: HashSet<String> = memory::lexical_terms(request.query).into_iter().collect();
    let select = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> Result<Vec<Item>> {
        db.prepare(&format!("SELECT {ITEM_COLUMNS} FROM ai_knowledge WHERE bot_id=?1 AND status='active' AND (expires_at IS NULL OR expires_at>?2) AND {sql} ORDER BY importance DESC,updated_at DESC,id DESC LIMIT {CANDIDATES}"))?
            .query_map(args, item_row)?
            .map(|r| decode(r?))
            .collect()
    };
    let time = now();
    let mut groups: Vec<(Vec<Item>, usize)> = Vec::new();
    if request.personal {
        groups.push((
            select(
                "kind='impression' AND subject='user' AND subject_id=?3 AND domain=?4",
                &[&bot, &time, &request.actor, &request.domain],
            )?,
            IMPRESSION_LIMIT,
        ));
    }
    let mut episodes = select(
        "kind='episode' AND subject='scope' AND scope=?3",
        &[&bot, &time, &scope],
    )?;
    if request.personal {
        episodes.extend(select(
            "kind='episode' AND subject='user' AND subject_id=?3",
            &[&bot, &time, &request.actor],
        )?);
    }
    groups.push((episodes, EPISODE_LIMIT));
    // Scope-bound items come from this chat, or from any group when sharing
    // across scenes was explicitly enabled; audience() checks each again.
    let groups_pattern = format!("{bot}:group:%");
    let shared = request.policy == CrossScenePolicy::Shared && request.scope.scene == Scene::Group;
    let scoped = if shared {
        "(scope=?3 OR scope LIKE ?4)"
    } else {
        "(scope=?3 OR ?4='')"
    };
    let pattern = if shared { groups_pattern.as_str() } else { "-" };
    groups.push((
        select(
            &format!("kind='self' AND subject='bot' AND character_id=?5 AND {scoped}"),
            &[&bot, &time, &scope, &pattern, &character],
        )?,
        SELF_LIMIT,
    ));
    groups.push((
        select(
            &format!("kind IN('norm','style','joke','avoid') AND subject='scope' AND {scoped}"),
            &[&bot, &time, &scope, &pattern],
        )?,
        BEHAVIOR_LIMIT,
    ));
    let mut result = Vec::new();
    let mut used = 0usize;
    for (mut items, limit) in groups {
        // Relevance first, then the stored order (importance, recency).
        items.sort_by_key(|item| std::cmp::Reverse(overlap(&query, &item.text)));
        let mut taken = 0;
        for item in items {
            if taken >= limit {
                break;
            }
            if usable(db, &item).is_err()
                || !recall_audience(db, &item, request.policy, request.scope, request.actor)?
            {
                continue;
            }
            let item_sources = sources(db, item.id)?;
            let evidence = item_sources
                .iter()
                .filter(|s| matches!(s.role.as_str(), "evidence" | "memory"))
                .count();
            // Bot replies are tracked by this item's own dependency; human
            // sources also join the turn's revocation chain.
            let text = render(&item, request.reference, evidence);
            let cost = memory::estimate_tokens(&text) + 16;
            if used + cost > request.budget {
                continue;
            }
            used += cost;
            taken += 1;
            result.push(Recalled {
                text,
                dependency: Dependency {
                    id: item.id,
                    version: item.version,
                },
                sources: item_sources
                    .into_iter()
                    .filter(|s| matches!(s.role.as_str(), "evidence" | "context"))
                    .map(|s| s.source_key)
                    .collect(),
            });
        }
    }
    Ok(result)
}

/// Periodic work: evaluate reply outcomes (database only), then the optional
/// model-backed jobs. Each job has its own interval and request quota.
pub async fn run(
    host: &crate::host::Host,
    bot: i64,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    let config = host.config.plugins.ai.knowledge.clone();
    if !config.enabled || cancel.is_cancelled() {
        return Ok(());
    }
    let window = i64::try_from(config.outcome_window_seconds)?;
    host.store
        .call(move |db| {
            maintain(db)?;
            outcomes::evaluate(db, bot, window)?;
            Ok(())
        })
        .await?;
    let mut failure = None;
    if config.behavior
        && let Err(error) = behavior::run(host, bot, cancel).await
    {
        failure = Some(error);
    }
    if config.impressions
        && let Err(error) = reflection::run(host, bot, cancel).await
    {
        failure = Some(error);
    }
    failure.map_or(Ok(()), Err)
}

pub(crate) fn job_due(db: &Connection, job: &str, key: &str, interval: i64) -> Result<bool> {
    let last: i64 = db
        .query_row(
            "SELECT last_attempt FROM ai_knowledge_jobs WHERE job=?1 AND key=?2",
            params![job, key],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    if now().saturating_sub(last) < interval {
        return Ok(false);
    }
    db.execute("INSERT INTO ai_knowledge_jobs(job,key,last_attempt) VALUES(?1,?2,?3) ON CONFLICT(job,key) DO UPDATE SET last_attempt=excluded.last_attempt", params![job, key, now()])?;
    Ok(true)
}

pub(crate) fn job_cursor(db: &Connection, job: &str, key: &str) -> Result<i64> {
    Ok(db
        .query_row(
            "SELECT cursor FROM ai_knowledge_jobs WHERE job=?1 AND key=?2",
            params![job, key],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

pub(crate) fn advance_cursor(db: &Connection, job: &str, key: &str, cursor: i64) -> Result<()> {
    db.execute("INSERT INTO ai_knowledge_jobs(job,key,cursor) VALUES(?1,?2,?3) ON CONFLICT(job,key) DO UPDATE SET cursor=max(cursor,excluded.cursor)", params![job, key, cursor])?;
    Ok(())
}

/// Strict JSON from a background model, with an optional single code fence.
pub(crate) fn response_json<T: serde::de::DeserializeOwned>(
    response: &crate::llm::LlmResponse,
) -> Result<T> {
    ensure!(
        response
            .message
            .get("tool_calls")
            .is_none_or(|v| v.is_null() || v.as_array().is_some_and(Vec::is_empty)),
        "知识整理不允许工具调用"
    );
    let content = response
        .message
        .get("content")
        .and_then(Value::as_str)
        .context("知识整理没有返回 JSON")?
        .trim();
    ensure!(content.len() <= 64_000, "知识整理输出过长");
    let body = content
        .strip_prefix("```json")
        .or_else(|| content.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .unwrap_or(content);
    serde_json::from_str(body.trim()).context("知识整理输出必须是有效 JSON")
}

// ---------------------------------------------------------------------------
// Administration

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Filter {
    pub kind: Option<String>,
    pub status: Option<String>,
    pub page: usize,
}

pub fn snapshot(db: &Connection, bot: i64, filter: &Filter) -> Result<Value> {
    let kind = filter.kind.as_deref().filter(|k| !k.is_empty());
    if let Some(kind) = kind {
        Kind::parse(kind)?;
    }
    let status = filter.status.as_deref().filter(|s| !s.is_empty());
    ensure!(
        status.is_none_or(|s| matches!(s, "active" | "pending" | "rejected")),
        "invalid knowledge status"
    );
    let page = filter.page.max(1);
    let total: i64 = db.query_row(
        "SELECT count(*) FROM ai_knowledge WHERE bot_id=?1 AND (?2 IS NULL OR kind=?2) AND (?3 IS NULL OR status=?3)",
        params![bot, kind, status],
        |r| r.get(0),
    )?;
    let items = db
        .prepare(&format!("SELECT {ITEM_COLUMNS} FROM ai_knowledge WHERE bot_id=?1 AND (?2 IS NULL OR kind=?2) AND (?3 IS NULL OR status=?3) ORDER BY status='pending' DESC,updated_at DESC,id DESC LIMIT 20 OFFSET ?4"))?
        .query_map(params![bot, kind, status, ((page - 1) * 20) as i64], item_row)?
        .map(|r| decode(r?))
        .collect::<Result<Vec<_>>>()?;
    let mut rendered = Vec::new();
    for item in items {
        let stats = stats::read(db, &format!("knowledge:{}", item.id))?;
        let valid = usable(db, &item).is_ok();
        rendered
            .push(json!({"item":item,"sources":sources(db,item.id)?,"stats":stats,"usable":valid}));
    }
    let counts = db
        .prepare("SELECT kind,status,count(*) FROM ai_knowledge WHERE bot_id=?1 GROUP BY kind,status")?
        .query_map([bot], |r| {
            Ok(json!({"kind":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?,"count":r.get::<_,i64>(2)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({
        "bot_id": bot.to_string(),
        "settings": settings(db)?.map(|(c, character)| json!({"config":c,"character_id":character})),
        "items": rendered,
        "counts": counts,
        "page": page,
        "total_pages": ((total + 19) / 20).max(1),
        "outcomes": outcomes::overview(db, bot)?,
    }))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Approve,
    Reject,
    Delete,
    Lock,
    Unlock,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    pub bot_id: String,
    pub id: i64,
    pub version: i64,
    pub request_id: String,
    pub action: Action,
}

/// Idempotent administrator changes with optimistic version checks.
pub fn mutate(
    db: &mut Connection,
    bot: i64,
    request: &Mutation,
    admin: &str,
    session: &str,
) -> Result<Value> {
    ensure!(
        request.bot_id == bot.to_string()
            && !request.request_id.is_empty()
            && request.request_id.len() <= 128,
        "invalid knowledge mutation"
    );
    let encoded = hash(&serde_json::to_string(request)?);
    let tx = db.transaction()?;
    if let Some((prior, result)) = tx.query_row("SELECT request_hash,result FROM ai_knowledge_operations WHERE admin_id=?1 AND session_id=?2 AND request_id=?3", params![admin, session, request.request_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).optional()? {
        ensure!(encoded == prior, "knowledge operation conflict");
        return Ok(serde_json::from_str(&result)?);
    }
    let item = get(&tx, request.id)?.context("知识条目不存在")?;
    ensure!(
        item.bot_id == bot && item.version == request.version,
        "知识条目已变化，请刷新后重试"
    );
    match request.action {
        Action::Approve => {
            ensure!(item.status != "active", "知识条目已启用");
            tx.execute("UPDATE ai_knowledge SET status='active',version=version+1,updated_at=?2 WHERE id=?1", params![item.id, now()])?;
        }
        Action::Reject => {
            tx.execute("UPDATE ai_knowledge SET status='rejected',version=version+1,updated_at=?2 WHERE id=?1", params![item.id, now()])?;
        }
        Action::Delete => {
            tx.execute("DELETE FROM ai_knowledge WHERE id=?1", [item.id])?;
        }
        Action::Lock | Action::Unlock => {
            tx.execute(
                "UPDATE ai_knowledge SET locked=?2,version=version+1 WHERE id=?1",
                params![item.id, matches!(request.action, Action::Lock)],
            )?;
        }
    }
    let result = json!({"status":"applied","id":item.id,"version":item.version+1});
    tx.execute(
        "INSERT INTO ai_knowledge_operations VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            admin,
            session,
            request.request_id,
            encoded,
            serde_json::to_string(&result)?,
            now()
        ],
    )?;
    tx.execute(
        "DELETE FROM ai_knowledge_operations WHERE created_at<?1",
        [now() - 7 * 86400],
    )?;
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
mod tests;
