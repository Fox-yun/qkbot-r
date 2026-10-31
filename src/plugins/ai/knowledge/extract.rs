//! Shared episodes and the bot's own statements, proposed by the existing
//! background memory extraction and checked here one candidate at a time.
//!
//! The bot's own confirmed replies are evidence of what the bot said. They
//! never become evidence of a person's facts: an episode about a person still
//! needs that person's own words in the same frozen batch.
use super::{Kind, NewItem, Source, Subject, insert, settings};
use crate::plugins::ai::memory::{Evidence, MemoryBatch, MessageSource, source_by_key};
use crate::types::now;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Instruction size reserved in the memory extraction input when enabled.
pub(crate) const PROMPT_RESERVE: usize = 1400;
/// Message space a batch keeps when episodes and statements are offered. With
/// the other reserves this needs a memory input budget of at least 9400.
pub(crate) const MIN_SOURCE_SPACE: usize = 3000;
pub(crate) const MAX_CHANGES: usize = 6;
/// Items per kind, subject and scope; new candidates beyond this are deferred.
const CAPACITY: i64 = 64;

pub(crate) const PROMPT: &str = r#"可选knowledge_changes数组(≤6项)，仅提供knowledge对象时可提议：{"kind":"episode|self","subject":"user|scope|bot","subject_id":数字,"text":"第三人称陈述","evidence":[{"source_key":"原文key","quote":"连续原文"}],"ttl_days":null,"importance":3}。episode=机器人与真人之间以后值得自然提起的共同经历：机器人答应过的事、一起定下的约定或计划、形成的玩笑或梗、一起完成的事；证据须含knowledge.bot_source_keys中的机器人回复和真人本人原文；subject=user时subject_id为该真人QQ；subject=scope仅用于本群多人共同的梗，subject_id=0且须有至少两位真人的原文。self=机器人自己说过、以后应保持一致的观点、喜好或自我描述，subject=bot，subject_id=knowledge.bot_id，证据只能是knowledge.bot_source_keys中的机器人原文。不记录事实性答案、工具结果、常识解释、客套话和真人的个人事实(个人事实仍写changes)；不推断隐私；text≤120字(episode≤160)；importance 1..5；ttl_days null或1..365；无则[]。"#;

#[derive(Clone, Debug, Serialize)]
pub struct BatchPolicy {
    pub bot_id: i64,
    pub character_id: String,
    pub kinds: Vec<&'static str>,
    pub bot_source_keys: Vec<String>,
}

/// Offer extraction only when the batch has confirmed bot replies, since both
/// kinds need at least one.
pub(crate) fn batch_policy(
    db: &Connection,
    sources: &[MessageSource],
    write: bool,
) -> Result<Option<BatchPolicy>> {
    let Some((config, character)) = settings(db)? else {
        return Ok(None);
    };
    let mut kinds = Vec::new();
    if config.enabled && config.episodes {
        kinds.push("episode");
    }
    if config.enabled && config.self_memory {
        kinds.push("self");
    }
    if !write || kinds.is_empty() {
        return Ok(None);
    }
    let mut keys = Vec::new();
    let mut bot = None;
    for source in sources
        .iter()
        .filter(|s| s.role == "assistant" && s.context_visible)
    {
        let valid: Option<i64> = db
            .query_row(
                "SELECT sender_id FROM messages WHERE key=?1 AND role='assistant' AND origin_plugin='ai' AND revoked=0",
                [&source.key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(sender) = valid {
            bot = Some(sender);
            keys.push(source.key.clone());
        }
    }
    let Some(bot_id) = bot.filter(|_| !keys.is_empty()) else {
        return Ok(None);
    };
    Ok(Some(BatchPolicy {
        bot_id,
        character_id: character,
        kinds,
        bot_source_keys: keys,
    }))
}

/// Remove references to sources trimmed from the batch.
pub(crate) fn retain_sources(batch: &mut MemoryBatch) {
    if !batch.memory_write {
        batch.knowledge = None;
    }
    if let Some(policy) = &mut batch.knowledge {
        policy
            .bot_source_keys
            .retain(|key| batch.sources.iter().any(|s| s.key == *key));
        if policy.bot_source_keys.is_empty() {
            batch.knowledge = None;
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub kind: Kind,
    pub subject: Subject,
    pub subject_id: i64,
    pub text: String,
    pub evidence: Vec<Evidence>,
    pub ttl_days: Option<u16>,
    #[serde(default)]
    pub importance: Option<u8>,
}

/// Check and store one candidate. Returns a fixed rejection reason, or None
/// when stored or already present. Storage errors are returned as errors.
pub(crate) fn apply_candidate(
    db: &Connection,
    batch: &MemoryBatch,
    candidate: &Candidate,
) -> Result<Option<&'static str>> {
    let Some(policy) = &batch.knowledge else {
        return Ok(Some("knowledge_disabled"));
    };
    let Some((config, character)) = settings(db)? else {
        return Ok(Some("knowledge_disabled"));
    };
    if character != policy.character_id || !config.enabled {
        return Ok(Some("knowledge_policy_changed"));
    }
    if !matches!(candidate.kind, Kind::Episode | Kind::Statement)
        || !policy.kinds.contains(&candidate.kind.as_str())
    {
        return Ok(Some("knowledge_kind"));
    }
    if super::validate_text(candidate.kind, &candidate.text).is_err()
        || candidate.ttl_days.is_some_and(|d| d == 0 || d > 365)
        || candidate.importance.is_some_and(|i| !(1..=5).contains(&i))
    {
        return Ok(Some("knowledge_value"));
    }
    if candidate.evidence.is_empty() || candidate.evidence.len() > 4 {
        return Ok(Some("knowledge_evidence"));
    }
    let scope_text: String = db.query_row(
        "SELECT scope FROM ai_conversations WHERE id=?1",
        [&batch.guard.conversation_id],
        |r| r.get(0),
    )?;
    let scope = super::continuity::parse_scope(&scope_text)?;
    let mut frozen: Vec<&MessageSource> = Vec::new();
    for evidence in &candidate.evidence {
        let Some(source) = batch.sources.iter().find(|s| s.key == evidence.source_key) else {
            return Ok(Some("knowledge_evidence"));
        };
        let quote = evidence.quote.trim();
        if quote.is_empty() || quote.chars().count() > 200 || !source.text.contains(quote) {
            return Ok(Some("knowledge_evidence"));
        }
        // The source must still be the same, unrevoked row in this epoch.
        let current = source_by_key(db, &batch.guard, &source.key)?;
        if !current.is_some_and(|c| {
            c.text == source.text && c.role == source.role && c.subject_id == source.subject_id
        }) {
            return Ok(Some("knowledge_evidence"));
        }
        frozen.push(source);
    }
    let is_bot =
        |s: &MessageSource| s.role == "assistant" && policy.bot_source_keys.contains(&s.key);
    let is_human = |s: &MessageSource| {
        s.role == "user"
            && s.memory_eligible
            && s.learnable
            && s.subject_id > 0
            && s.subject_id != policy.bot_id
    };
    if frozen.iter().any(|s| !is_bot(s) && !is_human(s)) {
        return Ok(Some("knowledge_evidence"));
    }
    let humans: BTreeSet<i64> = frozen
        .iter()
        .filter(|s| is_human(s))
        .map(|s| s.subject_id)
        .collect();
    let has_bot = frozen.iter().any(|s| is_bot(s));
    let valid_identity = match (candidate.kind, candidate.subject) {
        (Kind::Episode, Subject::User) => {
            has_bot
                && humans.contains(&candidate.subject_id)
                && candidate.subject_id != policy.bot_id
                && (scope.scene != crate::types::Scene::Friend
                    || scope.peer_id == candidate.subject_id)
        }
        (Kind::Episode, Subject::Scope) => {
            has_bot
                && candidate.subject_id == 0
                && humans.len() >= 2
                && scope.scene == crate::types::Scene::Group
        }
        (Kind::Statement, Subject::Bot) => {
            candidate.subject_id == policy.bot_id && humans.is_empty() && has_bot
        }
        _ => false,
    };
    if !valid_identity || scope.bot_id != policy.bot_id {
        return Ok(Some("knowledge_identity"));
    }
    let character_id = if candidate.kind == Kind::Statement {
        policy.character_id.as_str()
    } else {
        ""
    };
    let text = candidate.text.trim();
    let duplicate: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM ai_knowledge WHERE bot_id=?1 AND kind=?2 AND subject=?3 AND subject_id=?4 AND scope=?5 AND character_id=?6 AND text=?7)",
        params![policy.bot_id, candidate.kind.as_str(), candidate.subject.as_str(), candidate.subject_id, scope_text, character_id, text],
        |r| r.get(0),
    )?;
    if duplicate {
        return Ok(None);
    }
    let count: i64 = db.query_row(
        "SELECT count(*) FROM ai_knowledge WHERE bot_id=?1 AND kind=?2 AND subject=?3 AND subject_id=?4 AND scope=?5",
        params![policy.bot_id, candidate.kind.as_str(), candidate.subject.as_str(), candidate.subject_id, scope_text],
        |r| r.get(0),
    )?;
    if count >= CAPACITY {
        return Ok(Some("knowledge_capacity"));
    }
    let occurred = frozen
        .iter()
        .map(|s| s.received_at.timestamp())
        .max()
        .unwrap_or_else(now);
    let sources: Vec<Source> = frozen
        .iter()
        .zip(&candidate.evidence)
        .map(|(s, e)| {
            if s.role == "assistant" {
                Source::reply(&s.key, e.quote.trim(), &s.text)
            } else {
                Source::evidence(&s.key, e.quote.trim(), &s.text)
            }
        })
        .collect();
    insert(
        db,
        &NewItem {
            bot_id: policy.bot_id,
            kind: candidate.kind,
            subject: candidate.subject,
            subject_id: candidate.subject_id,
            scope: &scope_text,
            character_id,
            domain: "",
            text,
            origin: "auto",
            confidence: 0.7,
            importance: i64::from(candidate.importance.unwrap_or(3)),
            status: "active",
            expires_at: candidate.ttl_days.map(|d| occurred + i64::from(d) * 86400),
        },
        &sources,
    )?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::ai::memory::{
        MEMORY_PROMPT_RESERVE, RELATIONSHIP_PROMPT_RESERVE, TOPIC_PROMPT_RESERVE,
    };

    #[test]
    fn default_extraction_budget_keeps_its_message_space() {
        let needed = MEMORY_PROMPT_RESERVE
            + RELATIONSHIP_PROMPT_RESERVE
            + TOPIC_PROMPT_RESERVE
            + PROMPT_RESERVE
            + MIN_SOURCE_SPACE;
        assert_eq!(needed, 9400);
        assert!(crate::config::AiConfig::default().memory_input_budget < needed);
        assert!(serde_json::to_string(PROMPT).unwrap().len() <= PROMPT_RESERVE);
    }
}
