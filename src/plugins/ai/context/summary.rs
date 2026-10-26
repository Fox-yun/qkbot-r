//! Topic summaries remain derived conversation data, with source-bound attribution.
use super::super::memory::MemoryBatch;
use anyhow::{Result, ensure};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub topics: Vec<Topic>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Topic {
    pub title: String,
    pub participants: Vec<i64>,
    pub summary: String,
    pub decisions: Vec<String>,
    pub open_questions: Vec<String>,
    pub key_details: Vec<String>,
    pub sources: Vec<String>,
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(text) => Ok(text),
        serde_json::Value::Object(_) => {
            serde_json::to_string(&value).map_err(serde::de::Error::custom)
        }
        _ => Err(serde::de::Error::custom(
            "summary must be text or a topic document",
        )),
    }
}

pub fn parse(text: &str) -> Result<Option<Document>> {
    if !text.trim_start().starts_with('{') {
        return Ok(None);
    }
    let value: Document = serde_json::from_str(text)?;
    ensure!(
        !value.topics.is_empty() && value.topics.len() <= 8,
        "invalid topic count"
    );
    let mut titles = BTreeSet::new();
    for topic in &value.topics {
        ensure!(
            !topic.title.trim().is_empty()
                && topic.title.chars().count() <= 120
                && titles.insert(&topic.title),
            "invalid topic title"
        );
        ensure!(
            !topic.participants.is_empty()
                && topic.participants.len() <= 16
                && topic.participants.iter().all(|id| *id > 0),
            "invalid topic participants"
        );
        ensure!(
            topic.summary.chars().count() <= 2500,
            "topic summary too long"
        );
        for entries in [&topic.decisions, &topic.open_questions, &topic.key_details] {
            ensure!(
                entries.len() <= 12
                    && entries
                        .iter()
                        .all(|s| !s.trim().is_empty() && s.chars().count() <= 500),
                "invalid topic details"
            );
        }
        ensure!(
            !topic.sources.is_empty()
                && topic.sources.len() <= 32
                && topic
                    .sources
                    .iter()
                    .all(|s| !s.is_empty() && s.len() <= 1024),
            "invalid topic sources"
        );
    }
    Ok(Some(value))
}

pub fn validate(text: &str) -> Result<()> {
    let limit = if parse(text)?.is_some() { 6000 } else { 1500 };
    ensure!(
        text.chars().count() <= limit,
        "summary character limit exceeded"
    );
    Ok(())
}

pub fn repair_input(
    text: &str,
    budget: usize,
    input_budget: usize,
) -> Result<super::super::memory::BudgetedContext> {
    super::super::memory::build_context(
        super::super::memory::ContextInput {
            system: format!(
                "仅返回JSON对象含summary及空changes。压缩给定话题摘要，来源和人物只能取原有集合；保留open_questions和key_details的原文，不能新增事实。summary双JSON转义UTF-8≤{budget}字节。{PROMPT}"
            ),
            current: text.to_owned(),
            history: vec![],
            memories: vec![],
            summary: String::new(),
            tool_schema: serde_json::Value::Null,
            tool_protocol: vec![],
        },
        input_budget,
    )
}

pub fn validate_repair(original: &str, repaired: &str) -> Result<()> {
    let Some(before) = parse(original)? else {
        return Ok(());
    };
    let after =
        parse(repaired)?.ok_or_else(|| anyhow::anyhow!("topic structure lost during repair"))?;
    let mut evidence = BTreeMap::<String, BTreeSet<i64>>::new();
    for topic in &before.topics {
        for source in &topic.sources {
            evidence
                .entry(source.clone())
                .or_default()
                .extend(&topic.participants);
        }
    }
    let questions: BTreeSet<_> = after
        .topics
        .iter()
        .flat_map(|t| &t.open_questions)
        .collect();
    let details: BTreeSet<_> = after.topics.iter().flat_map(|t| &t.key_details).collect();
    validate_attribution(&after, &evidence)?;
    ensure!(
        before
            .topics
            .iter()
            .all(|t| t.open_questions.iter().all(|s| questions.contains(s))
                && t.key_details.iter().all(|s| details.contains(s))),
        "repair lost unresolved items or key details"
    );
    Ok(())
}

pub fn validate_sources(db: &rusqlite::Connection, batch: &MemoryBatch, text: &str) -> Result<()> {
    let Some(document) = parse(text)? else {
        return Ok(());
    };
    let mut evidence: BTreeMap<String, BTreeSet<i64>> = batch
        .sources
        .iter()
        .filter(|s| s.summary_eligible && s.context_visible)
        .map(|s| (s.key.clone(), BTreeSet::from([s.subject_id])))
        .collect();
    if !batch.previous_summary.is_empty() {
        // Only already validated previous-summary dependencies may survive a merge.
        let prior = super::summary_guard(db, &batch.guard)?;
        let previous_keys = prior.source_keys();
        for key in &previous_keys {
            use rusqlite::OptionalExtension;
            if let Some(id) = db
                .query_row("SELECT sender_id FROM messages WHERE key=?1", [key], |r| {
                    r.get::<_, i64>(0)
                })
                .optional()?
            {
                evidence.entry(key.clone()).or_default().insert(id);
            }
            evidence.entry(key.clone()).or_default();
        }
        if let Some(previous) = parse(&batch.previous_summary)? {
            for topic in previous.topics {
                for key in topic.sources {
                    if let Some(actors) = evidence.get_mut(&key) {
                        actors.extend(&topic.participants);
                    }
                }
            }
        }
    }
    validate_attribution(&document, &evidence)
}

fn validate_attribution(
    document: &Document,
    evidence: &BTreeMap<String, BTreeSet<i64>>,
) -> Result<()> {
    for topic in &document.topics {
        ensure!(
            topic.sources.iter().all(|key| evidence.contains_key(key)),
            "topic source outside summary evidence"
        );
        let actors: BTreeSet<_> = topic
            .sources
            .iter()
            .flat_map(|key| &evidence[key])
            .collect();
        ensure!(
            topic.participants.iter().all(|id| actors.contains(id)),
            "topic participant outside summary evidence"
        );
    }
    Ok(())
}

pub const PROMPT: &str = r#"summary_enabled=true时summary为话题对象{"topics":[{"title":"话题","participants":[发言者ID],"summary":"讨论经过，区分本人自述、转述与机器人建议","decisions":["已确定结论"],"open_questions":["未解决问题或待办"],"key_details":["原样保留关键名称、数字、约定和必要短原话"],"sources":["来源key"]}]}；最多8个话题。合并同一话题，保留尚未解决事项及其主体，不混合人物/群聊。来源只用原摘要已有来源或本批summary_eligible且context_visible原文key；participants只用真实发言者ID。禁止把allowed_memories或学习专用消息写进摘要。压缩已结束细节，保留关键原文；无需填满预算。"#;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn topic() -> serde_json::Value {
        json!({"topics":[{"title":"搬家","participants":[7],"summary":"讨论搬家安排",
            "decisions":["周五确认"],"open_questions":["费用待确认"],
            "key_details":["预算8192"],"sources":["source-7"]}]})
    }

    #[test]
    fn compression_preserves_open_items_and_source_bound_people() {
        let original = topic();
        let mut compressed = original.clone();
        compressed["topics"][0]["summary"] = json!("搬家");
        validate_repair(&original.to_string(), &compressed.to_string()).unwrap();
        compressed["topics"][0]["open_questions"] = json!([]);
        assert!(validate_repair(&original.to_string(), &compressed.to_string()).is_err());
        let document = parse(&original.to_string()).unwrap().unwrap();
        let evidence = BTreeMap::from([
            ("source-7".into(), BTreeSet::from([8])),
            ("source-other".into(), BTreeSet::from([7])),
        ]);
        assert!(
            validate_attribution(&document, &evidence).is_err(),
            "an unrelated source cannot validate this topic's speaker"
        );
        assert!(validate_repair(&original.to_string(), "搬家").is_err());
    }

    #[test]
    fn topic_document_has_bounded_schema_and_legacy_text_remains_valid() {
        validate("原有摘要").unwrap();
        let mut value = topic();
        value["topics"][0]["summary"] = json!("细节".repeat(800));
        validate(&value.to_string()).unwrap();
        value["topics"][0]["participants"] = json!([0]);
        assert!(validate(&value.to_string()).is_err());
        assert!(validate("{\"unknown\":1}").is_err());
    }
}
