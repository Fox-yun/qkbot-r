//! Durable, bounded retry receipts for automatic personal-knowledge extraction.
//! Receipts contain classifications and hashes, never rejected claims or quotes.
use super::*;
use sha2::{Digest, Sha256};

pub(super) const MIGRATION: &str = "CREATE TABLE ai_memory_candidate_results(
 conversation_id TEXT NOT NULL,epoch INTEGER NOT NULL,fingerprint TEXT NOT NULL,
 through_position INTEGER NOT NULL,status TEXT NOT NULL,reason TEXT NOT NULL,
 memory_id INTEGER,updated_at INTEGER NOT NULL,
 PRIMARY KEY(conversation_id,epoch,fingerprint));
CREATE TABLE ai_memory_batch_results(
 conversation_id TEXT NOT NULL,epoch INTEGER NOT NULL,start_position INTEGER NOT NULL,
 through_position INTEGER NOT NULL,attempts INTEGER NOT NULL,status TEXT NOT NULL,
 summary_reason TEXT NOT NULL,rejected_count INTEGER NOT NULL,updated_at INTEGER NOT NULL,
 PRIMARY KEY(conversation_id,epoch,start_position,through_position));";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningUpdate {
    #[serde(deserialize_with = "super::super::context::summary::deserialize")]
    pub summary: String,
    #[serde(default)]
    changes: Vec<Box<serde_json::value::RawValue>>,
    #[serde(default)]
    relationship_changes: Vec<Box<serde_json::value::RawValue>>,
}

impl From<BatchUpdate> for LearningUpdate {
    fn from(update: BatchUpdate) -> Self {
        Self {
            relationship_changes: vec![],
            summary: update.summary,
            changes: update
                .changes
                .into_iter()
                .map(|c| serde_json::value::to_raw_value(&c).expect("serializable memory change"))
                .collect(),
        }
    }
}

pub(crate) fn parse(content: &str) -> Result<LearningUpdate> {
    let update: LearningUpdate = parse_response(content)?;
    ensure!(
        update.changes.len() + update.relationship_changes.len() <= MAX_CHANGES,
        "too many memory changes"
    );
    Ok(update)
}

pub(super) fn pending_through(
    db: &Connection,
    batch: &ContextGuard,
    start: i64,
) -> Result<Option<i64>> {
    let installed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='ai_memory_batch_results')",
        [],
        |r| r.get(0),
    )?;
    if !installed {
        return Ok(None);
    }
    Ok(db.query_row("SELECT MIN(through_position) FROM ai_memory_batch_results WHERE conversation_id=?1 AND epoch=?2 AND start_position=?3 AND status='retry'",
        params![batch.conversation_id,batch.epoch,start], |r| r.get(0))?)
}

pub(crate) fn retry_ready(db: &Connection, guard: &ContextGuard) -> Result<bool> {
    let installed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='ai_memory_batch_results')",
        [],
        |r| r.get(0),
    )?;
    if !installed {
        return Ok(true);
    }
    Ok(!db.query_row(
        "SELECT EXISTS(SELECT 1 FROM ai_memory_batch_results WHERE conversation_id=?1 AND epoch=?2
        AND (status='retry' OR summary_reason!='') AND updated_at>?3)",
        params![guard.conversation_id, guard.epoch, now() - 60],
        |r| r.get::<_, bool>(0),
    )?)
}

pub(super) fn commit(
    db: &mut Connection,
    batch: &MemoryBatch,
    update: LearningUpdate,
    maximum: usize,
) -> Result<()> {
    ensure!(
        update.changes.len() + update.relationship_changes.len() <= MAX_CHANGES,
        "too many memory changes"
    );
    ensure!(
        batch.memory_write || update.changes.is_empty(),
        "automatic memory writes are disabled"
    );
    ensure!(
        batch.summary_enabled || update.summary.is_empty(),
        "summary generation is disabled for this batch"
    );
    let tx = db.transaction()?;
    let domain = validate_batch_snapshot(&tx, batch)?;
    purge_expired(&tx)?;
    let summary_error = validate_summary(&update.summary, batch.summary_budget)
        .and_then(|()| {
            super::super::context::summary::validate_sources(&tx, batch, &update.summary)
        })
        .err();
    let summary_reason = summary_error
        .as_ref()
        .map(memory_commit_failure_reason)
        .unwrap_or("");
    let summary_ok = batch.summary_enabled && summary_error.is_none();
    let parsed: Vec<_> = update
        .changes
        .iter()
        .map(|raw| serde_json::from_str::<MemoryChange>(raw.get()))
        .collect();
    let mut inserted = 0;
    let mut updated = 0;
    let mut rejected = 0;
    let mut duplicates = 0;
    for (raw, candidate) in update.changes.iter().zip(&parsed) {
        // Canonical valid values make whitespace changes idempotent. Invalid
        // values are hashed without storing or logging their contents.
        let canonical = candidate
            .as_ref()
            .ok()
            .map(serde_json::to_string)
            .transpose()?;
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(canonical.as_deref().unwrap_or(raw.get()).as_bytes())
        );
        let previous: Option<(String,Option<i64>)> = tx.query_row(
            "SELECT status,memory_id FROM ai_memory_candidate_results WHERE conversation_id=?1 AND epoch=?2 AND fingerprint=?3",
            params![batch.guard.conversation_id,batch.guard.epoch,fingerprint], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        if previous
            .as_ref()
            .is_some_and(|(status, _)| status == "applied")
        {
            duplicates += 1;
            continue;
        }
        let validation = match candidate {
            Err(_) => Err(anyhow::anyhow!("candidate schema is invalid")),
            Ok(change) => {
                let MemoryChange::Upsert { id, .. } = change;
                if id.is_some() && parsed.iter().filter(|c| matches!(c, Ok(MemoryChange::Upsert { id: other, .. }) if other == id)).count() > 1 {
                    Err(anyhow::anyhow!("duplicate memory mutation"))
                } else { validate_change(&tx, batch, &domain, change) }
            }
        };
        let (status, reason, memory_id) = if let Err(error) = validation {
            let reason = if candidate.is_err() {
                "candidate_schema"
            } else {
                memory_commit_failure_reason(&error)
            };
            // Unknown errors can be storage failures, not bad candidates. Never
            // swallow them or commit a partially executed SQL operation.
            if reason == "storage_or_unknown" {
                return Err(error);
            }
            rejected += 1;
            ("rejected", reason, None)
        } else {
            let MemoryChange::Upsert {
                id,
                subject_id,
                kind,
                text,
                evidence,
                ttl_days,
            } = candidate.as_ref().unwrap();
            if id.is_none()
                && scope::count(&tx, &batch.guard.conversation_id, *subject_id)? >= maximum
            {
                rejected += 1;
                ("deferred", "capacity", None)
            } else {
                let saved = write_memory(
                    &tx,
                    &batch.guard.conversation_id,
                    *id,
                    *subject_id,
                    *kind,
                    text,
                    evidence,
                    *ttl_days,
                )?;
                if id.is_some() {
                    updated += 1;
                } else {
                    inserted += 1;
                }
                ("applied", "", Some(saved))
            }
        };
        tx.execute("INSERT INTO ai_memory_candidate_results(conversation_id,epoch,fingerprint,through_position,status,reason,memory_id,updated_at)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(conversation_id,epoch,fingerprint) DO UPDATE SET
            through_position=excluded.through_position,status=excluded.status,reason=excluded.reason,memory_id=excluded.memory_id,updated_at=excluded.updated_at",
            params![batch.guard.conversation_id,batch.guard.epoch,fingerprint,batch.through_position,status,reason,memory_id,now()])?;
    }
    for raw in &update.relationship_changes {
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(format!("relationship:{}", raw.get()).as_bytes())
        );
        let reason = match serde_json::from_str::<super::super::personal::Candidate>(raw.get()) {
            Ok(candidate) => super::super::personal::apply_candidate(&tx, batch, &candidate)?,
            Err(_) => Some("relationship_schema"),
        };
        if reason.is_some() {
            rejected += 1;
        }
        tx.execute("INSERT INTO ai_memory_candidate_results(conversation_id,epoch,fingerprint,through_position,status,reason,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(conversation_id,epoch,fingerprint) DO UPDATE SET through_position=excluded.through_position,status=excluded.status,reason=excluded.reason,updated_at=excluded.updated_at",
            params![batch.guard.conversation_id,batch.guard.epoch,fingerprint,batch.through_position,if reason.is_some(){"rejected"}else{"applied"},reason.unwrap_or(""),now()])?;
    }
    let attempts: i64 = tx.query_row("SELECT COALESCE((SELECT attempts FROM ai_memory_batch_results WHERE conversation_id=?1 AND epoch=?2 AND start_position=?3 AND through_position=?4),0)+1",
        params![batch.guard.conversation_id,batch.guard.epoch,batch.previous_memory_processed_until,batch.through_position], |r| r.get(0))?;
    // Retry from unchanged original evidence, at most three model passes for a
    // prefix. An exhausted prefix stays visible as needs_review, never as success.
    let status = if rejected == 0 {
        "complete"
    } else if attempts >= 3 {
        "needs_review"
    } else {
        "retry"
    };
    tx.execute("INSERT INTO ai_memory_batch_results(conversation_id,epoch,start_position,through_position,attempts,status,summary_reason,rejected_count,updated_at)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(conversation_id,epoch,start_position,through_position) DO UPDATE SET
        attempts=excluded.attempts,status=excluded.status,summary_reason=excluded.summary_reason,rejected_count=excluded.rejected_count,updated_at=excluded.updated_at",
        params![batch.guard.conversation_id,batch.guard.epoch,batch.previous_memory_processed_until,batch.through_position,attempts,status,summary_reason,rejected,now()])?;
    if summary_ok {
        super::super::context::record_summary(&tx, batch)?;
        super::super::context::compaction::completed(&tx, &batch.guard, batch.through_position)?;
    }
    tx.execute("UPDATE ai_conversations SET summary=CASE WHEN ?1 THEN ?2 ELSE summary END,
        compacted_until=CASE WHEN ?1 THEN ?3 ELSE compacted_until END,
        memory_processed_until=CASE WHEN ?4 THEN ?3 ELSE memory_processed_until END WHERE id=?5 AND epoch=?6",
        params![summary_ok,update.summary,batch.through_position,batch.memory_write && status != "retry",batch.guard.conversation_id,batch.guard.epoch])?;
    if summary_ok {
        tx.execute("UPDATE ai_memory_batch_results SET summary_reason='' WHERE conversation_id=?1 AND epoch=?2 AND through_position<=?3",
            params![batch.guard.conversation_id,batch.guard.epoch,batch.through_position])?;
    }
    // Keep active retry receipts (for idempotence), plus a bounded recent audit.
    tx.execute("DELETE FROM ai_memory_candidate_results WHERE conversation_id=?1 AND
        (epoch!=?2 OR (through_position<=(SELECT memory_processed_until FROM ai_conversations WHERE id=?1)
        AND rowid NOT IN(SELECT rowid FROM ai_memory_candidate_results WHERE conversation_id=?1 AND epoch=?2 ORDER BY updated_at DESC,rowid DESC LIMIT 256)))",
        params![batch.guard.conversation_id,batch.guard.epoch])?;
    tx.execute("DELETE FROM ai_memory_batch_results WHERE conversation_id=?1 AND
        (epoch!=?2 OR (status!='retry' AND rowid NOT IN(SELECT rowid FROM ai_memory_batch_results WHERE conversation_id=?1 AND epoch=?2 ORDER BY updated_at DESC,rowid DESC LIMIT 128)))",
        params![batch.guard.conversation_id,batch.guard.epoch])?;
    tx.commit()?;
    tracing::info!(conversation_id=%batch.guard.conversation_id, epoch=batch.guard.epoch, through_position=batch.through_position,
        inserted, updated, rejected, duplicates, attempts, status, summary_reason, summary_committed=summary_ok,
        "automatic memory candidates processed");
    Ok(())
}

pub fn overview(db: &Connection, bot: i64) -> Result<Value> {
    let mut rows = db.prepare("SELECT b.conversation_id,b.epoch,b.start_position,b.through_position,b.attempts,b.status,b.summary_reason,b.rejected_count,b.updated_at
        FROM ai_memory_batch_results b JOIN ai_conversations c ON c.id=b.conversation_id AND c.epoch=b.epoch
        WHERE c.scope LIKE ?1 AND (b.status!='complete' OR b.summary_reason!='') ORDER BY b.updated_at DESC LIMIT 20")?
        .query_map([format!("{bot}:%")], |r| Ok(json!({"conversation_id":r.get::<_,String>(0)?,"epoch":r.get::<_,i64>(1)?,
            "start_position":r.get::<_,i64>(2)?,"through_position":r.get::<_,i64>(3)?,"attempts":r.get::<_,i64>(4)?,
            "status":r.get::<_,String>(5)?,"summary_reason":r.get::<_,String>(6)?,"rejected_count":r.get::<_,i64>(7)?,"updated_at":r.get::<_,i64>(8)?})))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for row in &mut rows {
        let reasons = db.prepare("SELECT DISTINCT reason FROM ai_memory_candidate_results WHERE conversation_id=?1 AND epoch=?2 AND through_position=?3 AND status!='applied' ORDER BY reason")?
            .query_map(params![row["conversation_id"].as_str(),row["epoch"].as_i64(),row["through_position"].as_i64()], |r| r.get::<_,String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        row["reasons"] = json!(reasons);
    }
    Ok(json!(rows))
}
