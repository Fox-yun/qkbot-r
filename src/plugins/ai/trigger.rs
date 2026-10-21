//! Resolve quote-only triggers before deciding how to retain the current message.
//! Ingress has already persisted a content-free deduplication row. Slow lookups
//! run outside dispatch; later messages in the same conversation stay ordered.
use super::{Conversation, chat::Chat};
use crate::{
    config::Limits,
    host::Host,
    types::{ContextGuard, Incoming, MessageKey, OutgoingMessage, Scene, message_content},
};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reason {
    Private,
    Mention,
    Keyword,
    ReplyToBot,
}

pub(super) fn direct_reason(msg: &Incoming, keywords: &[String]) -> Option<Reason> {
    if msg.is_self() || msg.notification.is_some() {
        None
    } else if msg.key.scope.scene == Scene::Friend {
        Some(Reason::Private)
    } else if msg.mentions.contains(&msg.key.scope.bot_id) {
        Some(Reason::Mention)
    } else if keywords
        .iter()
        .any(|k| !k.is_empty() && msg.text.contains(k))
    {
        Some(Reason::Keyword)
    } else {
        None
    }
}

#[derive(Clone)]
pub(super) struct Decision {
    pub reason: Option<Reason>,
    pub quote: Option<Quote>,
}

/// Only a bounded, normalized snapshot crosses into the model queue. Raw Milky
/// events, previews and forwarded identities never establish a quote's sender.
#[derive(Clone)]
pub(super) struct Quote {
    pub key: MessageKey,
    pub(super) sender: Option<i64>,
    content: String,
}
impl Quote {
    pub(super) fn from_message(message: &Incoming) -> Self {
        Self::message(
            message.key.clone(),
            message.sender_id,
            message.sender_nickname.as_deref(),
            message.sender_card.as_deref(),
            &message.text,
            None,
        )
    }
    fn unavailable(key: MessageKey) -> Self {
        Self {
            key,
            sender: None,
            content: "[引用不可用]".into(),
        }
    }

    fn revoked(key: MessageKey) -> Self {
        Self {
            key,
            sender: None,
            content: "[引用已遗忘]".into(),
        }
    }

    fn message(
        key: MessageKey,
        sender: i64,
        nickname: Option<&str>,
        card: Option<&str>,
        text: &str,
        origin: Option<&str>,
    ) -> Self {
        let source = if sender == key.scope.bot_id {
            origin
                .map(|id| format!("本账号；来源插件：{id}"))
                .unwrap_or_else(|| "本账号（仅凭引用无法确认来源插件）".into())
        } else {
            "原始发言者".into()
        };
        let text = if text.is_empty() {
            "[引用没有可用文字；未读取图片或其他媒体内容]".into()
        } else {
            text.chars().take(1000).collect::<String>()
        };
        Self {
            key,
            sender: Some(sender),
            content: format!(
                "[引用消息；{source}] {}",
                message_content(sender, nickname, card, &text)
            ),
        }
    }

    pub async fn content(&self, host: &Host) -> Result<String> {
        let key = self.key.key();
        if host.store.call(move |db| revoked(db, &key)).await? {
            Ok("[引用已遗忘]".into())
        } else {
            Ok(self.content.clone())
        }
    }
}

fn revoked(db: &Connection, key: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM ai_revoked_sources WHERE source_key=?1)
         OR EXISTS(SELECT 1 FROM messages WHERE key=?1 AND revoked=1)",
        [key],
        |r| r.get(0),
    )?)
}

async fn local_quote(host: &Host, key: &MessageKey) -> Result<Option<Quote>> {
    let key = key.clone();
    host.store
        .call(move |db| {
            if revoked(db, &key.key())? {
                return Ok(Some(Quote::revoked(key)));
            }
            type Row = (i64, String, Option<String>, Option<String>, Option<String>);
            let row: Option<Row> = db.query_row(
            "SELECT sender_id,substr(content,1,1000),sender_nickname,sender_card,origin_plugin
             FROM messages WHERE key=?1 AND scope=?2",
            params![key.key(), key.scope.key()],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
        ).optional()?;
            if let Some((sender, text, nickname, card, origin)) = row {
                return Ok(Some(Quote::message(
                    key,
                    sender,
                    nickname.as_deref(),
                    card.as_deref(),
                    &text,
                    origin.as_deref(),
                )));
            }
            // Confirmed sends from every plugin share this ledger. Failed, prepared
            // and ambiguous sends cannot be used to prove bot authorship.
            let sent: Option<(String, String)> = db
                .query_row(
                    "SELECT body,origin_plugin FROM outbox WHERE scope=?1 AND message_seq=?2
             AND state='confirmed' ORDER BY created_at DESC LIMIT 1",
                    params![key.scope.key(), key.message_seq],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            Ok(sent.map(|(body, origin)| {
                let text = serde_json::from_str::<OutgoingMessage>(&body)
                    .map(|message| message.text_content())
                    .unwrap_or_default();
                let sender = key.scope.bot_id;
                Quote::message(key, sender, None, None, &text, Some(&origin))
            }))
        })
        .await
}

struct Pending {
    message: Incoming,
    position: i64,
    conversation: Conversation,
    access_revision: i64,
    generation: Option<u64>,
    cancel: CancellationToken,
    _slot: OwnedSemaphorePermit,
}
struct Lane {
    active: (ContextGuard, CancellationToken),
    queue: VecDeque<Pending>,
}

pub(super) struct Ingress {
    lanes: Mutex<HashMap<String, Lane>>,
    slots: Arc<Semaphore>,
    lookups: Arc<Semaphore>,
    receipts: Arc<Semaphore>,
    per_conversation: usize,
    tasks: TaskTracker,
}

impl Ingress {
    pub fn new(limits: &Limits) -> Self {
        Self {
            lanes: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(limits.queued_global)),
            lookups: Arc::new(Semaphore::new(2)),
            receipts: Arc::new(Semaphore::new(limits.control_tasks)),
            per_conversation: limits.queued_per_conversation,
            tasks: TaskTracker::new(),
        }
    }

    pub fn cancel(&self, matches: impl Fn(&ContextGuard) -> bool) {
        for lane in self.lanes.lock().unwrap().values() {
            if matches(&lane.active.0) {
                lane.active.1.cancel();
            }
            for job in &lane.queue {
                if matches(&job.conversation.guard) {
                    job.cancel.cancel();
                }
            }
        }
    }

    pub async fn shutdown(&self) {
        self.cancel(|_| true);
        self.tasks.close();
        self.tasks.wait().await;
    }

    pub async fn submit(
        self: &Arc<Self>,
        chat: &Arc<Chat>,
        mut message: Incoming,
        position: i64,
        conversation: Conversation,
        access_revision: i64,
    ) -> Result<()> {
        let Ok(slot) = self.slots.clone().try_acquire_owned() else {
            self.feedback(&chat.host, &message, position);
            return Ok(());
        };
        // Keep only bounded image resource identifiers while waiting. Raw
        // previews, forwarded nodes and temporary URLs never enter the AI queue.
        message.raw_message = super::vision::Images::from_message(&message).raw_snapshot();
        let job = Pending {
            generation: chat.host.milky.scope_generation(&message.key.scope).ok(),
            message,
            position,
            conversation,
            access_revision,
            cancel: chat.host.cancel.child_token(),
            _slot: slot,
        };
        let id = job.conversation.guard.conversation_id.clone();
        {
            let mut lanes = self.lanes.lock().unwrap();
            if let Some(lane) = lanes.get_mut(&id) {
                if lane.queue.len() >= self.per_conversation {
                    self.feedback(&chat.host, &job.message, position);
                    return Ok(());
                }
                lane.queue.push_back(job);
                return Ok(());
            }
            lanes.insert(
                id.clone(),
                Lane {
                    active: (job.conversation.guard.clone(), job.cancel.clone()),
                    queue: VecDeque::new(),
                },
            );
        }
        // A tracked worker owns the lane even if the dispatch caller disappears.
        // Local decisions finish storage before returning; remote work releases
        // ingress immediately, while the lane keeps subsequent messages ordered.
        let (ready, wait) = oneshot::channel();
        self.spawn(chat, id, job, ready);
        wait.await.context("消息预处理已停止")?
    }

    fn spawn(
        self: &Arc<Self>,
        chat: &Arc<Chat>,
        id: String,
        first: Pending,
        ready: oneshot::Sender<Result<()>>,
    ) {
        let this = self.clone();
        let chat = chat.clone();
        self.tasks.spawn(async move {
            let mut job = first;
            let mut ready = Some(ready);
            loop {
                let result = async {
                    if job.cancel.is_cancelled() {
                        return Ok(());
                    }
                    let decision = match this.local_decision(&chat.host, &job).await? {
                        Some(decision) => decision,
                        None => {
                            if let Some(ready) = ready.take() {
                                let _ = ready.send(Ok(()));
                            }
                            let key = quote_key(&job.message).expect("quote-only lookup");
                            let quote = this.remote_quote(&chat.host, key, &job.cancel).await;
                            Decision {
                                reason: (quote.sender == Some(job.message.key.scope.bot_id))
                                    .then_some(Reason::ReplyToBot),
                                quote: Some(quote),
                            }
                        }
                    };
                    this.finish(&chat, &job, decision).await
                }
                .await;
                if let Some(ready) = ready.take() {
                    let _ = ready.send(result);
                } else if let Err(error) = result {
                    tracing::warn!(%error, "message preprocessing failed");
                }
                drop(job);
                let Some(next) = this.advance(&id) else { break };
                job = next;
            }
        });
    }

    fn advance(&self, id: &str) -> Option<Pending> {
        let mut lanes = self.lanes.lock().unwrap();
        let lane = lanes.get_mut(id)?;
        if let Some(next) = lane.queue.pop_front() {
            lane.active = (next.conversation.guard.clone(), next.cancel.clone());
            Some(next)
        } else {
            lanes.remove(id);
            None
        }
    }

    async fn local_decision(&self, host: &Host, job: &Pending) -> Result<Option<Decision>> {
        let reason = direct_reason(&job.message, &host.config.plugins.ai.keywords);
        let settings = &job.conversation.settings;
        if reason.is_some()
            || !settings.reply_to_bot
            || (settings.paused && !settings.once)
            || job.cancel.is_cancelled()
            || job.message.is_self()
        {
            return Ok(Some(Decision {
                reason,
                quote: None,
            }));
        }
        let Some(key) = quote_key(&job.message) else {
            return Ok(Some(Decision {
                reason: None,
                quote: None,
            }));
        };
        Ok(local_quote(host, &key).await?.map(|quote| Decision {
            reason: (quote.sender == Some(key.scope.bot_id)).then_some(Reason::ReplyToBot),
            quote: Some(quote),
        }))
    }

    async fn remote_quote(
        &self,
        host: &Host,
        key: MessageKey,
        cancel: &CancellationToken,
    ) -> Quote {
        // Congestion is an unavailable quote, never an unbounded wait or task.
        let Ok(_permit) = self.lookups.clone().try_acquire_owned() else {
            return Quote::unavailable(key);
        };
        let fetched = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            result = tokio::time::timeout(LOOKUP_TIMEOUT, host.milky.get_message(&key)) => result.ok().and_then(Result::ok),
        };
        match fetched {
            Some(message) => Quote::from_message(&message),
            None => Quote::unavailable(key),
        }
    }

    pub async fn quote(
        &self,
        host: &Host,
        key: MessageKey,
        cancel: &CancellationToken,
    ) -> Result<Quote> {
        match local_quote(host, &key).await? {
            Some(quote) => Ok(quote),
            None => Ok(self.remote_quote(host, key, cancel).await),
        }
    }

    async fn finish(&self, chat: &Arc<Chat>, job: &Pending, mut decision: Decision) -> Result<()> {
        let host = &chat.host;
        if job.cancel.is_cancelled()
            || job
                .generation
                .is_some_and(|generation| host.milky.check_generation(generation).is_err())
        {
            return Ok(());
        }
        let message = job.message.clone();
        let guard = job.conversation.guard.clone();
        let access_revision = job.access_revision;
        let position = job.position;
        let defaults = host.config.plugins.ai.defaults.clone();
        let read_background = host.config.plugins.ai.read_group_background;
        let learn_background = host.config.plugins.ai.learn_all_group_messages;
        let epoch = host.epochs["ai"];
        let reason = decision.reason;
        let quote = decision.quote.as_ref().map(|quote| quote.key.key());
        let ready = host
            .store
            .call(move |db| {
                let tx = db.transaction()?;
                if crate::access::check_revision(&tx, &message.key.scope.key(), access_revision)
                    .is_err()
                    || crate::host::check_guard(&tx, &guard).is_err()
                    || crate::host::check_plugin(&tx, "ai", epoch).is_err()
                    || revoked(&tx, &message.key.key())?
                {
                    return Ok(None);
                }
                let overrides: String = tx.query_row(
                    "SELECT settings FROM ai_conversations WHERE id=?1",
                    [&guard.conversation_id],
                    |r| r.get(0),
                )?;
                let settings = super::resolve_settings(&defaults, &overrides)?;
                let mut reason = reason;
                if reason == Some(Reason::ReplyToBot)
                    && (!settings.reply_to_bot
                        || quote
                            .as_ref()
                            .is_some_and(|key| revoked(&tx, key).unwrap_or(true)))
                {
                    reason = None;
                }
                let trigger = reason.is_some();
                let keep = trigger || read_background;
                let learn = settings.memory_write
                    && (!settings.paused || settings.once)
                    && (trigger || learn_background);
                let origin = if learn && !keep {
                    Some("memory_only")
                } else {
                    None
                };
                tx.execute(
                    "UPDATE messages SET role=?2,content=?3,origin_plugin=?4,learnable=?5
                WHERE id=?1 AND conversation_id=?6 AND epoch=?7 AND revoked=0",
                    params![
                        position,
                        if keep || learn { "user" } else { "control" },
                        if keep || learn { &message.text } else { "" },
                        origin,
                        learn,
                        guard.conversation_id,
                        guard.epoch
                    ],
                )?;
                tx.commit()?;
                Ok(Some((settings, reason)))
            })
            .await?;
        let Some((settings, reason)) = ready else {
            return Ok(());
        };
        decision.reason = reason;
        if reason.is_none() || (settings.paused && !settings.once) || job.cancel.is_cancelled() {
            return Ok(());
        }
        let mut conversation = job.conversation.clone();
        conversation.settings = settings;
        match chat.enqueue_resolved(&job.message, job.position, &conversation, Some(decision)) {
            Ok(()) if conversation.settings.once => {
                let guard = conversation.guard;
                host.store.call(move |db| {
                    db.execute("UPDATE ai_conversations SET settings=json_set(settings,'$.once',json('false')) WHERE id=?1 AND epoch=?2", params![guard.conversation_id, guard.epoch])?;
                    Ok(())
                }).await?;
            }
            Ok(()) => {}
            Err(error) => {
                tracing::warn!(%error, "chat rejected at queue boundary");
                self.feedback(host, &job.message, job.position);
            }
        }
        Ok(())
    }

    pub fn feedback(&self, host: &Arc<Host>, message: &Incoming, position: i64) {
        if let Ok(permit) = self.receipts.clone().try_acquire_owned() {
            let host = host.clone();
            let scope = message.key.scope.clone();
            self.tasks.spawn(async move {
                let _permit = permit;
                let _ = host
                    .send(
                        &scope,
                        "host",
                        None,
                        position,
                        "当前聊天队列已满，请稍后重试。".into(),
                    )
                    .await;
            });
        }
    }
}

fn quote_key(message: &Incoming) -> Option<MessageKey> {
    message
        .quote_seq
        .filter(|seq| *seq >= 0)
        .map(|message_seq| MessageKey {
            scope: message.key.scope.clone(),
            message_seq,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, types::Scope};

    #[tokio::test]
    async fn quote_lookup_is_scoped_bounded_and_never_resurrects_revoked_sources() {
        let mut config = Config::default();
        config.app.database = ":memory:".into();
        config.plugins.ai.enabled = false;
        let host = Host::create(config).await.unwrap();
        host.store.call(|db| {
            db.execute_batch("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at)
                VALUES('1:group:2:7','1:group:2','',0,1,'system','原生插件',1),
                ('1:group:2:8','1:group:2','',0,3,'control','',1);")?;
            let body = serde_json::to_string(&OutgoingMessage::text("长".repeat(2000)))?;
            for (seq, state, body) in [(9, "confirmed", body.as_str()), (10, "unknown", body.as_str()), (11, "confirmed", "{\"segments\":[{\"type\":\"image\",\"data\":{\"uri\":\"https://example.invalid/image.png\"}}]}")] {
                db.execute("INSERT INTO outbox(id,scope,origin_plugin,plugin_epoch,body,state,message_seq,created_at,updated_at)
                    VALUES(?1,'1:group:2','nb_music',1,?2,?3,?4,1,1)", params![format!("out-{seq}"),body,state,seq])?;
            }
            Ok(())
        }).await.unwrap();
        let key = |seq| MessageKey {
            scope: Scope {
                bot_id: 1,
                scene: Scene::Group,
                peer_id: 2,
            },
            message_seq: seq,
        };
        assert_eq!(
            local_quote(&host, &key(7)).await.unwrap().unwrap().sender,
            Some(1)
        );
        assert_eq!(
            local_quote(&host, &key(8)).await.unwrap().unwrap().sender,
            Some(3)
        );
        let ledger = local_quote(&host, &key(9)).await.unwrap().unwrap();
        assert!(ledger.content.contains("nb_music"));
        assert_eq!(ledger.content.matches('长').count(), 1000);
        assert!(local_quote(&host, &key(10)).await.unwrap().is_none());
        assert!(
            local_quote(&host, &key(11))
                .await
                .unwrap()
                .unwrap()
                .content
                .contains("未读取图片")
        );
        for scope in [
            Scope {
                bot_id: 2,
                ..key(7).scope
            },
            Scope {
                peer_id: 3,
                ..key(7).scope
            },
            Scope {
                scene: Scene::Friend,
                ..key(7).scope
            },
        ] {
            assert!(
                local_quote(
                    &host,
                    &MessageKey {
                        scope,
                        message_seq: 7
                    }
                )
                .await
                .unwrap()
                .is_none()
            );
        }
        host.store
            .call(|db| {
                db.execute("INSERT INTO ai_revoked_sources VALUES('1:group:2:9',1)", [])?;
                db.execute("UPDATE messages SET revoked=1 WHERE key='1:group:2:7'", [])?;
                Ok(())
            })
            .await
            .unwrap();
        for seq in [7, 9] {
            let quote = local_quote(&host, &key(seq)).await.unwrap().unwrap();
            assert_eq!(quote.sender, None);
            assert_eq!(quote.content, "[引用已遗忘]");
        }
        assert_eq!(
            ledger.content(&host).await.unwrap(),
            "[引用已遗忘]",
            "queued snapshots recheck revocation"
        );
        host.store.stop().await.unwrap();
    }
}
