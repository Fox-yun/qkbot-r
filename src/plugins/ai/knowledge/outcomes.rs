//! What happened after each confirmed AI reply, derived only from stored
//! messages, quotes and mentions. No model request is made here.
//!
//! The correction and positive signals are keyword heuristics. They feed
//! statistics and the interaction-note job, which still needs real evidence
//! quotes and model judgement; they are never treated as facts about a person.
use crate::{store::Store, types::now};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const SCHEMA: &str = "CREATE TABLE ai_reply_outcomes(
 reply_key TEXT PRIMARY KEY,bot_id INTEGER NOT NULL,scope TEXT NOT NULL,conversation_id TEXT NOT NULL,
 request_key TEXT NOT NULL,target_id INTEGER NOT NULL,automatic INTEGER NOT NULL,sent_at INTEGER NOT NULL,
 evaluated_at INTEGER NOT NULL,responders INTEGER NOT NULL,quoted INTEGER NOT NULL,target_replied INTEGER NOT NULL,
 corrected INTEGER NOT NULL,positive INTEGER NOT NULL,first_response_secs INTEGER,evidence TEXT NOT NULL);
CREATE INDEX ai_reply_outcomes_scope ON ai_reply_outcomes(scope,automatic,sent_at);
CREATE INDEX ai_reply_outcomes_bot ON ai_reply_outcomes(bot_id,sent_at);";

/// Replies older than this are never evaluated, for example after downtime.
const MAX_AGE: i64 = 3 * 86400;
const BATCH: i64 = 64;
const MAX_REACTIONS: i64 = 40;
const MAX_EVIDENCE: usize = 8;
/// A direct asker's next message soon after the reply counts as a reaction.
const TARGET_FOLLOWUP_SECONDS: i64 = 120;

pub async fn init(store: &Store) -> Result<()> {
    store.migrate("ai_reply_outcomes", 1, SCHEMA).await
}

fn installed(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='ai_reply_outcomes')",
        [],
        |r| r.get(0),
    )?)
}

const CORRECTION_PREFIXES: &[&str] = &[
    "不对",
    "不是",
    "错了",
    "你错",
    "别",
    "不要",
    "没让你",
    "谁问你",
    "闭嘴",
    "答非所问",
    "听不懂",
    "不用",
    "算了",
    "无语",
];
const CORRECTION_PHRASES: &[&str] = &[
    "说错",
    "答非所问",
    "别说了",
    "别回了",
    "好烦",
    "闭嘴",
    "不是这个意思",
    "我没问",
    "跑题",
    "太长了",
    "啰嗦",
    "看不懂",
    "不好笑",
    "没意思",
];
const POSITIVE_PHRASES: &[&str] = &[
    "哈哈",
    "233",
    "笑死",
    "hhh",
    "xswl",
    "确实",
    "有道理",
    "谢谢",
    "谢了",
    "感谢",
    "好的",
    "牛",
    "厉害",
    "绝了",
    "懂了",
    "明白了",
    "学到了",
    "好耶",
    "太对了",
];

pub(crate) fn corrected(text: &str) -> bool {
    let text = text.trim().to_lowercase();
    CORRECTION_PREFIXES.iter().any(|p| text.starts_with(p))
        || CORRECTION_PHRASES.iter().any(|p| text.contains(p))
}

pub(crate) fn positive(text: &str) -> bool {
    let text = text.trim().to_lowercase();
    !corrected(&text) && POSITIVE_PHRASES.iter().any(|p| text.contains(p))
}

/// Text of a stored message body, which is either plain text or a JSON
/// envelope with a `text` field.
pub(crate) fn plain_text(content: &str) -> String {
    serde_json::from_str::<Value>(content)
        .ok()
        .and_then(|v| v.get("text").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| content.to_owned())
}

/// Evaluate replies whose observation window has closed. Returns how many
/// outcomes were stored.
pub fn evaluate(db: &Connection, bot: i64, window: i64) -> Result<usize> {
    if !installed(db)? {
        return Ok(0);
    }
    let time = now();
    let replies = db
        .prepare("SELECT m.key,m.scope,m.conversation_id,m.received_at,m.inbound_position FROM messages m
            WHERE m.role='assistant' AND m.origin_plugin='ai' AND m.scope LIKE ?1 AND m.received_at<=?2 AND m.received_at>=?3
            AND NOT EXISTS(SELECT 1 FROM ai_reply_outcomes o WHERE o.reply_key=m.key) ORDER BY m.id LIMIT ?4")?
        .query_map(params![format!("{bot}:%"), time - window, time - MAX_AGE, BATCH], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut stored = 0;
    for (reply_key, scope, conversation, sent_at, position) in replies {
        let trigger: Option<(String, i64)> = db
            .query_row(
                "SELECT key,sender_id FROM messages WHERE id=?1 AND role='user' AND scope=?2",
                params![position, scope],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (request_key, sender) = trigger.unwrap_or_default();
        let direct: bool = !request_key.is_empty()
            && db
                .query_row(
                    "SELECT COALESCE((SELECT direct FROM ai_context_sources WHERE source_key=?1),0)",
                    [&request_key],
                    |r| r.get(0),
                )
                .unwrap_or(false);
        let target = if direct { sender } else { 0 };
        let friend = scope.contains(":friend:");
        let reactions = db
            .prepare("SELECT m.key,m.sender_id,m.content,m.received_at,r.reply_to_key,COALESCE(r.mentioned_ids,'[]') FROM messages m
                LEFT JOIN ai_context_relations r ON r.source_key=m.key
                WHERE m.scope=?1 AND m.role='user' AND m.sender_id>0 AND m.sender_id!=?2 AND m.revoked=0
                AND COALESCE(m.origin_plugin,'') NOT IN('control','memory_only')
                AND m.received_at>=?3 AND m.received_at<=?4 AND m.key!=?5
                AND NOT EXISTS(SELECT 1 FROM ai_revoked_sources v WHERE v.source_key=m.key)
                ORDER BY m.id LIMIT ?6")?
            .query_map(params![scope, bot, sent_at, sent_at + window, request_key, MAX_REACTIONS], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, String>(5)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut responders = BTreeSet::new();
        let (mut quoted, mut target_replied, mut was_corrected, mut was_positive) =
            (0i64, false, false, false);
        let mut first: Option<i64> = None;
        let mut evidence = Vec::new();
        for (key, sender, content, received, reply_to, mentions) in reactions {
            let quotes_reply = reply_to.as_deref() == Some(reply_key.as_str());
            let mentions_bot = serde_json::from_str::<Vec<i64>>(&mentions)
                .unwrap_or_default()
                .contains(&bot);
            let from_target = target > 0
                && sender == target
                && (friend || received - sent_at <= TARGET_FOLLOWUP_SECONDS);
            if !(quotes_reply || mentions_bot || from_target || friend) {
                continue;
            }
            let text = plain_text(&content);
            responders.insert(sender);
            quoted += i64::from(quotes_reply);
            target_replied |= sender == target;
            was_corrected |= corrected(&text);
            was_positive |= positive(&text);
            first = Some(first.map_or(received - sent_at, |f| f.min(received - sent_at)));
            if evidence.len() < MAX_EVIDENCE {
                evidence.push(key);
            }
        }
        db.execute("INSERT OR IGNORE INTO ai_reply_outcomes(reply_key,bot_id,scope,conversation_id,request_key,target_id,automatic,sent_at,evaluated_at,responders,quoted,target_replied,corrected,positive,first_response_secs,evidence) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![reply_key,bot,scope,conversation,request_key,target,!direct,sent_at,time,responders.len() as i64,quoted,target_replied,was_corrected,was_positive,first,serde_json::to_string(&evidence)?])?;
        stored += 1;
    }
    Ok(stored)
}

/// Recent results of automatic replies in one chat.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Participation {
    pub replies: i64,
    pub engaged: i64,
    pub corrected: i64,
}

pub fn participation(db: &Connection, scope: &str, since: i64) -> Result<Participation> {
    if !installed(db)? {
        return Ok(Participation::default());
    }
    Ok(db.query_row(
        "SELECT count(*),COALESCE(sum(responders>0),0),COALESCE(sum(corrected),0) FROM ai_reply_outcomes WHERE scope=?1 AND automatic=1 AND sent_at>=?2",
        params![scope, since],
        |r| Ok(Participation { replies: r.get(0)?, engaged: r.get(1)?, corrected: r.get(2)? }),
    )?)
}

impl Participation {
    /// How much to lengthen the automatic-reply cooldown. Needs a few samples.
    pub fn cooldown_factor(&self) -> u64 {
        if self.replies < 4 {
            return 1;
        }
        let ignored = self.replies - self.engaged;
        if self.engaged * 4 < self.replies || self.corrected * 2 >= self.replies {
            3
        } else if ignored * 2 > self.replies {
            2
        } else {
            1
        }
    }

    /// A host-written note for observation turns, or None without data.
    pub fn hint(&self) -> Option<String> {
        if self.replies == 0 {
            return None;
        }
        let advice = if self.cooldown_factor() > 1 {
            "回应偏少或常被否定：只在明确有新信息或被需要时参与。"
        } else {
            ""
        };
        Some(format!(
            "\n【本群近24小时主动接话效果（宿主按引用、@和后续回复统计，仅供参考）】主动接话{}次，有人回应{}次，被否定{}次。{advice}",
            self.replies, self.engaged, self.corrected
        ))
    }
}

/// Effective cooldown for automatic replies in a chat.
pub fn adaptive_cooldown(base: u64, maximum: u64, participation: &Participation) -> u64 {
    let factor = participation.cooldown_factor();
    if factor <= 1 {
        return base;
    }
    let ceiling = maximum.max(base);
    (base.max(30) * factor).min(ceiling).max(base)
}

pub fn overview(db: &Connection, bot: i64) -> Result<Value> {
    if !installed(db)? {
        return Ok(Value::Null);
    }
    let mut periods = Vec::new();
    for (label, seconds) in [("24h", 86400i64), ("7d", 7 * 86400)] {
        let rows = db
            .prepare("SELECT automatic,count(*),COALESCE(sum(responders>0),0),COALESCE(sum(corrected),0),COALESCE(sum(positive),0),COALESCE(sum(quoted>0),0),avg(first_response_secs) FROM ai_reply_outcomes WHERE bot_id=?1 AND sent_at>=?2 GROUP BY automatic ORDER BY automatic")?
            .query_map(params![bot, now() - seconds], |r| {
                Ok(json!({"automatic":r.get::<_,bool>(0)?,"replies":r.get::<_,i64>(1)?,"engaged":r.get::<_,i64>(2)?,"corrected":r.get::<_,i64>(3)?,"positive":r.get::<_,i64>(4)?,"quoted":r.get::<_,i64>(5)?,"avg_first_response_secs":r.get::<_,Option<f64>>(6)?}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        periods.push(json!({"period":label,"rows":rows}));
    }
    Ok(json!({"periods":periods}))
}

/// `encoded` is a JSON array of revoked source keys.
pub(crate) fn invalidate_sources(db: &Connection, encoded: &str) -> Result<()> {
    if !installed(db)? {
        return Ok(());
    }
    db.execute("DELETE FROM ai_reply_outcomes WHERE reply_key IN(SELECT value FROM json_each(?1)) OR request_key IN(SELECT value FROM json_each(?1)) OR EXISTS(SELECT 1 FROM json_each(evidence) e WHERE e.value IN(SELECT value FROM json_each(?1)))", [encoded])?;
    Ok(())
}

pub(crate) fn maintain(db: &Connection) -> Result<()> {
    if !installed(db)? {
        return Ok(());
    }
    db.execute("DELETE FROM ai_reply_outcomes WHERE sent_at<?1 OR NOT EXISTS(SELECT 1 FROM messages m WHERE m.key=reply_key)", [now() - 30 * 86400])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaction_lexicon_separates_corrections_from_approval() {
        for text in ["不对，我说的是周五", "你说错了", "别回了", "答非所问"] {
            assert!(corrected(text), "{text}");
            assert!(!positive(text), "{text}");
        }
        for text in ["哈哈哈哈", "有道理，谢谢", "学到了"] {
            assert!(positive(text) && !corrected(text), "{text}");
        }
        assert!(!corrected("明天见") && !positive("明天见"));
        assert_eq!(plain_text(r#"{"sender_id":"1","text":"好的"}"#), "好的");
        assert_eq!(plain_text("原文"), "原文");
    }

    #[test]
    fn cooldown_only_grows_with_enough_ignored_samples_and_stays_bounded() {
        let few = Participation {
            replies: 3,
            engaged: 0,
            corrected: 0,
        };
        assert_eq!(adaptive_cooldown(30, 300, &few), 30);
        let ignored = Participation {
            replies: 8,
            engaged: 1,
            corrected: 0,
        };
        assert_eq!(adaptive_cooldown(30, 300, &ignored), 90);
        assert_eq!(adaptive_cooldown(30, 60, &ignored), 60);
        assert_eq!(adaptive_cooldown(120, 60, &ignored), 120);
        let mixed = Participation {
            replies: 8,
            engaged: 3,
            corrected: 0,
        };
        assert_eq!(adaptive_cooldown(30, 300, &mixed), 60);
        let welcome = Participation {
            replies: 8,
            engaged: 6,
            corrected: 1,
        };
        assert_eq!(adaptive_cooldown(30, 300, &welcome), 30);
        assert!(welcome.hint().unwrap().contains("有人回应6次"));
        assert!(ignored.hint().unwrap().contains("只在明确"));
        assert!(Participation::default().hint().is_none());
    }
}
