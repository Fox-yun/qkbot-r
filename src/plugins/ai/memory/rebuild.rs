//! Explicit, resumable history extraction. This ledger never owns the live
//! summary or either normal maintenance cursor, and existing memories are read-only.
use super::{
    BatchUpdate, Evidence, MAX_CHANGES, MemoryChange, MemoryKind, MessageSource, check_guard, now,
    source_by_key, validate_profile, validate_text, validate_ttl, write_memory,
};
use crate::{config::AiSettings, types::ContextGuard};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS ai_memory_rebuild_runs (
 id TEXT PRIMARY KEY, bot_id INTEGER NOT NULL, defaults TEXT NOT NULL,
 created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS ai_memory_rebuild_jobs (
 run_id TEXT NOT NULL, conversation_id TEXT NOT NULL, subject_id INTEGER NOT NULL,
 boundary TEXT NOT NULL, upper_message_id INTEGER NOT NULL,
 cursor INTEGER NOT NULL DEFAULT 0, version INTEGER NOT NULL DEFAULT 0,
 completed INTEGER NOT NULL DEFAULT 0, batches INTEGER NOT NULL DEFAULT 0,
 sources INTEGER NOT NULL DEFAULT 0, inserted INTEGER NOT NULL DEFAULT 0,
 duplicates INTEGER NOT NULL DEFAULT 0, expired INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(run_id,conversation_id,subject_id)
);
"#;

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub run_id: String,
    pub bot_id: i64,
    pub jobs: usize,
    pub completed_jobs: usize,
    pub batches: usize,
    pub sources: usize,
    pub inserted: usize,
    pub duplicates: usize,
    pub expired: usize,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct CommitStats {
    pub sources: usize,
    pub inserted: usize,
    pub duplicates: usize,
    pub expired: usize,
    pub completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Boundary {
    scope: String,
    epoch: i64,
    mode: String,
    owner_id: Option<i64>,
    settings: String,
    scope_mode: String,
    scope_revision: i64,
    access_revision: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceMetadata {
    scope: String,
    origin_plugin: Option<String>,
    access_revision: i64,
}

#[derive(Debug, Clone)]
pub struct Batch {
    pub guard: ContextGuard,
    pub subject_id: i64,
    pub sources: Vec<MessageSource>,
    run_id: String,
    boundary: Boundary,
    cursor: i64,
    through: i64,
    upper: i64,
    version: i64,
    memory_revision: i64,
    existing: Vec<Value>,
    source_metadata: Vec<SourceMetadata>,
}

impl Batch {
    /// Existing rows are a complete read-only deduplication reference. The
    /// caller must reject an oversized request, never truncate this collection.
    pub fn input(&self) -> Value {
        let existing: Vec<_> = self
            .existing
            .iter()
            .map(|memory| {
                json!({
                    "id":memory["id"],"subject_id":memory["subject_id"],"kind":memory["kind"],
                    "text":memory["text"],"origin":memory["origin"],"active":memory["active"],
                    "locked":memory["locked"],"expires_at":memory["expires_at"],
                })
            })
            .collect();
        json!({
            "conversation_id":self.guard.conversation_id,
            "subject_id":self.subject_id,
            "previous_summary":"",
            "summary_enabled":false,
            "memory_write":true,
            "sources":self.sources,
            "allowed_memories":[],
            "existing_memories":existing,
            "existing_memories_read_only":true,
        })
    }
}

fn validate_run_id(run_id: &str) -> Result<()> {
    ensure!(
        !run_id.is_empty()
            && run_id.len() <= 64
            && run_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
        "rebuild run ID must contain 1..=64 ASCII letters, digits, '-', '_' or '.'"
    );
    Ok(())
}

fn run_config(db: &Connection, run_id: &str, defaults: &AiSettings) -> Result<i64> {
    validate_run_id(run_id)?;
    let (bot_id, stored): (i64, String) = db
        .query_row(
            "SELECT bot_id,defaults FROM ai_memory_rebuild_runs WHERE id=?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .context("rebuild run not found")?;
    ensure!(
        stored == serde_json::to_string(defaults)?,
        "rebuild defaults changed; start a new run"
    );
    Ok(bot_id)
}

fn boundary(
    db: &Connection,
    conversation_id: &str,
    bot_id: i64,
    defaults: &AiSettings,
) -> Result<Option<Boundary>> {
    let (scope, epoch, mode, owner_id, settings): (String, i64, String, Option<i64>, String) = db
        .query_row(
        "SELECT scope,epoch,mode,owner_id,settings FROM ai_conversations WHERE id=?1",
        [conversation_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let parts: Vec<_> = scope.split(':').collect();
    ensure!(
        parts.len() == 3
            && parts[0].parse::<i64>().ok() == Some(bot_id)
            && parts[2].parse::<i64>().is_ok_and(|id| id > 0),
        "invalid rebuild account or scope"
    );
    let (scope_mode, scope_revision) = match parts[1] {
        "friend" => ("user".to_owned(), 0),
        "group" => db
            .query_row(
                "SELECT mode,revision FROM ai_scope_modes WHERE scope=?1",
                [&scope],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?
            .unwrap_or_else(|| (defaults.mode.clone(), 0)),
        _ => anyhow::bail!("invalid rebuild chat type"),
    };
    let expected_id = match owner_id {
        Some(owner) if owner > 0 && scope_mode == "user" => {
            format!("{scope}:v{scope_revision}:user:{owner}")
        }
        None if scope_mode == "group" => format!("{scope}:v{scope_revision}:group"),
        _ => return Ok(None),
    };
    if conversation_id != expected_id || mode != scope_mode {
        return Ok(None);
    }
    let effective = crate::plugins::ai::resolve_settings(defaults, &settings)?;
    if effective.paused || !effective.memory_write {
        return Ok(None);
    }
    let Some(access_revision) = crate::access::allowed_revision(db, &scope)? else {
        return Ok(None);
    };
    Ok(Some(Boundary {
        scope,
        epoch,
        mode,
        owner_id,
        settings,
        scope_mode,
        scope_revision,
        access_revision,
    }))
}

/// Create an immutable set of current-epoch source windows, or resume this exact
/// run. Opening this module never activates plugins or repairs the host outbox.
pub fn start(
    db: &mut Connection,
    run_id: &str,
    bot_id: i64,
    defaults: &AiSettings,
) -> Result<Status> {
    validate_run_id(run_id)?;
    ensure!(
        bot_id > 0,
        "rebuild requires an explicit positive bot account"
    );
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA)?;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM ai_memory_rebuild_runs WHERE id=?1)",
        [run_id],
        |r| r.get(0),
    )?;
    if exists {
        ensure!(
            run_config(&tx, run_id, defaults)? == bot_id,
            "rebuild run belongs to another bot"
        );
    } else {
        tx.execute(
            "INSERT INTO ai_memory_rebuild_runs(id,bot_id,defaults,created_at) VALUES(?1,?2,?3,?4)",
            params![run_id, bot_id, serde_json::to_string(defaults)?, now()],
        )?;
        let conversations = tx
            .prepare("SELECT id FROM ai_conversations WHERE scope LIKE ?1 ORDER BY id")?
            .query_map([format!("{bot_id}:%")], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in conversations {
            let Some(boundary) = boundary(&tx, &id, bot_id, defaults)? else {
                continue;
            };
            let subjects = tx.prepare("SELECT sender_id,MAX(id) FROM messages m WHERE conversation_id=?1 AND epoch=?2 AND scope=?3 AND sender_id>0 AND sender_id!=?4 AND role='user' AND learnable=1 AND revoked=0 AND COALESCE(origin_plugin,'')!='control' AND NOT EXISTS(SELECT 1 FROM ai_revoked_sources r WHERE r.source_key=m.key) GROUP BY sender_id ORDER BY sender_id")?
                .query_map(params![id,boundary.epoch,boundary.scope,bot_id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (subject, upper) in subjects {
                if boundary.owner_id.is_some_and(|owner| owner != subject) {
                    continue;
                }
                tx.execute("INSERT INTO ai_memory_rebuild_jobs(run_id,conversation_id,subject_id,boundary,upper_message_id) VALUES(?1,?2,?3,?4,?5)",
                    params![run_id,id,subject,serde_json::to_string(&boundary)?,upper])?;
            }
        }
    }
    tx.commit()?;
    status(db, run_id)
}

pub fn status(db: &Connection, run_id: &str) -> Result<Status> {
    validate_run_id(run_id)?;
    let bot_id = db
        .query_row(
            "SELECT bot_id FROM ai_memory_rebuild_runs WHERE id=?1",
            [run_id],
            |r| r.get(0),
        )
        .optional()?
        .context("rebuild run not found")?;
    Ok(db.query_row("SELECT COUNT(*),COALESCE(SUM(completed),0),COALESCE(SUM(batches),0),COALESCE(SUM(sources),0),COALESCE(SUM(inserted),0),COALESCE(SUM(duplicates),0),COALESCE(SUM(expired),0) FROM ai_memory_rebuild_jobs WHERE run_id=?1",[run_id],|r|Ok(Status {
        run_id:run_id.to_owned(),bot_id,jobs:r.get(0)?,completed_jobs:r.get(1)?,batches:r.get(2)?,sources:r.get(3)?,inserted:r.get(4)?,duplicates:r.get(5)?,expired:r.get(6)?,
    }))?)
}

fn existing_memories(db: &Connection, conversation_id: &str, subject: i64) -> Result<Vec<Value>> {
    Ok(db.prepare("SELECT id,subject_id,kind,text,evidence,expires_at,origin,active,locked,version,created_at,updated_at,edited_by FROM ai_memories WHERE conversation_id=?1 AND subject_id=?2 ORDER BY id")?
        .query_map(params![conversation_id,subject],|r|Ok(json!({
            "id":r.get::<_,i64>(0)?,"subject_id":r.get::<_,i64>(1)?,"kind":r.get::<_,String>(2)?,
            "text":r.get::<_,String>(3)?,"evidence":r.get::<_,String>(4)?,"expires_at":r.get::<_,Option<i64>>(5)?,
            "origin":r.get::<_,String>(6)?,"active":r.get::<_,bool>(7)?,"locked":r.get::<_,bool>(8)?,
            "version":r.get::<_,i64>(9)?,"created_at":r.get::<_,i64>(10)?,"updated_at":r.get::<_,i64>(11)?,
            "edited_by":r.get::<_,Option<String>>(12)?,
        })))?.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn source_metadata(db: &Connection, key: &str) -> Result<SourceMetadata> {
    Ok(db.query_row(
        "SELECT scope,origin_plugin,access_revision FROM messages WHERE key=?1",
        [key],
        |r| {
            Ok(SourceMetadata {
                scope: r.get(0)?,
                origin_plugin: r.get(1)?,
                access_revision: r.get(2)?,
            })
        },
    )?)
}

/// A consistent read snapshot; no source or existing-memory collection is
/// silently shortened except for the caller's explicit source-row limit.
pub fn prepare(
    db: &Connection,
    run_id: &str,
    defaults: &AiSettings,
    max_sources: usize,
) -> Result<Option<Batch>> {
    ensure!(
        (1..=48).contains(&max_sources),
        "rebuild source limit must be 1..=48"
    );
    let tx = db.unchecked_transaction()?;
    let bot_id = run_config(&tx, run_id, defaults)?;
    let job: Option<(String,i64,String,i64,i64,i64)> = tx.query_row(
        "SELECT conversation_id,subject_id,boundary,upper_message_id,cursor,version FROM ai_memory_rebuild_jobs WHERE run_id=?1 AND completed=0 ORDER BY conversation_id,subject_id LIMIT 1",
        [run_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
    let Some((conversation_id, subject_id, stored, upper, cursor, version)) = job else {
        tx.commit()?;
        return Ok(None);
    };
    let frozen: Boundary = serde_json::from_str(&stored)?;
    ensure!(
        boundary(&tx, &conversation_id, bot_id, defaults)?.as_ref() == Some(&frozen),
        "rebuild conversation boundary changed; start a new run"
    );
    let guard = ContextGuard {
        conversation_id,
        epoch: frozen.epoch,
    };
    let keys=tx.prepare("SELECT key FROM messages m WHERE conversation_id=?1 AND epoch=?2 AND scope=?3 AND sender_id=?4 AND sender_id!=?8 AND role='user' AND learnable=1 AND revoked=0 AND COALESCE(origin_plugin,'')!='control' AND id>?5 AND id<=?6 AND NOT EXISTS(SELECT 1 FROM ai_revoked_sources r WHERE r.source_key=m.key) ORDER BY id LIMIT ?7")?
        .query_map(params![guard.conversation_id,guard.epoch,frozen.scope,subject_id,cursor,upper,max_sources as i64,bot_id],|r|r.get::<_,String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut sources = Vec::with_capacity(keys.len());
    let mut metadata = Vec::with_capacity(keys.len());
    for key in keys {
        let mut source = source_by_key(&tx, &guard, &key)?.context("rebuild source disappeared")?;
        source.memory_eligible = true;
        metadata.push(source_metadata(&tx, &key)?);
        sources.push(source);
    }
    let through = if sources.len() < max_sources {
        upper
    } else {
        sources.last().unwrap().position
    };
    let memory_revision = tx.query_row(
        "SELECT memory_revision FROM ai_conversations WHERE id=?1",
        [&guard.conversation_id],
        |r| r.get(0),
    )?;
    let existing = existing_memories(&tx, &guard.conversation_id, subject_id)?;
    tx.commit()?;
    Ok(Some(Batch {
        guard,
        subject_id,
        sources,
        run_id: run_id.to_owned(),
        boundary: frozen,
        cursor,
        through,
        upper,
        version,
        memory_revision,
        existing,
        source_metadata: metadata,
    }))
}

fn normalized_text(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn evidence_time(batch: &Batch, evidence: &[Evidence]) -> Result<i64> {
    ensure!(
        !evidence.is_empty() && evidence.len() <= 3,
        "a rebuild memory needs 1..=3 evidence quotes"
    );
    let mut latest = i64::MIN;
    for quote in evidence {
        ensure!(
            !quote.quote.trim().is_empty() && quote.quote.chars().count() <= 200,
            "invalid rebuild evidence quote length"
        );
        let source = batch
            .sources
            .iter()
            .find(|s| s.key == quote.source_key)
            .context("rebuild evidence was not in this batch")?;
        ensure!(
            source.memory_eligible
                && source.learnable
                && source.role == "user"
                && source.subject_id == batch.subject_id
                && source.text.contains(&quote.quote),
            "rebuild evidence must quote its human subject exactly"
        );
        latest = latest.max(source.received_at.timestamp());
    }
    Ok(latest)
}

struct Insert {
    kind: MemoryKind,
    text: String,
    evidence: Vec<Evidence>,
    expires_at: Option<i64>,
}

/// Revalidate every source and authorization coordinate without writing. Callers
/// should hold a short transaction while validating and reserving an outbound
/// request, and must not retain that transaction across network awaits.
pub fn validate_snapshot(db: &Connection, batch: &Batch, defaults: &AiSettings) -> Result<()> {
    let bot_id = run_config(db, &batch.run_id, defaults)?;
    check_guard(db, &batch.guard)?;
    ensure!(
        boundary(db, &batch.guard.conversation_id, bot_id, defaults)?.as_ref()
            == Some(&batch.boundary),
        "rebuild conversation boundary changed"
    );
    let job:(String,i64,i64,i64,bool)=db.query_row("SELECT boundary,upper_message_id,cursor,version,completed FROM ai_memory_rebuild_jobs WHERE run_id=?1 AND conversation_id=?2 AND subject_id=?3",params![batch.run_id,batch.guard.conversation_id,batch.subject_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    ensure!(
        job.0 == serde_json::to_string(&batch.boundary)?
            && job.1 == batch.upper
            && job.2 == batch.cursor
            && job.3 == batch.version
            && !job.4,
        "rebuild checkpoint changed"
    );
    ensure!(
        batch.through > batch.cursor && batch.through <= batch.upper,
        "invalid rebuild source window"
    );
    let revision: i64 = db.query_row(
        "SELECT memory_revision FROM ai_conversations WHERE id=?1",
        [&batch.guard.conversation_id],
        |r| r.get(0),
    )?;
    ensure!(
        revision == batch.memory_revision,
        "memories changed while rebuilding"
    );
    ensure!(
        existing_memories(db, &batch.guard.conversation_id, batch.subject_id)? == batch.existing,
        "existing rebuild references changed"
    );
    ensure!(
        batch.sources.len() == batch.source_metadata.len(),
        "invalid rebuild source snapshot"
    );
    let current_keys=db.prepare("SELECT key FROM messages m WHERE conversation_id=?1 AND epoch=?2 AND scope=?3 AND sender_id=?4 AND sender_id!=?7 AND role='user' AND learnable=1 AND revoked=0 AND COALESCE(origin_plugin,'')!='control' AND id>?5 AND id<=?6 AND NOT EXISTS(SELECT 1 FROM ai_revoked_sources r WHERE r.source_key=m.key) ORDER BY id LIMIT 49")?
        .query_map(params![batch.guard.conversation_id,batch.guard.epoch,batch.boundary.scope,batch.subject_id,batch.cursor,batch.through,bot_id],|r|r.get::<_,String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        current_keys
            == batch
                .sources
                .iter()
                .map(|s| s.key.clone())
                .collect::<Vec<_>>(),
        "rebuild source window changed"
    );
    for (frozen, metadata) in batch.sources.iter().zip(&batch.source_metadata) {
        let mut current = source_by_key(db, &batch.guard, &frozen.key)?
            .context("rebuild source was revoked or removed")?;
        current.memory_eligible = true;
        ensure!(
            current.position > batch.cursor
                && current.position <= batch.through
                && current.subject_id == batch.subject_id
                && current.role == "user"
                && current.learnable,
            "rebuild source is ineligible"
        );
        ensure!(
            serde_json::to_value(&current)? == serde_json::to_value(frozen)?
                && source_metadata(db, &frozen.key)? == *metadata
                && metadata.scope == batch.boundary.scope
                && metadata.origin_plugin.as_deref() != Some("control"),
            "rebuild source changed"
        );
    }
    Ok(())
}

/// Validate the complete response, then insert and checkpoint in one transaction.
/// Normal maintenance can move its cursors while the model runs; memory edits,
/// source changes, resets and permission changes invalidate this snapshot.
pub fn commit(
    db: &mut Connection,
    batch: &Batch,
    update: BatchUpdate,
    defaults: &AiSettings,
    capacity: usize,
) -> Result<CommitStats> {
    ensure!(
        update.summary.is_empty(),
        "rebuild cannot replace the conversation summary"
    );
    ensure!(
        update.changes.len() <= MAX_CHANGES,
        "too many rebuild memory changes"
    );
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_snapshot(&tx, batch, defaults)?;
    let mut seen: HashSet<(String, String)> = batch
        .existing
        .iter()
        .map(|m| {
            Ok((
                m["kind"]
                    .as_str()
                    .context("invalid stored memory kind")?
                    .to_owned(),
                normalized_text(m["text"].as_str().context("invalid stored memory text")?),
            ))
        })
        .collect::<Result<_>>()?;
    let mut stats = CommitStats {
        sources: batch.sources.len(),
        completed: batch.through == batch.upper,
        ..Default::default()
    };
    let mut inserts = Vec::new();
    let timestamp = now();
    for change in update.changes {
        let MemoryChange::Upsert {
            id,
            subject_id,
            kind,
            text,
            evidence,
            ttl_days,
        } = change;
        ensure!(
            subject_id == batch.subject_id && subject_id > 0,
            "rebuild memory belongs to another subject"
        );
        validate_text(&text)?;
        validate_ttl(ttl_days)?;
        validate_profile(kind, &text, &evidence, ttl_days)?;
        let source_time = evidence_time(batch, &evidence)?;
        let expires_at = ttl_days
            .map(|days| {
                source_time
                    .checked_add(i64::from(days) * 86_400)
                    .context("rebuild TTL overflow")
            })
            .transpose()?;
        if let Some(id) = id {
            ensure!(
                batch.existing.iter().any(|m| m["id"].as_i64() == Some(id)),
                "rebuild reference ID was not supplied for this subject"
            );
            stats.duplicates += 1;
            continue;
        }
        let identity = (kind.as_str().to_owned(), normalized_text(&text));
        if seen.contains(&identity) {
            stats.duplicates += 1;
            continue;
        }
        if expires_at.is_some_and(|expiry| expiry <= timestamp) {
            stats.expired += 1;
            continue;
        }
        seen.insert(identity);
        inserts.push(Insert {
            kind,
            text,
            evidence,
            expires_at,
        });
    }
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM ai_memories WHERE conversation_id=?1",
        [&batch.guard.conversation_id],
        |r| r.get(0),
    )?;
    ensure!(
        inserts.is_empty() || usize::try_from(count)?.saturating_add(inserts.len()) <= capacity,
        "rebuild memory capacity exceeded; checkpoint was not advanced"
    );
    for insert in inserts {
        let id = write_memory(
            &tx,
            &batch.guard.conversation_id,
            None,
            batch.subject_id,
            insert.kind,
            &insert.text,
            &insert.evidence,
            None,
        )?;
        // No observer can see this row until the final transaction commit.
        tx.execute(
            "UPDATE ai_memories SET expires_at=?1 WHERE id=?2",
            params![insert.expires_at, id],
        )?;
        stats.inserted += 1;
    }
    ensure!(tx.execute("UPDATE ai_memory_rebuild_jobs SET cursor=?1,version=version+1,completed=?2,batches=batches+1,sources=sources+?3,inserted=inserted+?4,duplicates=duplicates+?5,expired=expired+?6 WHERE run_id=?7 AND conversation_id=?8 AND subject_id=?9 AND cursor=?10 AND version=?11 AND completed=0",params![batch.through,stats.completed,stats.sources,stats.inserted,stats.duplicates,stats.expired,batch.run_id,batch.guard.conversation_id,batch.subject_id,batch.cursor,batch.version])?==1,"rebuild checkpoint changed");
    tx.commit()?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONVERSATION: &str = "1:group:20:v0:group";

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE scope_access(scope TEXT PRIMARY KEY,enabled INTEGER NOT NULL,revision INTEGER NOT NULL);
            CREATE TABLE ai_scope_modes(scope TEXT PRIMARY KEY,mode TEXT NOT NULL,revision INTEGER NOT NULL);
            CREATE TABLE ai_conversations(id TEXT PRIMARY KEY,scope TEXT NOT NULL,mode TEXT NOT NULL,owner_id INTEGER,epoch INTEGER NOT NULL,settings TEXT NOT NULL DEFAULT '{}',summary TEXT NOT NULL DEFAULT '',compacted_until INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE messages(id INTEGER PRIMARY KEY AUTOINCREMENT,key TEXT UNIQUE NOT NULL,scope TEXT NOT NULL,conversation_id TEXT NOT NULL,epoch INTEGER NOT NULL,sender_id INTEGER NOT NULL,role TEXT NOT NULL,content TEXT NOT NULL,received_at INTEGER NOT NULL,inbound_position INTEGER NOT NULL,origin_plugin TEXT,learnable INTEGER NOT NULL DEFAULT 1,revoked INTEGER NOT NULL DEFAULT 0,sender_nickname TEXT,sender_card TEXT,access_revision INTEGER NOT NULL DEFAULT 0);
            INSERT INTO ai_scope_modes VALUES('1:group:20','group',0);
            INSERT INTO ai_conversations(id,scope,mode,epoch,summary,compacted_until) VALUES('1:group:20:v0:group','1:group:20','group',1,'现有摘要',900);").unwrap();
        db.execute_batch(super::super::MIGRATION).unwrap();
        db.execute_batch(super::super::CURSOR_MIGRATION).unwrap();
        db.execute_batch(super::super::ADMIN_MIGRATION).unwrap();
        db.execute(
            "UPDATE ai_conversations SET memory_processed_until=1000",
            [],
        )
        .unwrap();
        db
    }

    fn source(db: &Connection, id: i64, subject: i64, text: &str, time: i64) {
        db.execute("INSERT INTO messages(id,key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,sender_nickname) VALUES(?1,?2,'1:group:20',?3,1,?4,'user',?5,?6,?1,'虚构用户')",params![id,format!("1:group:20:{id}"),CONVERSATION,subject,text,time]).unwrap();
    }

    fn change(source: &MessageSource, text: &str, ttl_days: Option<u16>) -> MemoryChange {
        MemoryChange::Upsert {
            id: None,
            subject_id: source.subject_id,
            kind: MemoryKind::Event,
            text: text.to_owned(),
            evidence: vec![Evidence {
                source_key: source.key.clone(),
                quote: source.text.clone(),
            }],
            ttl_days,
        }
    }

    fn update(changes: Vec<MemoryChange>) -> BatchUpdate {
        BatchUpdate {
            summary: String::new(),
            changes,
        }
    }

    fn prepared(db: &mut Connection) -> Batch {
        start(db, "rebuild-test", 1, &AiSettings::default()).unwrap();
        prepare(db, "rebuild-test", &AiSettings::default(), 24)
            .unwrap()
            .unwrap()
    }

    fn commit_update(
        db: &mut Connection,
        batch: &Batch,
        changes: Vec<MemoryChange>,
    ) -> CommitStats {
        commit(db, batch, update(changes), &AiSettings::default(), 256).unwrap()
    }

    fn live_context(db: &Connection) -> (String, i64, i64, i64) {
        db.query_row("SELECT summary,compacted_until,memory_processed_until,epoch FROM ai_conversations WHERE id=?1",[CONVERSATION],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap()
    }

    fn row_count(db: &Connection) -> i64 {
        db.query_row("SELECT COUNT(*) FROM ai_memories", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn rebuild_frozen_window_preserves_live_context_and_resumes_without_duplicates() {
        let mut db = db();
        source(&db, 1, 7, "今天完成了测试部署", now());
        source(&db, 2, 7, "今天完成了复测", now());
        let before = live_context(&db);
        start(&mut db, "rebuild-test", 1, &AiSettings::default()).unwrap();
        source(&db, 3, 7, "窗口之外的新消息", now());
        let first = prepare(&db, "rebuild-test", &AiSettings::default(), 1)
            .unwrap()
            .unwrap();
        assert_eq!(first.sources[0].position, 1);
        let stats = commit_update(
            &mut db,
            &first,
            vec![change(&first.sources[0], "完成测试部署", None)],
        );
        assert!(!stats.completed);
        assert!(commit(&mut db, &first, update(vec![]), &AiSettings::default(), 256).is_err());
        let resumed = start(&mut db, "rebuild-test", 1, &AiSettings::default()).unwrap();
        assert_eq!(resumed.inserted, 1);
        let second = prepare(&db, "rebuild-test", &AiSettings::default(), 24)
            .unwrap()
            .unwrap();
        assert_eq!(second.sources.len(), 1);
        assert_eq!(second.sources[0].position, 2);
        commit_update(
            &mut db,
            &second,
            vec![change(&second.sources[0], "完成复测", None)],
        );
        assert!(
            prepare(&db, "rebuild-test", &AiSettings::default(), 24)
                .unwrap()
                .is_none()
        );
        assert_eq!(live_context(&db), before);
        assert_eq!(row_count(&db), 2);
        let status = status(&db, "rebuild-test").unwrap();
        assert_eq!(
            (
                status.jobs,
                status.completed_jobs,
                status.batches,
                status.sources,
                status.inserted
            ),
            (1, 1, 2, 2, 2)
        );
    }

    #[test]
    fn rebuild_sees_all_existing_rows_and_never_changes_locked_inactive_or_expired_memories() {
        let mut db = db();
        source(&db, 1, 7, "今天完成了测试部署", now());
        for index in 0..20 {
            let id = write_memory(
                &db,
                CONVERSATION,
                None,
                7,
                MemoryKind::Event,
                &format!("既有事件 {index}"),
                &[],
                None,
            )
            .unwrap();
            db.execute("UPDATE ai_memories SET origin='manual',locked=1,active=0,expires_at=1,version=5,edited_by='test-admin' WHERE id=?1",[id]).unwrap();
        }
        let before = existing_memories(&db, CONVERSATION, 7).unwrap();
        let batch = prepared(&mut db);
        assert_eq!(
            batch.input()["existing_memories"].as_array().unwrap().len(),
            20
        );
        assert_eq!(batch.input()["allowed_memories"], json!([]));
        let input = batch.input();
        let reference = &input["existing_memories"][0];
        assert_eq!(reference.as_object().unwrap().len(), 8);
        for private in [
            "evidence",
            "edited_by",
            "version",
            "created_at",
            "updated_at",
        ] {
            assert!(reference.get(private).is_none());
        }
        let mut explicit = change(&batch.sources[0], "模型认为与既有事件重复", None);
        let MemoryChange::Upsert { id, .. } = &mut explicit;
        *id = before[0]["id"].as_i64();
        let stats = commit_update(
            &mut db,
            &batch,
            vec![
                explicit,
                change(&batch.sources[0], "  既有事件   19  ", None),
            ],
        );
        assert_eq!((stats.inserted, stats.duplicates), (0, 2));
        assert_eq!(existing_memories(&db, CONVERSATION, 7).unwrap(), before);
        assert_eq!(row_count(&db), 20);
    }

    #[test]
    fn rebuild_deduplicates_same_batch_but_keeps_distinct_events_from_the_same_evidence() {
        let mut db = db();
        source(&db, 1, 7, "上午吃面，下午跑步", now());
        let batch = prepared(&mut db);
        let stats = commit_update(
            &mut db,
            &batch,
            vec![
                change(&batch.sources[0], "上午吃面", None),
                change(&batch.sources[0], "下午跑步", None),
                change(&batch.sources[0], "  上午吃面  ", None),
            ],
        );
        assert_eq!((stats.inserted, stats.duplicates), (2, 1));
        start(&mut db, "another-run", 1, &AiSettings::default()).unwrap();
        let next = prepare(&db, "another-run", &AiSettings::default(), 24)
            .unwrap()
            .unwrap();
        let stats = commit_update(
            &mut db,
            &next,
            vec![change(&next.sources[0], "上午吃面", None)],
        );
        assert_eq!((stats.inserted, stats.duplicates), (0, 1));
        assert_eq!(row_count(&db), 2);
    }

    #[test]
    fn rebuild_uses_evidence_time_for_ttl_and_does_not_resurrect_old_temporary_events() {
        let mut db = db();
        let recent = now() - 86_400;
        source(&db, 1, 7, "昨天完成部署", recent);
        source(&db, 2, 7, "很久以前临时约了复测", now() - 100 * 86_400);
        source(&db, 3, 7, "很久以前完成大学学业", now() - 100 * 86_400);
        let batch = prepared(&mut db);
        let stats = commit_update(
            &mut db,
            &batch,
            vec![
                change(&batch.sources[0], "昨天完成部署", Some(7)),
                change(&batch.sources[1], "临时约了复测", Some(30)),
                change(&batch.sources[2], "完成大学学业", None),
            ],
        );
        assert_eq!((stats.inserted, stats.expired), (2, 1));
        let expiry: Option<i64> = db
            .query_row(
                "SELECT expires_at FROM ai_memories WHERE text='昨天完成部署'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(expiry, Some(recent + 7 * 86_400));
        let permanent: Option<i64> = db
            .query_row(
                "SELECT expires_at FROM ai_memories WHERE text='完成大学学业'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(permanent, None);
        let indexed: i64 = db
            .query_row("SELECT COUNT(*) FROM ai_memory_search", [], |r| r.get(0))
            .unwrap();
        assert_eq!(indexed, 2);
    }

    #[test]
    fn rebuild_skips_old_epochs_revoked_control_and_unlearnable_sources_and_isolates_subjects() {
        let mut db = db();
        for id in 1..=9 {
            source(&db, id, if id == 9 { 8 } else { 7 }, "测试原文", now());
        }
        source(&db, 10, 1, "机器人自己的入站事件", now());
        db.execute("UPDATE messages SET epoch=0 WHERE id=2", [])
            .unwrap();
        db.execute("UPDATE messages SET revoked=1 WHERE id=3", [])
            .unwrap();
        db.execute(
            "INSERT INTO ai_revoked_sources VALUES('1:group:20:4',1)",
            [],
        )
        .unwrap();
        db.execute("UPDATE messages SET origin_plugin='control' WHERE id=5", [])
            .unwrap();
        db.execute("UPDATE messages SET learnable=0 WHERE id=6", [])
            .unwrap();
        db.execute("UPDATE messages SET role='assistant' WHERE id=7", [])
            .unwrap();
        db.execute("UPDATE messages SET scope='2:group:20' WHERE id=8", [])
            .unwrap();
        let batch = prepared(&mut db);
        assert_eq!(status(&db, "rebuild-test").unwrap().jobs, 2);
        assert_eq!(batch.subject_id, 7);
        assert_eq!(
            batch.sources.iter().map(|s| s.position).collect::<Vec<_>>(),
            vec![1]
        );
        commit_update(&mut db, &batch, vec![]);
        let next = prepare(&db, "rebuild-test", &AiSettings::default(), 24)
            .unwrap()
            .unwrap();
        assert_eq!(next.subject_id, 8);
        assert_eq!(
            next.sources.iter().map(|s| s.position).collect::<Vec<_>>(),
            vec![9]
        );
    }

    #[test]
    fn rebuild_rejects_old_scope_modes_disabled_access_and_memory_settings_at_start() {
        for alteration in [
            "UPDATE ai_scope_modes SET revision=1",
            "INSERT INTO scope_access VALUES('1:group:20',0,1)",
            "UPDATE ai_conversations SET settings='{\"memory_write\":false}'",
            "UPDATE ai_conversations SET settings='{\"paused\":true}'",
            "UPDATE ai_conversations SET mode='user',owner_id=8",
        ] {
            let mut db = db();
            source(&db, 1, 7, "原文", now());
            db.execute(alteration, []).unwrap();
            assert_eq!(
                start(&mut db, "rebuild-test", 1, &AiSettings::default())
                    .unwrap()
                    .jobs,
                0,
                "{alteration}"
            );
            assert!(
                prepare(&db, "rebuild-test", &AiSettings::default(), 24)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn rebuild_cas_rejects_reset_permission_settings_memory_and_source_changes() {
        for alteration in [
            "UPDATE ai_conversations SET epoch=epoch+1",
            "UPDATE ai_scope_modes SET revision=revision+1",
            "INSERT INTO scope_access VALUES('1:group:20',1,2)",
            "UPDATE ai_conversations SET settings='{\"memory_write\":false}'",
            "UPDATE ai_conversations SET memory_revision=memory_revision+1",
            "UPDATE messages SET content='替换后的原文' WHERE id=1",
            "UPDATE messages SET sender_id=8 WHERE id=1",
            "UPDATE messages SET received_at=received_at+1 WHERE id=1",
            "UPDATE messages SET origin_plugin='control' WHERE id=1",
            "UPDATE messages SET access_revision=2 WHERE id=1",
            "INSERT INTO ai_revoked_sources VALUES('1:group:20:1',1)",
            "DELETE FROM messages WHERE id=1",
        ] {
            let mut db = db();
            source(&db, 1, 7, "原文", now());
            let batch = prepared(&mut db);
            db.execute(alteration, []).unwrap();
            assert!(
                commit(
                    &mut db,
                    &batch,
                    update(vec![change(&batch.sources[0], "事件", None)]),
                    &AiSettings::default(),
                    256
                )
                .is_err(),
                "{alteration}"
            );
            assert_eq!(row_count(&db), 0, "{alteration}");
            let status = status(&db, "rebuild-test").unwrap();
            assert_eq!(
                (status.batches, status.completed_jobs),
                (0, 0),
                "{alteration}"
            );
        }
    }

    #[test]
    fn rebuild_bad_evidence_unknown_id_or_cross_subject_rejects_the_entire_response() {
        for invalid in 0..7 {
            let mut db = db();
            source(&db, 1, 7, "原文", now());
            let batch = prepared(&mut db);
            let good = change(&batch.sources[0], "有效事件", None);
            let mut bad = change(&batch.sources[0], "错误事件", None);
            let MemoryChange::Upsert {
                id,
                subject_id,
                text,
                evidence,
                ttl_days,
                ..
            } = &mut bad;
            match invalid {
                0 => *id = Some(987654),
                1 => *subject_id = 8,
                2 => evidence[0].source_key = "1:group:20:999".into(),
                3 => evidence[0].quote = "未曾说过".into(),
                4 => *ttl_days = Some(0),
                5 => *text = "字".repeat(181),
                _ => evidence.clear(),
            }
            assert!(
                commit(
                    &mut db,
                    &batch,
                    update(vec![good, bad]),
                    &AiSettings::default(),
                    256
                )
                .is_err()
            );
            assert_eq!(row_count(&db), 0);
            assert_eq!(status(&db, "rebuild-test").unwrap().batches, 0);
        }
    }

    #[test]
    fn rebuild_capacity_and_item_limit_do_not_partially_commit_or_checkpoint() {
        let mut db = db();
        source(&db, 1, 7, "上午吃面，下午跑步", now());
        let batch = prepared(&mut db);
        let changes = vec![
            change(&batch.sources[0], "上午吃面", None),
            change(&batch.sources[0], "下午跑步", None),
        ];
        assert!(commit(&mut db, &batch, update(changes), &AiSettings::default(), 1).is_err());
        let changes = (0..17)
            .map(|n| change(&batch.sources[0], &format!("事件 {n}"), None))
            .collect();
        assert!(
            commit(
                &mut db,
                &batch,
                update(changes),
                &AiSettings::default(),
                256
            )
            .is_err()
        );
        assert_eq!(row_count(&db), 0);
        assert_eq!(status(&db, "rebuild-test").unwrap().batches, 0);
        commit_update(
            &mut db,
            &batch,
            vec![change(&batch.sources[0], "上午吃面", None)],
        );
        assert_eq!(row_count(&db), 1);
    }

    #[test]
    fn rebuild_empty_tail_is_checkpointed_without_touching_normal_summary_or_cursors() {
        let mut db = db();
        source(&db, 1, 7, "原文", now());
        start(&mut db, "rebuild-test", 1, &AiSettings::default()).unwrap();
        db.execute("DELETE FROM messages", []).unwrap();
        let batch = prepare(&db, "rebuild-test", &AiSettings::default(), 24)
            .unwrap()
            .unwrap();
        assert!(batch.sources.is_empty());
        let before = live_context(&db);
        assert!(commit_update(&mut db, &batch, vec![]).completed);
        assert_eq!(live_context(&db), before);
        assert_eq!(status(&db, "rebuild-test").unwrap().completed_jobs, 1);
    }

    #[test]
    fn rebuild_normal_cursor_progress_does_not_conflict_but_summary_output_is_rejected() {
        let mut db = db();
        source(&db, 1, 7, "原文", now());
        let batch = prepared(&mut db);
        assert!(
            commit(
                &mut db,
                &batch,
                BatchUpdate {
                    summary: "不允许替换".into(),
                    changes: vec![]
                },
                &AiSettings::default(),
                256
            )
            .is_err()
        );
        db.execute("UPDATE ai_conversations SET compacted_until=1900,memory_processed_until=2000,summary='新摘要'",[]).unwrap();
        commit_update(
            &mut db,
            &batch,
            vec![change(&batch.sources[0], "事件", None)],
        );
        assert_eq!(live_context(&db), ("新摘要".into(), 1900, 2000, 1));
    }

    #[test]
    fn rebuild_run_identity_and_defaults_are_fixed_and_invalid_ids_do_not_create_a_run() {
        let mut db = db();
        source(&db, 1, 7, "原文", now());
        assert!(start(&mut db, "invalid/id", 1, &AiSettings::default()).is_err());
        start(&mut db, "rebuild-test", 1, &AiSettings::default()).unwrap();
        assert!(start(&mut db, "rebuild-test", 2, &AiSettings::default()).is_err());
        let changed = AiSettings {
            memory_write: false,
            ..AiSettings::default()
        };
        assert!(start(&mut db, "rebuild-test", 1, &changed).is_err());
        assert!(prepare(&db, "rebuild-test", &changed, 24).is_err());
        assert!(prepare(&db, "rebuild-test", &AiSettings::default(), 0).is_err());
    }

    #[test]
    fn rebuild_newly_eligible_rows_cannot_be_consumed_behind_a_frozen_batch() {
        let mut db = db();
        source(&db, 1, 7, "原先不可学习", now());
        source(&db, 2, 7, "原文", now());
        db.execute("UPDATE messages SET learnable=0 WHERE id=1", [])
            .unwrap();
        let batch = prepared(&mut db);
        assert_eq!(batch.sources.len(), 1);
        db.execute("UPDATE messages SET learnable=1 WHERE id=1", [])
            .unwrap();
        assert!(commit(&mut db, &batch, update(vec![]), &AiSettings::default(), 256).is_err());
        assert_eq!(status(&db, "rebuild-test").unwrap().batches, 0);
    }

    #[test]
    fn rebuild_expired_candidate_does_not_suppress_a_fresh_occurrence() {
        let mut db = db();
        source(&db, 1, 7, "今天散步了", now() - 100 * 86_400);
        source(&db, 2, 7, "今天散步了", now() - 86_400);
        let batch = prepared(&mut db);
        let stats = commit_update(
            &mut db,
            &batch,
            vec![
                change(&batch.sources[0], "散步", Some(7)),
                change(&batch.sources[1], "散步", Some(7)),
            ],
        );
        assert_eq!((stats.inserted, stats.expired, stats.duplicates), (1, 1, 0));
        let evidence: String = db
            .query_row("SELECT evidence FROM ai_memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<Evidence>>(&evidence).unwrap()[0].source_key,
            batch.sources[1].key
        );
    }

    #[test]
    fn rebuild_chat_observations_require_two_sources_and_use_the_latest_evidence_time() {
        let mut db = db();
        source(&db, 1, 7, "我喜欢用简短的句子聊天", now() - 2 * 86_400);
        source(&db, 2, 7, "聊天时我总是尽量少写几个字", now() - 86_400);
        let batch = prepared(&mut db);
        let mut observation = change(&batch.sources[0], "聊天观察：表达通常简短", Some(7));
        let MemoryChange::Upsert { kind, .. } = &mut observation;
        *kind = MemoryKind::Preference;
        assert!(
            commit(
                &mut db,
                &batch,
                update(vec![observation.clone()]),
                &AiSettings::default(),
                256
            )
            .is_err()
        );
        let MemoryChange::Upsert { evidence, .. } = &mut observation;
        evidence.push(Evidence {
            source_key: batch.sources[1].key.clone(),
            quote: batch.sources[1].text.clone(),
        });
        commit_update(&mut db, &batch, vec![observation]);
        let expiry: i64 = db
            .query_row("SELECT expires_at FROM ai_memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            expiry,
            batch.sources[1].received_at.timestamp() + 7 * 86_400
        );
    }

    #[test]
    fn rebuild_preflight_is_read_only_and_rejects_every_outbound_snapshot_change() {
        for alteration in [
            None,
            Some("UPDATE ai_conversations SET epoch=epoch+1"),
            Some("INSERT INTO ai_revoked_sources VALUES('1:group:20:1',1)"),
            Some("UPDATE ai_conversations SET settings='{\"memory_write\":false}'"),
            Some("UPDATE ai_conversations SET settings='{\"paused\":true}'"),
            Some("INSERT INTO scope_access VALUES('1:group:20',0,1)"),
            Some("UPDATE ai_scope_modes SET revision=revision+1"),
            Some("UPDATE ai_conversations SET memory_revision=memory_revision+1"),
            Some("UPDATE ai_memory_rebuild_jobs SET version=version+1"),
            Some("UPDATE messages SET content='修改后的原文' WHERE id=1"),
            Some("UPDATE ai_memories SET text='修改后的既有记忆'"),
            Some("UPDATE ai_memory_rebuild_runs SET defaults='{}'"),
        ] {
            let mut db = db();
            source(&db, 1, 7, "原文", now());
            write_memory(
                &db,
                CONVERSATION,
                None,
                7,
                MemoryKind::Fact,
                "既有事实",
                &[],
                None,
            )
            .unwrap();
            let batch = prepared(&mut db);
            if let Some(sql) = alteration {
                db.execute(sql, []).unwrap();
            }
            let context = live_context(&db);
            let memories = existing_memories(&db, CONVERSATION, 7).unwrap();
            let before = serde_json::to_value(status(&db, "rebuild-test").unwrap()).unwrap();
            db.execute_batch("PRAGMA query_only=ON").unwrap();
            let tx = db.transaction().unwrap();
            let result = validate_snapshot(&tx, &batch, &AiSettings::default());
            assert_eq!(
                result.is_ok(),
                alteration.is_none(),
                "{alteration:?}: {result:?}"
            );
            tx.commit().unwrap();
            assert_eq!(live_context(&db), context);
            assert_eq!(existing_memories(&db, CONVERSATION, 7).unwrap(), memories);
            assert_eq!(
                serde_json::to_value(status(&db, "rebuild-test").unwrap()).unwrap(),
                before
            );
        }
    }
}
