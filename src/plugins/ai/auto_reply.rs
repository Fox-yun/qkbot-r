//! Bounded, short-lived batches for an opted-in chat's automatic replies.
//! Text and identities are reloaded from storage; queued media retain resource IDs only.
use super::{
    reply::References,
    trigger::{Ingress, Quote},
    vision::{self, Images, Loaded, Origin},
};
use crate::{
    host::Host,
    types::{ContextGuard, Incoming, MessageKey, Scope, message_content, now},
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use std::{collections::BTreeSet, time::Duration};
use tokio_util::sync::CancellationToken;

pub(super) const MAX_MESSAGES: usize = 8;
const MAX_AGE_SECONDS: i64 = 120;
// The fifth input slot remains available for the existing sticker preview.
const MAX_BATCH_IMAGES: usize = crate::llm::MAX_INPUT_IMAGES - 1;

#[derive(Clone)]
pub(super) struct AutoSource {
    pub key: MessageKey,
    pub actor: i64,
    pub position: i64,
    pub received_at: i64,
    pub mentions: Vec<i64>,
    pub images: Images,
    pub quote: Option<Quote>,
    pub quote_seq: Option<i64>,
}

impl AutoSource {
    pub(super) fn from_message(message: &Incoming, position: i64, quote: Option<Quote>) -> Self {
        Self {
            key: message.key.clone(),
            actor: message.sender_id,
            position,
            received_at: message.received_at,
            mentions: message
                .mentions
                .iter()
                .copied()
                .filter(|id| *id > 0)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .take(32)
                .collect(),
            images: Images::from_message(message),
            quote,
            quote_seq: message.quote_seq.filter(|seq| *seq >= 0),
        }
    }

    fn quote_key(&self) -> Option<MessageKey> {
        self.quote
            .as_ref()
            .map(|quote| quote.key.clone())
            .or_else(|| {
                self.quote_seq.map(|message_seq| MessageKey {
                    scope: self.key.scope.clone(),
                    message_seq,
                })
            })
    }
}

#[derive(Clone)]
pub(super) struct AutoBatch {
    pub sources: Vec<AutoSource>,
}

impl AutoBatch {
    pub(super) fn new(source: AutoSource) -> Self {
        Self {
            sources: vec![source],
        }
    }

    /// A full batch is flushed by its owner; it never silently evicts an earlier message.
    pub(super) fn push(&mut self, source: AutoSource) -> bool {
        if self.sources.len() >= MAX_MESSAGES
            || self.sources.iter().any(|old| old.key == source.key)
            || self.sources.last().is_some_and(|last| {
                last.key.scope != source.key.scope || last.position >= source.position
            })
        {
            return false;
        }
        self.sources.push(source);
        true
    }

    pub(super) fn source_keys(&self) -> Vec<String> {
        self.sources.iter().map(|source| source.key.key()).collect()
    }

    pub(super) fn authorization(&self) -> BatchCheck {
        BatchCheck {
            sources: self
                .sources
                .iter()
                .map(|source| SourceCheck {
                    key: source.key.clone(),
                    actor: source.actor,
                    position: source.position,
                    received_at: source.received_at,
                    quote_key: source.quote_key(),
                })
                .collect(),
        }
    }
}

#[derive(Clone)]
struct SourceCheck {
    key: MessageKey,
    actor: i64,
    position: i64,
    received_at: i64,
    quote_key: Option<MessageKey>,
}

/// Lightweight dependencies retained through cooldown and the final outbox check.
#[derive(Clone)]
pub(super) struct BatchCheck {
    sources: Vec<SourceCheck>,
}

impl BatchCheck {
    pub(super) fn check(&self, db: &Connection, guard: &ContextGuard, scope: &Scope) -> Result<()> {
        crate::host::check_guard(db, guard)?;
        crate::access::require_auto_reply(db, &scope.key())?;
        ensure!(
            !self.sources.is_empty() && self.sources.len() <= MAX_MESSAGES,
            "自动接话批次无效"
        );
        let timestamp = now();
        let mut last_position = 0;
        let mut seen = BTreeSet::new();
        for source in &self.sources {
            ensure!(
                source.key.scope == *scope
                    && source.key.message_seq >= 0
                    && source.actor > 0
                    && source.actor != scope.bot_id
                    && source.position > last_position
                    && seen.insert(source.key.key()),
                "自动接话来源无效"
            );
            last_position = source.position;
            ensure!(
                source.received_at >= timestamp.saturating_sub(MAX_AGE_SECONDS)
                    && source.received_at <= timestamp.saturating_add(5),
                "自动接话消息已过期"
            );
            let valid: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE key=?1 AND scope=?2
                 AND conversation_id=?3 AND epoch=?4 AND sender_id=?5 AND inbound_position=?6
                 AND received_at=?7 AND revoked=0 AND role='user'
                 AND COALESCE(origin_plugin,'') NOT IN('control','memory_only')
                 AND NOT EXISTS(SELECT 1 FROM ai_revoked_sources WHERE source_key=messages.key))",
                params![
                    source.key.key(),
                    scope.key(),
                    guard.conversation_id,
                    guard.epoch,
                    source.actor,
                    source.position,
                    source.received_at
                ],
                |row| row.get(0),
            )?;
            ensure!(valid, "自动接话来源已失效");
            if let Some(quote) = &source.quote_key {
                ensure!(
                    quote.scope == *scope && quote.message_seq >= 0,
                    "自动接话引用不属于当前会话"
                );
                let revoked: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM ai_revoked_sources WHERE source_key=?1)
                     OR EXISTS(SELECT 1 FROM messages WHERE key=?1 AND revoked=1)",
                    [quote.key()],
                    |row| row.get(0),
                )?;
                ensure!(!revoked, "自动接话引用已遗忘");
            }
        }
        Ok(())
    }
}

pub(super) fn check(
    db: &Connection,
    guard: &ContextGuard,
    scope: &Scope,
    batch: &AutoBatch,
) -> Result<()> {
    batch.authorization().check(db, guard, scope)
}

pub(super) async fn check_host(
    host: &Host,
    guard: &ContextGuard,
    scope: &Scope,
    batch: &AutoBatch,
) -> Result<()> {
    let guard = guard.clone();
    let scope = scope.clone();
    let authorization = batch.authorization();
    host.store
        .call(move |db| authorization.check(db, &guard, &scope))
        .await
}

/// Persist the exact recalled source, including recalls received before the
/// original message. Other messages, memories and summaries remain stored.
pub(crate) async fn revoke_source(host: &Host, key: &MessageKey) -> Result<()> {
    ensure!(key.message_seq >= 0, "invalid recalled message sequence");
    let key = key.key();
    host.store
        .call(move |db| {
            let tx = db.transaction()?;
            tx.execute(
                "INSERT OR IGNORE INTO ai_revoked_sources(source_key,revoked_at) VALUES(?1,?2)",
                params![key, now()],
            )?;
            tx.execute(
                "UPDATE messages SET revoked=1,learnable=0 WHERE key=?1",
                [&key],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
}

/// Reuse the normal identity envelope and reference issuer. Every envelope is in
/// mandatory current input, so References::observe can retain its exact grants.
pub(super) fn load_sources(
    db: &Connection,
    guard: &ContextGuard,
    scope: &Scope,
    batch: &AutoBatch,
    references: &mut References,
    reference_enabled: bool,
) -> Result<String> {
    check(db, guard, scope, batch)?;
    let mut current =
        String::from("[本轮新增消息批次；以下按接收顺序排列，身份和真实提及由宿主提供。]\n");
    for (index, source) in batch.sources.iter().enumerate() {
        let (text, nickname, card): (String, Option<String>, Option<String>) = db.query_row(
            "SELECT content,sender_nickname,sender_card FROM messages WHERE key=?1",
            [source.key.key()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let mut content =
            message_content(source.actor, nickname.as_deref(), card.as_deref(), &text);
        if reference_enabled {
            content = references.local(
                source.key.key(),
                source.position,
                source.actor,
                "user",
                serde_json::from_str(&content)?,
                &source.mentions,
                true,
            )?;
        } else if !source.mentions.is_empty() {
            let mut envelope: serde_json::Value = serde_json::from_str(&content)?;
            envelope["mentions"] = serde_json::json!(
                source
                    .mentions
                    .iter()
                    .map(|id| { serde_json::json!({"user_id": id.to_string()}) })
                    .collect::<Vec<_>>()
            );
            content = envelope.to_string();
        }
        current.push_str(&format!("[新增消息 {}]\n{content}\n", index + 1));
    }
    Ok(current)
}

pub(super) struct BatchMedia {
    pub text: String,
    pub visual: Loaded,
    pub quote_refs: Vec<String>,
}

/// Preserve every explicit quote and attribute every supplied or omitted image.
/// Downloads reuse vision's resource checks, bounded decoding and cancellation.
#[allow(clippy::too_many_arguments)]
pub(super) async fn load_media(
    host: &Host,
    ingress: &Ingress,
    guard: &ContextGuard,
    batch: &AutoBatch,
    cancel: &CancellationToken,
    references: &mut References,
    reply_enabled: bool,
) -> Result<BatchMedia> {
    let scope = &batch
        .sources
        .first()
        .ok_or_else(|| anyhow::anyhow!("自动接话批次为空"))?
        .key
        .scope;
    check_host(host, guard, scope, batch).await?;
    let mut result = BatchMedia {
        text: String::new(),
        visual: Loaded::default(),
        quote_refs: vec![],
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    for (index, source) in batch.sources.iter().enumerate() {
        ensure!(!cancel.is_cancelled(), "自动接话已取消");
        let label = format!("新增消息 {}（用户 {}）", index + 1, source.actor);
        let quote = if let Some(quote) = &source.quote {
            Some(quote.clone())
        } else if let Some(key) = source.quote_key() {
            match tokio::time::timeout_at(deadline, ingress.quote(host, key, cancel)).await {
                Ok(result) => Some(result?),
                Err(_) => {
                    result
                        .text
                        .push_str(&format!("\n[{label}的引用消息未能读取；请勿猜测。]"));
                    None
                }
            }
        } else {
            None
        };
        if let Some(quote) = &quote {
            let content = quote.content(host).await?;
            ensure!(content != "[引用已遗忘]", "自动接话引用已遗忘");
            let content = if reply_enabled && content != "[引用不可用]" && quote.sender.is_some()
            {
                let (reference, envelope) =
                    references.quote(&quote.key, quote.sender.unwrap(), &content)?;
                result.quote_refs.push(reference);
                envelope
            } else {
                content
            };
            result
                .text
                .push_str(&format!("\n[{label}的引用消息]\n{content}"));
        }
        if !host.config.llm.vision {
            if !source.images.is_empty() || source.quote_key().is_some() {
                append_note(
                    &mut result.visual.note,
                    &format!("{label}：识图已关闭，当前及引用图片未读取，请勿猜测。"),
                );
            }
            continue;
        }
        if result.visual.images.len() >= MAX_BATCH_IMAGES || tokio::time::Instant::now() >= deadline
        {
            if !source.images.is_empty() || source.quote_key().is_some() {
                append_note(
                    &mut result.visual.note,
                    &format!(
                        "{label}：因批次图片数量或处理时限限制，当前及引用图片未提供，请勿猜测。"
                    ),
                );
            }
            continue;
        }
        let quote_images = if let Some(quote) = &quote {
            match tokio::time::timeout_at(deadline, ingress.quote_images(host, quote, cancel)).await
            {
                Ok(images) => images?,
                Err(_) => None,
            }
        } else {
            None
        };
        if quote.as_ref().is_some_and(|quote| quote.sender.is_some()) && quote_images.is_none() {
            append_note(
                &mut result.visual.note,
                &format!("{label}：未能检查引用中的图片，请勿猜测。"),
            );
        }
        let quoted_images = quote
            .as_ref()
            .zip(quote_images.as_ref())
            .filter(|(_, images)| !images.is_empty())
            .map(|(quote, images)| (&quote.key, images));
        if source.images.is_empty() && quoted_images.is_none() {
            continue;
        }
        check_host(host, guard, scope, batch).await?;
        match tokio::time::timeout_at(
            deadline,
            vision::load(host, &source.key, &source.images, quoted_images, cancel),
        )
        .await
        {
            Ok(loaded) => {
                let loaded = loaded?;
                if !loaded.note.is_empty() {
                    append_note(
                        &mut result.visual.note,
                        &format!("{label}：{}", loaded.note),
                    );
                }
                for image in loaded.images {
                    if result.visual.images.len() == MAX_BATCH_IMAGES {
                        append_note(
                            &mut result.visual.note,
                            &format!("{label}：部分已读取图片因批次数量限制未提供，请勿猜测。"),
                        );
                        break;
                    }
                    let origin = if image.origin == Origin::Quote {
                        "引用消息"
                    } else {
                        "消息本身"
                    };
                    append_note(
                        &mut result.visual.note,
                        &format!(
                            "第 {} 张消息图片（不含表情候选预览）来自{label}的{origin}。",
                            result.visual.images.len() + 1
                        ),
                    );
                    result.visual.images.push(image);
                }
            }
            Err(_) => append_note(
                &mut result.visual.note,
                &format!("{label}：图片处理超时，图片未提供，请勿猜测。"),
            ),
        }
        check_host(host, guard, scope, batch).await?;
    }
    ensure!(!cancel.is_cancelled(), "自动接话已取消");
    check_host(host, guard, scope, batch).await?;
    Ok(result)
}

fn append_note(target: &mut String, text: &str) {
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Scene;
    use serde_json::{Value, json};

    fn fixture() -> (Connection, Scope, ContextGuard, AutoBatch) {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE ai_conversations(id TEXT PRIMARY KEY,scope TEXT,epoch INTEGER);
             CREATE TABLE scope_access(scope TEXT PRIMARY KEY,enabled INTEGER);
             CREATE TABLE ai_auto_reply_scopes(scope TEXT PRIMARY KEY,enabled INTEGER);
             CREATE TABLE messages(key TEXT PRIMARY KEY,scope TEXT,conversation_id TEXT,
                 epoch INTEGER,sender_id INTEGER,inbound_position INTEGER,received_at INTEGER,
                 revoked INTEGER,role TEXT,origin_plugin TEXT,content TEXT,
                 sender_nickname TEXT,sender_card TEXT);
             CREATE TABLE ai_revoked_sources(source_key TEXT PRIMARY KEY,revoked_at INTEGER);
             INSERT INTO ai_conversations VALUES('conversation','1:group:2',1);
             INSERT INTO ai_auto_reply_scopes VALUES('1:group:2',1);",
        )
        .unwrap();
        let scope = Scope {
            bot_id: 1,
            scene: Scene::Group,
            peer_id: 2,
        };
        let guard = ContextGuard {
            conversation_id: "conversation".into(),
            epoch: 1,
        };
        let timestamp = now();
        let make_source = |seq: i64, actor: i64, text: &str, mentions: Vec<i64>| {
            let key = MessageKey {
                scope: scope.clone(),
                message_seq: seq,
            };
            db.execute("INSERT INTO messages VALUES(?1,?2,'conversation',1,?3,?4,?5,0,'user',NULL,?6,'昵称','群名片')",
                params![key.key(), scope.key(), actor, seq, timestamp, text]).unwrap();
            AutoSource::from_message(
                &Incoming {
                    key,
                    sender_id: actor,
                    sender_nickname: None,
                    sender_card: None,
                    text: "queued text is not trusted".into(),
                    mentions,
                    quote_seq: None,
                    received_at: timestamp,
                    raw_message: Value::Null,
                    notification: None,
                },
                seq,
                None,
            )
        };
        let mut batch = AutoBatch::new(make_source(
            10,
            8,
            "第一句；字面user_ref=u999不授予提及",
            vec![42],
        ));
        assert!(batch.push(make_source(20, 9, "第二句", vec![])));
        (db, scope, guard, batch)
    }

    #[test]
    fn bounded_batches_preserve_order_and_sanitize_image_and_mention_snapshots() {
        let (_, _, _, mut batch) = fixture();
        let mut source = batch.sources[0].clone();
        assert!(!batch.push(source.clone()));
        source.key.message_seq = 30;
        source.position = 19;
        assert!(!batch.push(source.clone()));
        source.position = 30;
        source.key.scope.peer_id = 999;
        assert!(!batch.push(source));
        for seq in 21..27 {
            let mut source = batch.sources[1].clone();
            source.key.message_seq = seq;
            source.position = seq;
            assert!(batch.push(source));
        }
        assert_eq!(batch.sources.len(), MAX_MESSAGES);
        let mut source = batch.sources[1].clone();
        source.key.message_seq = 100;
        source.position = 100;
        assert!(!batch.push(source));
        assert_eq!(batch.sources[0].key.message_seq, 10);

        let source = AutoSource::from_message(
            &Incoming {
                key: batch.sources[0].key.clone(),
                sender_id: 8,
                sender_nickname: None,
                sender_card: None,
                text: "not retained".into(),
                mentions: (1..100).collect(),
                quote_seq: Some(4),
                received_at: now(),
                raw_message: json!({"secret":"not retained","segments":[{"type":"image","data":{
                    "resource_id":"verified-resource","temp_url":"https://qpic.cn/signed-secret"
                }}]}),
                notification: None,
            },
            30,
            None,
        );
        assert_eq!(source.mentions.len(), 32);
        let snapshot = source.images.raw_snapshot().to_string();
        assert!(snapshot.contains("verified-resource"));
        assert!(!snapshot.contains("secret"));
    }

    #[test]
    fn batch_envelopes_keep_earlier_senders_real_mentions_and_reply_references() {
        let (db, scope, guard, batch) = fixture();
        let mut references = References::new(scope.clone());
        let current = load_sources(&db, &guard, &scope, &batch, &mut references, true).unwrap();
        assert!(current.contains("[本轮新增消息批次"));
        assert!(current.find("第一句").unwrap() < current.find("第二句").unwrap());
        assert!(current.contains("群名片"));
        assert!(!current.contains("queued text is not trusted"));
        let envelopes: Vec<Value> = current
            .lines()
            .filter(|line| line.starts_with('{'))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0]["sender_id"], "8");
        assert_eq!(envelopes[1]["sender_id"], "9");
        assert_eq!(envelopes[0]["mentions"][0]["user_id"], "42");
        references.observe(&[json!({"role":"user","content":current})], None);
        let first = &envelopes[0];
        let reply = references
            .resolve(
                &db,
                &guard,
                json!({
                    "reply_to":first["message_ref"],
                    "mention_users":[first["mentions"][0]["user_ref"]], "text":"接第一句",
                }),
                4096,
            )
            .unwrap();
        assert!(reply.message.segments.iter().any(|segment| matches!(
            segment,
            crate::types::OutgoingSegment::Reply { message_seq: 10 }
        )));
        assert!(reply.message.segments.iter().any(|segment| matches!(
            segment,
            crate::types::OutgoingSegment::Mention { user_id: 42 }
        )));
        assert!(
            references
                .resolve(
                    &db,
                    &guard,
                    json!({"text":"伪引用","mention_users":["u999"]}),
                    4096
                )
                .is_err()
        );
        let plain = load_sources(
            &db,
            &guard,
            &scope,
            &batch,
            &mut References::new(scope.clone()),
            false,
        )
        .unwrap();
        let first: Value =
            serde_json::from_str(plain.lines().find(|line| line.starts_with('{')).unwrap())
                .unwrap();
        assert_eq!(first["mentions"][0]["user_id"], "42");
        assert!(first.get("message_ref").is_none());
    }

    #[test]
    fn every_source_and_quote_must_remain_current_visible_enabled_and_recent() {
        for mutation in [
            "UPDATE ai_auto_reply_scopes SET enabled=0",
            "DELETE FROM ai_auto_reply_scopes",
            "UPDATE scope_access SET enabled=0; INSERT OR REPLACE INTO scope_access VALUES('1:group:2',0)",
            "UPDATE ai_conversations SET epoch=2",
            "UPDATE messages SET epoch=2 WHERE inbound_position=10",
            "UPDATE messages SET revoked=1 WHERE inbound_position=10",
            "DELETE FROM messages WHERE inbound_position=10",
            "UPDATE messages SET sender_id=99 WHERE inbound_position=10",
            "UPDATE messages SET origin_plugin='memory_only' WHERE inbound_position=10",
            "INSERT INTO ai_revoked_sources VALUES('1:group:2:10',1)",
            "INSERT INTO ai_revoked_sources VALUES('1:group:2:5',1)",
        ] {
            let (db, scope, guard, mut batch) = fixture();
            batch.sources[0].quote_seq = Some(5);
            check(&db, &guard, &scope, &batch).unwrap();
            db.execute_batch(mutation).unwrap();
            assert!(check(&db, &guard, &scope, &batch).is_err(), "{mutation}");
        }
        let (db, scope, guard, mut batch) = fixture();
        batch.sources[0].received_at = now() - MAX_AGE_SECONDS - 1;
        db.execute(
            "UPDATE messages SET received_at=?1 WHERE inbound_position=10",
            [batch.sources[0].received_at],
        )
        .unwrap();
        assert!(check(&db, &guard, &scope, &batch).is_err());
    }

    #[test]
    fn final_submission_rechecks_all_batch_sources_and_direct_interruptions() {
        let (db, scope, guard, batch) = fixture();
        let cancel = CancellationToken::new();
        let authorization = super::super::reply::Authorization::external(vec![])
            .with_auto_reply(&batch, cancel.clone());
        authorization.check_sources(&db, &scope, &guard).unwrap();
        cancel.cancel();
        assert!(authorization.check_sources(&db, &scope, &guard).is_err());

        let authorization = super::super::reply::Authorization::external(vec![])
            .with_auto_reply(&batch, CancellationToken::new());
        authorization.check_sources(&db, &scope, &guard).unwrap();
        db.execute(
            "UPDATE messages SET revoked=1 WHERE inbound_position=10",
            [],
        )
        .unwrap();
        assert!(authorization.check_sources(&db, &scope, &guard).is_err());
    }

    #[tokio::test]
    async fn transport_recall_is_exact_and_persists_even_before_the_source_arrives() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.app.database = directory
            .path()
            .join("recalls.sqlite3")
            .to_string_lossy()
            .into_owned();
        config.plugins.ai.enabled = false;
        config.plugins.native.enabled.clear();
        let host = Host::create(config.clone()).await.unwrap();
        host.store.call(|db| {
            db.execute("INSERT INTO ai_conversations(id,scope,mode,epoch,summary) VALUES('recall-test','1:group:2','group',1,'保持原有摘要')", [])?;
            for seq in [10, 20] {
                db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,learnable) VALUES(?1,'1:group:2','recall-test',1,3,'user','原文审计记录',?2,?3,1)", params![format!("1:group:2:{seq}"), now(), seq])?;
            }
            Ok(())
        }).await.unwrap();
        let mut source = MessageKey {
            scope: Scope {
                bot_id: 1,
                scene: Scene::Group,
                peer_id: 2,
            },
            message_seq: 10,
        };
        revoke_source(&host, &source).await.unwrap();
        revoke_source(&host, &source).await.unwrap();
        source.message_seq = 30;
        revoke_source(&host, &source).await.unwrap();
        host.cancel.cancel();
        host.store.stop().await.unwrap();
        drop(host);

        let restarted = Host::create(config).await.unwrap();
        restarted.store.call(|db| {
            let recalled: (bool, bool, String) = db.query_row("SELECT revoked,learnable,content FROM messages WHERE key='1:group:2:10'", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            assert_eq!(recalled, (true, false, "原文审计记录".into()));
            let other: (bool, bool, i64) = db.query_row("SELECT revoked,learnable,epoch FROM messages WHERE key='1:group:2:20'", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            assert_eq!(other, (false, true, 1));
            assert!(db.query_row("SELECT EXISTS(SELECT 1 FROM ai_revoked_sources WHERE source_key='1:group:2:30')", [], |row| row.get::<_, bool>(0))?);
            let conversation: (i64, String) = db.query_row("SELECT epoch,summary FROM ai_conversations WHERE id='recall-test'", [], |row| Ok((row.get(0)?, row.get(1)?)))?;
            assert_eq!(conversation, (1, "保持原有摘要".into()));
            Ok(())
        }).await.unwrap();
        restarted.cancel.cancel();
        restarted.store.stop().await.unwrap();
    }
}
