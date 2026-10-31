//! Use and reinforcement statistics for personal facts and unified knowledge.
//!
//! These live in their own table on purpose: any UPDATE of `ai_memories`
//! advances the whole shared memory domain's revision and clears its vector
//! cache, which would interrupt background consolidation on every chat turn.
use super::installed;
use crate::types::now;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Count material that was actually delivered in a confirmed reply.
pub(crate) fn record_recall(db: &Connection, memories: &[i64], knowledge: &[i64]) -> Result<()> {
    if !installed(db)? {
        return Ok(());
    }
    let time = now();
    let items = memories
        .iter()
        .map(|id| format!("memory:{id}"))
        .chain(knowledge.iter().map(|id| format!("knowledge:{id}")));
    for item in items {
        db.execute("INSERT INTO ai_knowledge_stats(item,recall_count,last_recalled_at) VALUES(?1,1,?2) ON CONFLICT(item) DO UPDATE SET recall_count=recall_count+1,last_recalled_at=excluded.last_recalled_at", params![item, time])?;
    }
    Ok(())
}

/// A person repeating or updating a fact reinforces it.
pub(crate) fn reinforce_memory(db: &Connection, id: i64) -> Result<()> {
    if !installed(db)? {
        return Ok(());
    }
    db.execute("INSERT INTO ai_knowledge_stats(item,reinforced_count,reinforced_at) VALUES(?1,1,?2) ON CONFLICT(item) DO UPDATE SET reinforced_count=reinforced_count+1,reinforced_at=excluded.reinforced_at", params![format!("memory:{id}"), now()])?;
    Ok(())
}

pub(crate) fn set_memory_importance(db: &Connection, id: i64, importance: u8) -> Result<()> {
    if !installed(db)? || !(1..=5).contains(&importance) {
        return Ok(());
    }
    db.execute("INSERT INTO ai_knowledge_stats(item,importance) VALUES(?1,?2) ON CONFLICT(item) DO UPDATE SET importance=excluded.importance", params![format!("memory:{id}"), importance])?;
    Ok(())
}

pub(crate) fn read(db: &Connection, item: &str) -> Result<Option<Value>> {
    Ok(db
        .query_row("SELECT importance,recall_count,last_recalled_at,reinforced_count,reinforced_at FROM ai_knowledge_stats WHERE item=?1", [item], |r| {
            Ok(json!({"importance":r.get::<_,Option<i64>>(0)?,"recall_count":r.get::<_,i64>(1)?,"last_recalled_at":r.get::<_,Option<i64>>(2)?,"reinforced_count":r.get::<_,i64>(3)?,"reinforced_at":r.get::<_,Option<i64>>(4)?}))
        })
        .optional()?)
}

/// importance, recall_count, last_recalled_at, reinforced_count, reinforced_at
type StatsRow = (Option<i64>, i64, Option<i64>, i64, Option<i64>);

fn enabled(db: &Connection) -> Result<bool> {
    Ok(super::settings(db)?.is_some_and(|(c, _)| c.enabled && c.importance_ranking))
}

/// Additive ranking bonus per memory. Memories without statistics get none,
/// so ranking is unchanged until importance or use has been recorded.
///
/// Scale: reciprocal-rank scores of adjacent candidates differ by roughly
/// 0.0003, so typical bonuses move a fact by about ten places near the top
/// and relevance still dominates.
pub(crate) fn memory_boosts(db: &Connection, ids: &[i64]) -> Result<HashMap<i64, f64>> {
    let mut boosts = HashMap::new();
    if ids.is_empty() || !installed(db)? || !enabled(db)? {
        return Ok(boosts);
    }
    let time = now();
    for id in ids {
        let row: Option<StatsRow> = db
            .query_row("SELECT importance,recall_count,last_recalled_at,reinforced_count,reinforced_at FROM ai_knowledge_stats WHERE item=?1", [format!("memory:{id}")], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .optional()?;
        let Some((importance, recalls, recalled_at, reinforced, reinforced_at)) = row else {
            continue;
        };
        boosts.insert(
            *id,
            boost(
                time,
                importance,
                recalls,
                recalled_at,
                reinforced,
                reinforced_at,
            ),
        );
    }
    Ok(boosts)
}

fn boost(
    time: i64,
    importance: Option<i64>,
    recalls: i64,
    recalled_at: Option<i64>,
    reinforced: i64,
    reinforced_at: Option<i64>,
) -> f64 {
    let importance = (importance.unwrap_or(3).clamp(1, 5) - 3) as f64 * 0.0008;
    let use_signal = 0.0006 * (1.0 + recalls.max(0) as f64).ln();
    let reinforcement = 0.0008 * (1.0 + reinforced.max(0) as f64).ln();
    // Recent use or repetition decays with a 30-day time constant.
    let last = recalled_at.max(reinforced_at);
    let recency = last.map_or(0.0, |at| {
        0.0008 * (-((time - at).max(0) as f64) / (30.0 * 86400.0)).exp()
    });
    importance + use_signal + reinforcement + recency
}

/// Reorder candidates by their reciprocal rank plus the statistics bonus. The
/// original order breaks ties, so without statistics nothing moves.
pub(crate) fn rerank<T>(db: &Connection, items: Vec<T>, id: impl Fn(&T) -> i64) -> Result<Vec<T>> {
    let ids: Vec<i64> = items.iter().map(&id).collect();
    let boosts = memory_boosts(db, &ids)?;
    if boosts.is_empty() {
        return Ok(items);
    }
    let mut scored: Vec<(f64, usize, T)> = items
        .into_iter()
        .enumerate()
        .map(|(rank, item)| {
            let bonus = boosts.get(&id(&item)).copied().unwrap_or(0.0);
            (1.0 / (60.0 + rank as f64 + 1.0) + bonus, rank, item)
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(scored.into_iter().map(|(_, _, item)| item).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statistics_bonus_is_bounded_and_prefers_important_reinforced_recent_facts() {
        let time = 10_000_000;
        let plain = boost(time, None, 0, None, 0, None);
        assert_eq!(plain, 0.0);
        let important = boost(time, Some(5), 0, None, 0, None);
        let trivial = boost(time, Some(1), 0, None, 0, None);
        assert!(important > plain && plain > trivial);
        let recent = boost(time, None, 3, Some(time), 0, None);
        let stale = boost(time, None, 3, Some(time - 120 * 86400), 0, None);
        assert!(recent > stale && stale > 0.0);
        let reinforced = boost(time, None, 0, None, 4, Some(time));
        assert!(reinforced > plain);
        // Even the strongest bonus stays small compared with reciprocal ranks.
        let maximum = boost(time, Some(5), 10_000, Some(time), 10_000, Some(time));
        assert!(maximum < 0.02);
    }
}
