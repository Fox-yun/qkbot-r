//! Persistent pressure observations and summary cooldowns; no message bodies.
use crate::{
    config::{AiSettings, CompactionConfig},
    store::Store,
    types::{ContextGuard, now},
};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

pub async fn init(store: &Store) -> Result<()> {
    store.migrate("ai_compaction", 1, "CREATE TABLE ai_context_compaction(
        conversation_id TEXT PRIMARY KEY,epoch INTEGER NOT NULL,input_bytes INTEGER NOT NULL DEFAULT 0,
        history_bytes INTEGER NOT NULL DEFAULT 0,input_budget INTEGER NOT NULL DEFAULT 0,
        observed_position INTEGER NOT NULL DEFAULT 0,observed_at INTEGER NOT NULL DEFAULT 0,
        last_attempt_at INTEGER NOT NULL DEFAULT 0,completed_at INTEGER NOT NULL DEFAULT 0,
        through_position INTEGER NOT NULL DEFAULT 0);").await
}

fn installed(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='ai_context_compaction')",
        [],
        |r| r.get(0),
    )?)
}

pub fn observe(
    db: &Connection,
    guard: &ContextGuard,
    position: i64,
    usage: &super::assembly::ContextUsage,
) -> Result<()> {
    crate::host::check_guard(db, guard)?;
    db.execute("INSERT INTO ai_context_compaction(conversation_id,epoch,input_bytes,history_bytes,input_budget,observed_position,observed_at)
        VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(conversation_id) DO UPDATE SET
        epoch=excluded.epoch,input_bytes=excluded.input_bytes,history_bytes=excluded.history_bytes,
        input_budget=excluded.input_budget,observed_position=excluded.observed_position,observed_at=excluded.observed_at,
        last_attempt_at=CASE WHEN epoch=excluded.epoch THEN last_attempt_at ELSE 0 END,
        completed_at=CASE WHEN epoch=excluded.epoch THEN completed_at ELSE 0 END,
        through_position=CASE WHEN epoch=excluded.epoch THEN through_position ELSE 0 END
        WHERE epoch!=excluded.epoch OR observed_position<=excluded.observed_position",
        params![guard.conversation_id,guard.epoch,usage.untrimmed_estimated_tokens as i64,
            usage.history_estimated_tokens as i64,usage.base_budget as i64,position,now()])?;
    Ok(())
}

fn observation(db: &Connection, id: &str, epoch: i64, budget: usize) -> Result<(usize, usize)> {
    if !installed(db)? {
        return Ok((0, 0));
    }
    Ok(db.query_row("SELECT input_bytes,history_bytes FROM ai_context_compaction p JOIN ai_conversations c ON c.id=p.conversation_id
        WHERE p.conversation_id=?1 AND p.epoch=?2 AND p.input_budget=?3 AND p.observed_at>?4
        AND p.observed_position>c.compacted_until",params![id,epoch,budget as i64,now()-1800],
        |r|Ok((r.get::<_,i64>(0)? as usize,r.get::<_,i64>(1)? as usize))).optional()?.unwrap_or((0,0)))
}

pub fn pressured(
    db: &Connection,
    id: &str,
    epoch: i64,
    settings: &AiSettings,
    config: &CompactionConfig,
    pending_bytes: usize,
) -> Result<bool> {
    let (actual, _) = observation(db, id, epoch, settings.input_budget)?;
    Ok(actual.max(pending_bytes).saturating_mul(100)
        >= settings.input_budget.saturating_mul(config.trigger_percent))
}

pub fn due(db: &Connection, id: &str, epoch: i64, config: &CompactionConfig) -> Result<bool> {
    if !installed(db)? {
        return Ok(true);
    }
    Ok(!db.query_row("SELECT EXISTS(SELECT 1 FROM ai_context_compaction WHERE conversation_id=?1 AND epoch=?2 AND last_attempt_at>?3)",
        params![id,epoch,now()-config.min_interval_seconds as i64],|r|r.get::<_,bool>(0))?)
}

pub fn attempt(db: &Connection, guard: &ContextGuard) -> Result<()> {
    crate::host::check_guard(db, guard)?;
    db.execute("INSERT INTO ai_context_compaction(conversation_id,epoch,last_attempt_at) VALUES(?1,?2,?3)
        ON CONFLICT(conversation_id) DO UPDATE SET epoch=excluded.epoch,last_attempt_at=excluded.last_attempt_at,
        input_bytes=CASE WHEN epoch=excluded.epoch THEN input_bytes ELSE 0 END,
        history_bytes=CASE WHEN epoch=excluded.epoch THEN history_bytes ELSE 0 END,
        observed_position=CASE WHEN epoch=excluded.epoch THEN observed_position ELSE 0 END,
        completed_at=CASE WHEN epoch=excluded.epoch THEN completed_at ELSE 0 END,
        through_position=CASE WHEN epoch=excluded.epoch THEN through_position ELSE 0 END",
        params![guard.conversation_id,guard.epoch,now()])?;
    Ok(())
}

pub fn completed(db: &Connection, guard: &ContextGuard, through: i64) -> Result<()> {
    if installed(db)? {
        db.execute("UPDATE ai_context_compaction SET completed_at=?3,through_position=?4,input_bytes=0,history_bytes=0,observed_position=0
            WHERE conversation_id=?1 AND epoch=?2",params![guard.conversation_id,guard.epoch,now(),through])?;
    }
    Ok(())
}

pub fn keep(
    db: &Connection,
    id: &str,
    epoch: i64,
    settings: &AiSettings,
    config: &CompactionConfig,
) -> Result<usize> {
    let maximum = settings.recent_messages.max(2);
    let minimum = config.min_recent_messages.min(maximum);
    let costs=db.prepare("SELECT length(CAST(content AS BLOB))+256 FROM messages WHERE conversation_id=?1 AND epoch=?2 AND revoked=0
        AND role IN('user','assistant') AND COALESCE(origin_plugin,'') NOT IN('control','memory_only')
        AND inbound_position>(SELECT compacted_until FROM ai_conversations WHERE id=?1)
        ORDER BY inbound_position DESC,id DESC LIMIT ?3")?.query_map(params![id,epoch,maximum as i64],|r|Ok(r.get::<_,i64>(0)? as usize))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let history = costs.iter().sum::<usize>();
    if !pressured(db, id, epoch, settings, config, history)? {
        return Ok(maximum);
    }
    let (actual, actual_history) = observation(db, id, epoch, settings.input_budget)?;
    let fixed = actual.saturating_sub(actual_history);
    let target = (settings.input_budget * config.target_percent / 100)
        .saturating_sub(fixed)
        .saturating_sub(config.summary_budget);
    let mut used = 0usize;
    let mut keep = 0;
    for cost in costs {
        if keep >= minimum && used.saturating_add(cost) > target {
            break;
        }
        used = used.saturating_add(cost);
        keep += 1;
    }
    Ok(keep.max(minimum).min(maximum))
}

pub fn overview(db: &Connection, bot: i64) -> Result<serde_json::Value> {
    let mut q=db.prepare("SELECT c.scope,p.input_bytes,p.input_budget,p.last_attempt_at,p.completed_at,p.through_position
        FROM ai_context_compaction p JOIN ai_conversations c ON c.id=p.conversation_id AND c.epoch=p.epoch
        WHERE c.scope LIKE ?1 ORDER BY max(p.observed_at,p.last_attempt_at) DESC LIMIT 20")?;
    let rows=q.query_map([format!("{bot}:%")],|r|Ok(serde_json::json!({"scope":r.get::<_,String>(0)?,
        "input_bytes":r.get::<_,i64>(1)?,"input_budget":r.get::<_,i64>(2)?,"last_attempt_at":r.get::<_,i64>(3)?,
        "completed_at":r.get::<_,i64>(4)?,"through_position":r.get::<_,i64>(5)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(serde_json::json!(rows))
}
