//! Optional semantic cache over authoritative, subject-scoped memory rows.
//! Embeddings never replace memories, evidence, FTS, or normal memory cursors.
use super::memory::{self, Memory};
use crate::{
    config::AiSettings,
    embedding::{cosine, decode_vector, encode_vector},
    host::Host,
    store::Store,
    types::{ContextGuard, now},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

const SCAN_LIMIT: usize = 512;
const CANDIDATE_LIMIT: usize = 128;
const QUERY_MESSAGE_LIMIT: usize = 3;
const QUERY_RECENT_SECONDS: i64 = 120;
const REQUESTS_PER_HOUR: i64 = 120;
const RETRY_DELAY: i64 = 300;
const MIGRATION: &str = r#"
CREATE TABLE ai_memory_vectors(
 memory_id INTEGER NOT NULL, namespace TEXT NOT NULL, memory_version INTEGER NOT NULL,
 document_hash TEXT NOT NULL, dimensions INTEGER NOT NULL, vector BLOB NOT NULL,
 indexed_at INTEGER NOT NULL, PRIMARY KEY(memory_id,namespace)
);
CREATE TABLE ai_semantic_attempts(
 memory_id INTEGER NOT NULL, namespace TEXT NOT NULL, retry_after INTEGER NOT NULL,
 PRIMARY KEY(memory_id,namespace)
);
CREATE TABLE ai_semantic_admissions(
 id INTEGER PRIMARY KEY, at INTEGER NOT NULL, purpose TEXT NOT NULL
);
CREATE INDEX ai_semantic_admission_time ON ai_semantic_admissions(at);
CREATE TRIGGER ai_semantic_memory_deleted AFTER DELETE ON ai_memories BEGIN
 DELETE FROM ai_memory_vectors WHERE memory_id=OLD.id;
 DELETE FROM ai_semantic_attempts WHERE memory_id=OLD.id;
END;
CREATE TRIGGER ai_semantic_memory_changed AFTER UPDATE OF text,kind,active,expires_at,version ON ai_memories BEGIN
 DELETE FROM ai_memory_vectors WHERE memory_id=OLD.id;
 DELETE FROM ai_semantic_attempts WHERE memory_id=OLD.id;
END;
"#;

// Tombstones remain authoritative after raw message retention has removed the
// original messages. Manual memories need not have a still-existing source row.
const ELIGIBLE: &str = "m.active=1 AND (m.expires_at IS NULL OR m.expires_at>?1) AND NOT EXISTS(SELECT 1 FROM ai_memory_provenance p JOIN ai_revoked_sources r ON r.source_key=p.source_key WHERE p.memory_id=m.id) AND NOT EXISTS(SELECT 1 FROM json_each(m.evidence) e JOIN ai_revoked_sources r ON r.source_key=json_extract(e.value,'$.source_key'))";

pub async fn init(store: &Store) -> Result<()> {
    store.migrate("ai_semantic", 1, MIGRATION).await
}

#[derive(Clone)]
struct Request {
    guard: ContextGuard,
    subject: i64,
    bot_id: i64,
    plugin_epoch: i64,
    defaults: AiSettings,
    once_query: Option<OnceQueryPermit>,
}

/// An ephemeral permit captured only when a paused direct turn is admitted.
/// Its binding survives consumption of settings.once without authorizing a
/// different sender, message, conversation, or epoch.
#[derive(Clone, Debug)]
pub(crate) struct OnceQueryPermit {
    guard: ContextGuard,
    source_key: String,
    actor: i64,
}

impl OnceQueryPermit {
    pub(super) fn capture(
        conversation: &super::Conversation,
        message: &crate::types::Incoming,
        automatic: bool,
    ) -> Option<Self> {
        (conversation.settings.paused
            && conversation.settings.once
            && !automatic
            && conversation.scope == message.key.scope)
            .then(|| Self {
                guard: conversation.guard.clone(),
                source_key: message.key.key(),
                actor: message.sender_id,
            })
    }

    fn matches(&self, guard: &ContextGuard, source_key: &str, actor: i64) -> bool {
        self.guard.conversation_id == guard.conversation_id
            && self.guard.epoch == guard.epoch
            && self.source_key == source_key
            && self.actor == actor
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Boundary {
    scope: String,
    memory_scope: String,
    mode: String,
    owner: Option<i64>,
    settings: String,
    mode_revision: i64,
    access_revision: i64,
    plugin_revision: i64,
    memory_revision: i64,
}

#[derive(Clone)]
struct Document {
    memory: Memory,
    version: i64,
    hash: String,
    input: String,
}

#[derive(Clone)]
struct IndexSnapshot {
    request: Request,
    boundary: Boundary,
    document: Document,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QuerySource {
    key: String,
    text: String,
    position: i64,
    received_at: i64,
    access_revision: i64,
}

#[derive(Clone)]
struct QuerySnapshot {
    request: Request,
    boundary: Boundary,
    source: QuerySource,
    preceding: Vec<QuerySource>,
}

impl QuerySnapshot {
    fn input(&self) -> String {
        self.preceding
            .iter()
            .chain(std::iter::once(&self.source))
            .map(|source| source.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn request(host: &Host, guard: &ContextGuard, subject: i64) -> Result<Request> {
    ensure!(host.config.plugins.ai.enabled, "semantic AI is disabled");
    Ok(Request {
        guard: guard.clone(),
        subject,
        bot_id: host
            .milky
            .verified_bot_id()
            .context("semantic account is unverified")?,
        plugin_epoch: *host
            .epochs
            .get("ai")
            .context("semantic AI plugin is disabled")?,
        defaults: host.config.plugins.ai.defaults.clone(),
        once_query: None,
    })
}

fn query_request(
    host: &Host,
    guard: &ContextGuard,
    subject: i64,
    source_key: &str,
    once_query: Option<&OnceQueryPermit>,
) -> Result<Request> {
    let mut request = request(host, guard, subject)?;
    if let Some(permit) = once_query {
        ensure!(
            permit.matches(guard, source_key, subject),
            "one-shot memory query permit does not match the admitted turn"
        );
        request.once_query = Some(permit.clone());
    }
    Ok(request)
}

fn boundary(db: &Connection, request: &Request) -> Result<Boundary> {
    crate::host::check_guard(db, &request.guard)?;
    crate::host::check_plugin(db, "ai", request.plugin_epoch)?;
    ensure!(
        request.subject > 0 && request.subject != request.bot_id,
        "invalid semantic memory subject"
    );
    let (scope, mode, owner, settings, revision): (String, String, Option<i64>, String, i64) = db
        .query_row(
        "SELECT scope,mode,owner_id,settings,memory_revision FROM ai_conversations WHERE id=?1",
        [&request.guard.conversation_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let parts: Vec<_> = scope.split(':').collect();
    ensure!(
        parts.len() == 3 && parts[0].parse::<i64>().ok() == Some(request.bot_id),
        "semantic account scope changed"
    );
    ensure!(
        owner.is_none_or(|owner| owner == request.subject),
        "semantic subject is outside this conversation"
    );
    let (current_mode, mode_revision) = match parts[1] {
        "friend" => ("user".to_owned(), 0),
        "group" => db
            .query_row(
                "SELECT mode,revision FROM ai_scope_modes WHERE scope=?1",
                [&scope],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?
            .unwrap_or_else(|| (request.defaults.mode.clone(), 0)),
        _ => anyhow::bail!("invalid semantic scene"),
    };
    let current_id = match owner {
        Some(owner) if current_mode == "user" => format!("{scope}:v{mode_revision}:user:{owner}"),
        None if current_mode == "group" => format!("{scope}:v{mode_revision}:group"),
        _ => anyhow::bail!("semantic conversation mode changed"),
    };
    ensure!(
        current_id == request.guard.conversation_id && mode == current_mode,
        "semantic conversation version changed"
    );
    let effective = super::resolve_settings(&request.defaults, &settings)?;
    ensure!(
        effective.memory_read && (!effective.paused || request.once_query.is_some()),
        "semantic memory reads are disabled"
    );
    let access_revision = crate::access::require_enabled(db, &scope)?;
    let plugin_revision = crate::catalog::allowed_revision(db, &scope, "ai")?
        .context("semantic AI is disabled for this scope")?;
    Ok(Boundary {
        memory_scope: memory::scope::resolve(db, &request.guard.conversation_id)?,
        scope,
        mode,
        owner,
        settings,
        mode_revision,
        access_revision,
        plugin_revision,
        memory_revision: revision,
    })
}

fn document(db: &Connection, request: &Request, id: i64) -> Result<Document> {
    let memory_scope = memory::scope::resolve(db, &request.guard.conversation_id)?;
    let version=db.query_row(&format!("SELECT m.version FROM ai_memories m WHERE m.id=?2 AND m.conversation_id IN (SELECT id FROM ai_conversations WHERE memory_scope=?3) AND m.subject_id=?4 AND {ELIGIBLE}"),params![now(),id,memory_scope,request.subject],|r|r.get(0)).optional()?.context("semantic memory is no longer eligible")?;
    let memory = memory::get_memory(db, id)?.context("semantic memory disappeared")?;
    let input = format!("{}: {}", memory.kind.as_str(), memory.text);
    let hash = format!("{:x}", Sha256::digest(input.as_bytes()));
    Ok(Document {
        memory,
        version,
        hash,
        input,
    })
}

fn query_source(
    db: &Connection,
    request: &Request,
    context: &Boundary,
    key: &str,
) -> Result<QuerySource> {
    let source=db.query_row("SELECT key,content,id,received_at,access_revision FROM messages WHERE key=?1 AND conversation_id=?2 AND epoch=?3 AND scope=?4 AND sender_id=?5 AND role='user' AND revoked=0 AND COALESCE(origin_plugin,'') NOT IN('control','memory_only') AND NOT EXISTS(SELECT 1 FROM ai_revoked_sources WHERE source_key=?1)",params![key,request.guard.conversation_id,request.guard.epoch,context.scope,request.subject],|r|Ok(QuerySource {key:r.get(0)?,text:r.get(1)?,position:r.get(2)?,received_at:r.get(3)?,access_revision:r.get(4)?})).optional()?.context("semantic query source is unavailable")?;
    ensure!(
        source.access_revision == context.access_revision,
        "semantic query access changed"
    );
    Ok(source)
}

/// Preserve a split question when its owner follows up before receiving an
/// answer. Never use another speaker, conversation, or future message as input.
fn preceding_query_sources(
    db: &Connection,
    request: &Request,
    context: &Boundary,
    current: &QuerySource,
) -> Result<Vec<QuerySource>> {
    let since = current.received_at.saturating_sub(QUERY_RECENT_SECONDS);
    // Use the answered request's position, not its reply's row ID: a response
    // to an older question may arrive after a newer, still unanswered question.
    let answers = db.prepare(
        "SELECT a.key,a.inbound_position FROM messages a
         JOIN messages question ON question.id=a.inbound_position
         JOIN outbox o ON o.scope=a.scope AND a.key=o.scope||':'||o.message_seq
           AND o.conversation_id=a.conversation_id AND o.context_epoch=a.epoch
         LEFT JOIN ai_recent_turns turn ON turn.reply_key=a.key
         WHERE a.conversation_id=?1 AND a.epoch=?2 AND a.scope=?3
           AND a.role='assistant' AND a.sender_id=?4 AND a.revoked=0
           AND a.id<?5 AND a.inbound_position<?5 AND a.received_at>=?6
           AND question.sender_id=?7 AND question.role='user' AND question.revoked=0
           AND question.conversation_id=a.conversation_id AND question.epoch=a.epoch
           AND question.scope=a.scope
           AND o.origin_plugin='ai' AND o.state='confirmed'
           AND (turn.reply_key IS NULL OR turn.completed_position<?5)
           AND NOT EXISTS(SELECT 1 FROM ai_revoked_sources WHERE source_key=a.key OR source_key=question.key)
         ORDER BY a.inbound_position DESC LIMIT 8")?
        .query_map(params![request.guard.conversation_id,request.guard.epoch,context.scope,request.bot_id,current.position,since,request.subject],
            |row| Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut answered_until = 0;
    for (key, position) in answers {
        if super::context::reply_is_valid(db, &key)? {
            answered_until = position;
            break;
        }
    }
    let keys = db
        .prepare(
            "SELECT key FROM messages WHERE conversation_id=?1 AND epoch=?2 AND scope=?3
         AND sender_id=?4 AND role='user' AND revoked=0 AND id<?5 AND id>?6
         AND received_at>=?7 AND received_at<=?8 AND trim(content)!=''
         AND access_revision=?10
         AND COALESCE(origin_plugin,'') NOT IN('control','memory_only')
         AND NOT EXISTS(SELECT 1 FROM ai_revoked_sources WHERE source_key=messages.key)
         ORDER BY id DESC LIMIT ?9",
        )?
        .query_map(
            params![
                request.guard.conversation_id,
                request.guard.epoch,
                context.scope,
                request.subject,
                current.position,
                answered_until,
                since,
                current.received_at,
                QUERY_MESSAGE_LIMIT - 1,
                context.access_revision
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut used = current.text.len();
    let mut sources = Vec::new();
    for key in keys {
        let source = query_source(db, request, context, &key)?;
        let cost = source.text.len().saturating_add(1);
        if used.saturating_add(cost) > crate::embedding::MAX_TEXT_BYTES {
            continue;
        }
        used += cost;
        sources.push(source);
    }
    sources.reverse();
    Ok(sources)
}

fn validate_index(db: &Connection, snapshot: &IndexSnapshot) -> Result<()> {
    ensure!(
        boundary(db, &snapshot.request)? == snapshot.boundary,
        "semantic index context changed"
    );
    let current = document(db, &snapshot.request, snapshot.document.memory.id)?;
    ensure!(
        current.version == snapshot.document.version
            && current.hash == snapshot.document.hash
            && serde_json::to_value(&current.memory)?
                == serde_json::to_value(&snapshot.document.memory)?,
        "semantic index memory changed"
    );
    Ok(())
}

fn validate_query(db: &Connection, snapshot: &QuerySnapshot) -> Result<()> {
    ensure!(
        boundary(db, &snapshot.request)? == snapshot.boundary,
        "semantic query context changed"
    );
    ensure!(
        query_source(
            db,
            &snapshot.request,
            &snapshot.boundary,
            &snapshot.source.key
        )? == snapshot.source,
        "semantic query source changed"
    );
    for source in &snapshot.preceding {
        ensure!(
            query_source(db, &snapshot.request, &snapshot.boundary, &source.key)? == *source,
            "semantic preceding query source changed"
        );
    }
    Ok(())
}

fn reserve(db: &Connection, purpose: &str) -> Result<()> {
    let timestamp = now();
    db.execute(
        "DELETE FROM ai_semantic_admissions WHERE at<=?1",
        [timestamp - 3600],
    )?;
    let count: i64 = db.query_row("SELECT COUNT(*) FROM ai_semantic_admissions", [], |r| {
        r.get(0)
    })?;
    ensure!(
        count < REQUESTS_PER_HOUR,
        "semantic request quota exhausted"
    );
    db.execute(
        "INSERT INTO ai_semantic_admissions(at,purpose) VALUES(?1,?2)",
        params![timestamp, purpose],
    )?;
    Ok(())
}

fn namespace(embedding: &crate::embedding::Embedding) -> String {
    format!("{}:memory-text-v1", embedding.fingerprint())
}

fn failure_reason(error: &anyhow::Error) -> &'static str {
    use crate::embedding::EmbeddingError;
    match error.downcast_ref::<EmbeddingError>() {
        Some(EmbeddingError::Disabled) => "disabled",
        Some(EmbeddingError::Configuration) => "configuration",
        Some(EmbeddingError::Input) => "input",
        Some(EmbeddingError::Cancelled) => "cancelled",
        Some(EmbeddingError::Timeout) => "timeout",
        Some(EmbeddingError::SourceChanged) => "source_changed",
        Some(EmbeddingError::SchedulerClosed) => "scheduler_closed",
        Some(EmbeddingError::Transport) => "transport",
        Some(EmbeddingError::HttpStatus(_)) => "http_status",
        Some(EmbeddingError::ResponseTooLarge) => "response_too_large",
        Some(EmbeddingError::Response) => "response",
        Some(EmbeddingError::Vector) => "vector",
        None => "validation_or_storage",
    }
}

async fn usage(host: &Host, purpose: &'static str, input_tokens: u64) -> Result<()> {
    let model = host.config.embedding.model.clone();
    let tokens = i64::try_from(input_tokens)?;
    host.store.call(move |db| {
        db.execute("INSERT INTO usage_daily(day,plugin_id,purpose,model,requests,input_tokens,output_tokens) VALUES(date('now'),'ai',?1,?2,1,?3,0) ON CONFLICT(day,plugin_id,purpose,model) DO UPDATE SET requests=requests+1,input_tokens=input_tokens+excluded.input_tokens",params![purpose,model,tokens])?;
        Ok(())
    }).await
}

fn pending_id(
    db: &Connection,
    request: &Request,
    namespace: &str,
    dimensions: usize,
) -> Result<Option<i64>> {
    let memory_scope = memory::scope::resolve(db, &request.guard.conversation_id)?;
    Ok(db.query_row(&format!("SELECT m.id FROM ai_memories m LEFT JOIN ai_memory_vectors v ON v.memory_id=m.id AND v.namespace=?4 AND v.memory_version=m.version AND v.dimensions=?5 AND length(v.vector)=?5*4 LEFT JOIN ai_semantic_attempts a ON a.memory_id=m.id AND a.namespace=?4 WHERE m.conversation_id IN (SELECT id FROM ai_conversations WHERE memory_scope=?2) AND m.subject_id=?3 AND {ELIGIBLE} AND v.memory_id IS NULL AND (a.retry_after IS NULL OR a.retry_after<=?1) ORDER BY m.id LIMIT 1"),params![now(),memory_scope,request.subject,namespace,dimensions],|r|r.get(0)).optional()?)
}

fn commit_index(
    db: &mut Connection,
    snapshot: &IndexSnapshot,
    namespace: &str,
    dimensions: usize,
    vector: &[f32],
) -> Result<()> {
    ensure!(
        vector.len() == dimensions,
        "semantic vector dimensions changed"
    );
    let encoded = encode_vector(vector)?;
    let tx = db.transaction()?;
    validate_index(&tx, snapshot)?;
    tx.execute("INSERT INTO ai_memory_vectors(memory_id,namespace,memory_version,document_hash,dimensions,vector,indexed_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(memory_id,namespace) DO UPDATE SET memory_version=excluded.memory_version,document_hash=excluded.document_hash,dimensions=excluded.dimensions,vector=excluded.vector,indexed_at=excluded.indexed_at",params![snapshot.document.memory.id,namespace,snapshot.document.version,snapshot.document.hash,dimensions,encoded,now()])?;
    tx.execute(
        "DELETE FROM ai_semantic_attempts WHERE memory_id=?1 AND namespace=?2",
        params![snapshot.document.memory.id, namespace],
    )?;
    tx.commit()?;
    Ok(())
}

/// Index at most one existing memory of this exact person. All HTTP runs outside
/// SQLite, with a second authorization check after the client's permit wait.
pub async fn index_subject(
    host: &Host,
    guard: &ContextGuard,
    subject: i64,
    cancel: &CancellationToken,
) -> Result<bool> {
    let Some(embedding) = host.embedding.as_ref() else {
        return Ok(false);
    };
    if cancel.is_cancelled() {
        return Ok(false);
    }
    let request = request(host, guard, subject)?;
    let namespace = namespace(embedding);
    let dimensions = embedding.dimensions();
    let ns = namespace.clone();
    let snapshot = host
        .store
        .call(move |db| {
            let boundary = boundary(db, &request)?;
            let Some(id) = pending_id(db, &request, &ns, dimensions)? else {
                return Ok(None);
            };
            let document = document(db, &request, id)?;
            Ok(Some(IndexSnapshot {
                request,
                boundary,
                document,
            }))
        })
        .await?;
    let Some(snapshot) = snapshot else {
        return Ok(false);
    };
    let outbound = snapshot.clone();
    let response = embedding
        .embed_text(&snapshot.document.input, cancel, async {
            ensure!(
                !cancel.is_cancelled()
                    && host.milky.verified_bot_id() == Some(outbound.request.bot_id),
                "semantic index cancelled or account changed"
            );
            host.store
                .call(move |db| {
                    let tx = db.transaction()?;
                    validate_index(&tx, &outbound)?;
                    reserve(&tx, "memory_index")?;
                    tx.commit()?;
                    Ok(())
                })
                .await
        })
        .await;
    if let Ok(response) = &response {
        usage(host, "memory_embedding_index", response.input_tokens).await?;
    }
    let response = match response {
        Ok(response) if !cancel.is_cancelled() => response,
        failed_response => {
            let reason = failed_response
                .as_ref()
                .err()
                .map(failure_reason)
                .unwrap_or("cancelled");
            let failed = snapshot.clone();
            let ns = namespace.clone();
            // A failed/stale source never recreates an index. Backoff only applies
            // if the same memory is still valid, and cannot block another person.
            let _=host.store.call(move |db| {
                if validate_index(db,&failed).is_ok() {
                    db.execute("INSERT INTO ai_semantic_attempts(memory_id,namespace,retry_after) VALUES(?1,?2,?3) ON CONFLICT(memory_id,namespace) DO UPDATE SET retry_after=excluded.retry_after",params![failed.document.memory.id,ns,now()+RETRY_DELAY])?;
                }
                Ok(())
            }).await;
            tracing::warn!(
                stage = "embedding_index",
                status = "failed",
                reason,
                "semantic indexing unavailable; lexical recall remains available"
            );
            return Ok(false);
        }
    };
    ensure!(
        host.milky.verified_bot_id() == Some(snapshot.request.bot_id),
        "semantic account changed"
    );
    let committed = host
        .store
        .call(move |db| commit_index(db, &snapshot, &namespace, dimensions, &response.vector))
        .await;
    if committed.is_err() {
        tracing::warn!(
            stage = "embedding_index_commit",
            status = "rejected",
            reason = "source_or_storage_changed",
            "semantic index result was not committed"
        );
    }
    committed?;
    tracing::info!(
        stage = "embedding_index",
        status = "committed",
        "semantic memory indexed"
    );
    Ok(true)
}

/// Select a bounded pending row from the current account and current scope mode.
/// Each request still contains only one person's one stored memory.
pub async fn index_next(host: &Host, cancel: &CancellationToken) -> Result<bool> {
    let Some(embedding) = host.embedding.as_ref() else {
        return Ok(false);
    };
    let Some(bot_id) = host.milky.verified_bot_id() else {
        return Ok(false);
    };
    if !host.config.plugins.ai.enabled || cancel.is_cancelled() {
        return Ok(false);
    }
    let namespace = namespace(embedding);
    let dimensions = embedding.dimensions();
    let defaults = host.config.plugins.ai.defaults.clone();
    let plugin_epoch = *host
        .epochs
        .get("ai")
        .context("semantic AI plugin is disabled")?;
    let pending=host.store.call(move |db| {
        crate::host::check_plugin(db,"ai",plugin_epoch)?;
        let sql=format!("SELECT DISTINCT c.id,c.epoch,m.subject_id FROM ai_memories m JOIN ai_conversations source ON source.id=m.conversation_id JOIN ai_conversations c ON c.memory_scope=source.memory_scope LEFT JOIN ai_scope_modes sm ON sm.scope=c.scope LEFT JOIN scope_access sa ON sa.scope=c.scope LEFT JOIN scope_plugin_access pa ON pa.scope=c.scope AND pa.plugin='ai' LEFT JOIN ai_memory_vectors v ON v.memory_id=m.id AND v.namespace=?3 AND v.memory_version=m.version AND v.dimensions=?4 AND length(v.vector)=?4*4 LEFT JOIN ai_semantic_attempts a ON a.memory_id=m.id AND a.namespace=?3 WHERE {ELIGIBLE} AND c.scope LIKE ?2 AND m.subject_id>0 AND m.subject_id!=?5 AND (c.owner_id IS NULL OR c.owner_id=m.subject_id) AND COALESCE(sa.enabled,1)=1 AND COALESCE(pa.enabled,1)=1 AND COALESCE(json_extract(c.settings,'$.memory_read'),?6)=1 AND COALESCE(json_extract(c.settings,'$.paused'),?7)=0 AND c.id=c.scope||':v'||CASE WHEN c.scope LIKE '%:friend:%' THEN 0 ELSE COALESCE(sm.revision,0) END||CASE WHEN c.owner_id IS NULL THEN ':group' ELSE ':user:'||c.owner_id END AND c.mode=CASE WHEN c.scope LIKE '%:friend:%' THEN 'user' ELSE COALESCE(sm.mode,?8) END AND v.memory_id IS NULL AND (a.retry_after IS NULL OR a.retry_after<=?1) ORDER BY m.id LIMIT 48");
        let candidates=db.prepare(&sql)?.query_map(params![now(),format!("{bot_id}:%"),namespace,dimensions,bot_id,defaults.memory_read,defaults.paused,defaults.mode],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (id,epoch,subject) in candidates {
            let request=Request {guard:ContextGuard {conversation_id:id,epoch},subject,bot_id,plugin_epoch,defaults:defaults.clone(),once_query:None};
            if boundary(db,&request).is_ok() { return Ok(Some((request.guard,subject))); }
        }
        Ok(None)
    }).await?;
    let Some((guard, subject)) = pending else {
        return Ok(false);
    };
    index_subject(host, &guard, subject, cancel).await
}

fn semantic_candidates(
    db: &Connection,
    snapshot: &QuerySnapshot,
    namespace: &str,
    query: &[f32],
    count: usize,
) -> Result<Vec<Memory>> {
    let sql = format!(
        "SELECT m.id,v.memory_version,v.document_hash,v.vector FROM ai_memory_vectors v JOIN ai_memories m ON m.id=v.memory_id WHERE m.conversation_id IN (SELECT id FROM ai_conversations WHERE memory_scope=?2) AND m.subject_id=?3 AND v.namespace=?4 AND v.dimensions=?5 AND length(v.vector)=?5*4 AND v.memory_version=m.version AND {ELIGIBLE} ORDER BY m.updated_at DESC,m.id DESC LIMIT ?6"
    );
    let mut statement = db.prepare(&sql)?;
    let mut rows = statement.query(params![
        now(),
        snapshot.boundary.memory_scope,
        snapshot.request.subject,
        namespace,
        query.len(),
        SCAN_LIMIT
    ])?;
    let mut best: Vec<(f32, Memory)> = Vec::new();
    while let Some(row) = rows.next()? {
        let id = row.get(0)?;
        let version: i64 = row.get(1)?;
        let hash: String = row.get(2)?;
        let blob: Vec<u8> = row.get(3)?;
        let Ok(vector) = decode_vector(&blob, query.len()) else {
            continue;
        };
        let Some(score) = cosine(query, &vector).filter(|score| *score > 0.0) else {
            continue;
        };
        let current = document(db, &snapshot.request, id)?;
        if current.version != version || current.hash != hash {
            continue;
        }
        best.push((score, current.memory));
        best.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.id.cmp(&a.1.id)));
        best.truncate(count);
    }
    Ok(best.into_iter().map(|(_, memory)| memory).collect())
}

fn fuse(
    lexical: Vec<Memory>,
    semantic: Vec<Memory>,
    limit: usize,
    budget: usize,
    query: &str,
) -> Vec<Memory> {
    let mut found: HashMap<i64, (f64, usize, Memory)> = HashMap::new();
    for (origin, list) in [lexical, semantic].into_iter().enumerate() {
        for (rank, memory) in list.into_iter().enumerate() {
            let value =
                found
                    .entry(memory.id)
                    .or_insert((0.0, origin * CANDIDATE_LIMIT + rank, memory));
            value.0 += 1.0 / (60.0 + rank as f64 + 1.0);
        }
    }
    let mut found: Vec<_> = found.into_values().collect();
    let location_query = memory::requests_personal_location(query);
    found.sort_by(|a, b| {
        let location = |m: &Memory| location_query && memory::is_personal_location(&m.text);
        location(&b.2)
            .cmp(&location(&a.2))
            .then_with(|| b.0.total_cmp(&a.0))
            .then_with(|| a.1.cmp(&b.1))
    });
    let mut used = 0usize;
    let mut result = Vec::new();
    for (_, _, memory) in found {
        let cost = memory::estimate_tokens(&memory.text).saturating_add(16);
        if used.saturating_add(cost) > budget {
            continue;
        }
        if result.len() >= limit {
            break;
        }
        used += cost;
        result.push(memory);
    }
    result
}

fn lexical(
    db: &Connection,
    request: &Request,
    query: &str,
    limit: usize,
    budget: usize,
) -> Result<Vec<Memory>> {
    // Physical forgetting normally deletes the memory and FTS row together.
    // Check tombstones explicitly as well, including retained legacy evidence.
    Ok(
        memory::retrieve(db, &request.guard, request.subject, query, limit, budget)?
            .into_iter()
            .filter(|memory| document(db, request, memory.id).is_ok())
            .collect(),
    )
}

#[allow(clippy::too_many_arguments)]
async fn lexical_fallback(
    host: &Host,
    guard: &ContextGuard,
    subject: i64,
    source_key: &str,
    query: &str,
    limit: usize,
    budget: usize,
    cancel: &CancellationToken,
    once_query: Option<&OnceQueryPermit>,
) -> Result<Vec<Memory>> {
    ensure!(!cancel.is_cancelled(), "semantic recall cancelled");
    let request = query_request(host, guard, subject, source_key, once_query)?;
    let key = source_key.to_owned();
    let query = query.to_owned();
    host.store
        .call(move |db| {
            let boundary = boundary(db, &request)?;
            let source = query_source(db, &request, &boundary, &key)?;
            let preceding = preceding_query_sources(db, &request, &boundary, &source)?;
            let snapshot = QuerySnapshot {
                request,
                boundary,
                source,
                preceding,
            };
            let query = format!("{}\n{query}", snapshot.input());
            lexical(db, &snapshot.request, &query, limit, budget)
        })
        .await
}

/// Preserve lexical behavior whenever semantic service/cache is unavailable.
/// Only authoritative, unanswered text from the current human is embedded.
#[allow(clippy::too_many_arguments)]
pub async fn retrieve(
    host: &Host,
    guard: &ContextGuard,
    subject: i64,
    source_key: &str,
    query: &str,
    limit: usize,
    budget: usize,
    cancel: &CancellationToken,
) -> Result<Vec<Memory>> {
    retrieve_for_turn(
        host, guard, subject, source_key, query, limit, budget, cancel, None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn retrieve_for_turn(
    host: &Host,
    guard: &ContextGuard,
    subject: i64,
    source_key: &str,
    query: &str,
    limit: usize,
    budget: usize,
    cancel: &CancellationToken,
    once_query: Option<&OnceQueryPermit>,
) -> Result<Vec<Memory>> {
    if limit == 0 || budget == 0 {
        return Ok(Vec::new());
    }
    let Some(embedding) = host.embedding.as_ref() else {
        return lexical_fallback(
            host, guard, subject, source_key, query, limit, budget, cancel, once_query,
        )
        .await;
    };
    let namespace = namespace(embedding);
    let dimensions = embedding.dimensions();
    let request = query_request(host, guard, subject, source_key, once_query)?;
    let key = source_key.to_owned();
    let ns = namespace.clone();
    let snapshot=host.store.call(move |db| {
        let boundary=boundary(db,&request)?;
        let source=query_source(db,&request,&boundary,&key)?;
        let preceding=preceding_query_sources(db,&request,&boundary,&source)?;
        let exists:bool=db.query_row(&format!("SELECT EXISTS(SELECT 1 FROM ai_memory_vectors v JOIN ai_memories m ON m.id=v.memory_id WHERE m.conversation_id IN (SELECT id FROM ai_conversations WHERE memory_scope=?2) AND m.subject_id=?3 AND v.namespace=?4 AND v.dimensions=?5 AND length(v.vector)=?5*4 AND v.memory_version=m.version AND {ELIGIBLE})"),params![now(),boundary.memory_scope,subject,ns,dimensions],|r|r.get(0))?;
        Ok(if exists {Some(QuerySnapshot {request,boundary,source,preceding})} else {None})
    }).await?;
    let Some(snapshot) = snapshot else {
        return lexical_fallback(
            host, guard, subject, source_key, query, limit, budget, cancel, once_query,
        )
        .await;
    };
    let outbound = snapshot.clone();
    let input = snapshot.input();
    let response = embedding
        .embed_text(&input, cancel, async {
            ensure!(
                !cancel.is_cancelled()
                    && host.milky.verified_bot_id() == Some(outbound.request.bot_id),
                "semantic query cancelled or account changed"
            );
            host.store
                .call(move |db| {
                    let tx = db.transaction()?;
                    validate_query(&tx, &outbound)?;
                    reserve(&tx, "memory_query")?;
                    tx.commit()?;
                    Ok(())
                })
                .await
        })
        .await;
    if let Ok(response) = &response {
        usage(host, "memory_embedding_query", response.input_tokens).await?;
    }
    let reason = response
        .as_ref()
        .err()
        .map(failure_reason)
        .unwrap_or("source_or_storage_changed");
    if let Ok(response) = response
        && !cancel.is_cancelled()
        && host.milky.verified_bot_id() == Some(snapshot.request.bot_id)
    {
        let query = format!("{input}\n{query}");
        let result = host
            .store
            .call(move |db| {
                validate_query(db, &snapshot)?;
                ensure!(
                    response.vector.len() == dimensions,
                    "semantic query dimensions changed"
                );
                let candidates = limit.saturating_mul(4).min(CANDIDATE_LIMIT);
                let lexical = lexical(db, &snapshot.request, &query, candidates, usize::MAX)?;
                let semantic =
                    semantic_candidates(db, &snapshot, &namespace, &response.vector, candidates)?;
                Ok(fuse(lexical, semantic, limit, budget, &query))
            })
            .await;
        if let Ok(memories) = result {
            tracing::debug!(
                stage = "embedding_query",
                status = "recalled",
                "semantic memory recalled"
            );
            return Ok(memories);
        }
    }
    tracing::warn!(
        stage = "embedding_query",
        status = "fallback",
        reason,
        "semantic recall unavailable; using lexical recall"
    );
    lexical_fallback(
        host, guard, subject, source_key, query, limit, budget, cancel, once_query,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CONVERSATION: &str = "1:group:20:v0:group";

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE plugin_states(id TEXT PRIMARY KEY,epoch INTEGER,enabled INTEGER);
            INSERT INTO plugin_states VALUES('ai',1,1);
            CREATE TABLE scope_access(scope TEXT PRIMARY KEY,enabled INTEGER,revision INTEGER);
            CREATE TABLE scope_plugin_access(scope TEXT,plugin TEXT,enabled INTEGER,revision INTEGER,PRIMARY KEY(scope,plugin));
            CREATE TABLE ai_scope_modes(scope TEXT PRIMARY KEY,mode TEXT,revision INTEGER);
            INSERT INTO ai_scope_modes VALUES('1:group:20','group',0);
            CREATE TABLE ai_conversations(id TEXT PRIMARY KEY,scope TEXT,mode TEXT,owner_id INTEGER,epoch INTEGER,settings TEXT,summary TEXT,compacted_until INTEGER,memory_processed_until INTEGER,memory_revision INTEGER);
            INSERT INTO ai_conversations VALUES('1:group:20:v0:group','1:group:20','group',NULL,1,'{}','原摘要',90,95,0);
            CREATE TABLE ai_memories(id INTEGER PRIMARY KEY AUTOINCREMENT,conversation_id TEXT,subject_id INTEGER,kind TEXT,text TEXT,evidence TEXT,created_at INTEGER,updated_at INTEGER,expires_at INTEGER,origin TEXT,active INTEGER DEFAULT 1,locked INTEGER DEFAULT 0,version INTEGER DEFAULT 1);
            CREATE VIRTUAL TABLE ai_memory_search USING fts5(memory_id UNINDEXED,terms);
            CREATE TABLE ai_memory_provenance(memory_id INTEGER,source_key TEXT,quote TEXT);
            CREATE TABLE ai_revoked_sources(source_key TEXT PRIMARY KEY,revoked_at INTEGER);
            CREATE TABLE messages(id INTEGER PRIMARY KEY,key TEXT,scope TEXT,conversation_id TEXT,epoch INTEGER,sender_id INTEGER,role TEXT,content TEXT,received_at INTEGER,access_revision INTEGER DEFAULT 0,revoked INTEGER DEFAULT 0,origin_plugin TEXT,inbound_position INTEGER);
            CREATE TABLE outbox(scope TEXT,message_seq INTEGER,conversation_id TEXT,context_epoch INTEGER,origin_plugin TEXT,state TEXT);
            CREATE TABLE ai_recent_turns(reply_key TEXT,completed_position INTEGER);
            CREATE TABLE ai_context_invalidated(source_key TEXT PRIMARY KEY,invalidated_at INTEGER);
            INSERT INTO messages VALUES(1,'1:group:20:1','1:group:20','1:group:20:v0:group',1,7,'user','我想喝什么',1,0,0,NULL,1);").unwrap();
        db.execute_batch(MIGRATION).unwrap();
        db.execute_batch(memory::scope::MIGRATION).unwrap();
        db
    }

    fn request_for(subject: i64) -> Request {
        Request {
            guard: ContextGuard {
                conversation_id: CONVERSATION.into(),
                epoch: 1,
            },
            subject,
            bot_id: 1,
            plugin_epoch: 1,
            defaults: AiSettings::default(),
            once_query: None,
        }
    }

    fn add(db: &Connection, subject: i64, text: &str) -> i64 {
        db.execute("INSERT INTO ai_memories(conversation_id,subject_id,kind,text,evidence,created_at,updated_at,origin) VALUES(?1,?2,'event',?3,'[]',1,1,'auto')",params![CONVERSATION,subject,text]).unwrap();
        let id = db.last_insert_rowid();
        db.execute(
            "INSERT INTO ai_memory_search(memory_id,terms) VALUES(?1,?2)",
            params![id, memory::lexical_terms(text).join(" ")],
        )
        .unwrap();
        db.execute(
            "UPDATE ai_conversations SET memory_revision=memory_revision+1",
            [],
        )
        .unwrap();
        id
    }

    fn vector(axis: usize) -> Vec<f32> {
        let mut vector = vec![0.0; 64];
        vector[axis] = 1.0;
        vector
    }

    fn index_snapshot(db: &Connection, id: i64, subject: i64) -> IndexSnapshot {
        let request = request_for(subject);
        IndexSnapshot {
            boundary: boundary(db, &request).unwrap(),
            document: document(db, &request, id).unwrap(),
            request,
        }
    }

    fn query_snapshot(db: &Connection) -> QuerySnapshot {
        snapshot_for_key(db, "1:group:20:1")
    }

    fn snapshot_for_key(db: &Connection, key: &str) -> QuerySnapshot {
        let request = request_for(7);
        let boundary = boundary(db, &request).unwrap();
        let source = query_source(db, &request, &boundary, key).unwrap();
        let preceding = preceding_query_sources(db, &request, &boundary, &source).unwrap();
        QuerySnapshot {
            request,
            boundary,
            source,
            preceding,
        }
    }

    fn query_message(db: &Connection, id: i64, at: i64, text: &str) {
        db.execute("INSERT INTO messages(id,key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position) VALUES(?1,?2,'1:group:20',?3,1,7,'user',?4,?5,?1)",
            params![id,format!("1:group:20:{id}"),CONVERSATION,text,at]).unwrap();
    }

    #[test]
    fn follow_up_query_uses_only_recent_unanswered_sources_from_its_owner() {
        let db = db();
        db.execute(
            "UPDATE messages SET content='我在哪个城市',received_at=100",
            [],
        )
        .unwrap();
        for id in 2..=13 {
            query_message(&db, id, 190, "不可合并的内容");
        }
        db.execute_batch(
            "UPDATE messages SET sender_id=8 WHERE id=2;
            UPDATE messages SET scope='1:friend:7' WHERE id=3;
            UPDATE messages SET conversation_id='other' WHERE id=4;
            UPDATE messages SET epoch=0 WHERE id=5;
            UPDATE messages SET origin_plugin='control' WHERE id=6;
            UPDATE messages SET origin_plugin='memory_only' WHERE id=7;
            UPDATE messages SET revoked=1 WHERE id=8;
            INSERT INTO ai_revoked_sources VALUES('1:group:20:9',1);
            UPDATE messages SET received_at=79 WHERE id=10;
            UPDATE messages SET scope='2:group:20' WHERE id=11;
            UPDATE messages SET received_at=201 WHERE id=12;
            UPDATE messages SET access_revision=1 WHERE id=13;",
        )
        .unwrap();
        query_message(&db, 20, 200, "回答我啊");
        query_message(&db, 21, 200, "水位之后的消息");
        let snapshot = snapshot_for_key(&db, "1:group:20:20");
        assert_eq!(snapshot.input(), "我在哪个城市\n回答我啊");
        assert_eq!(snapshot.preceding.len(), 1);
        validate_query(&db, &snapshot).unwrap();
        for id in 14..=19 {
            query_message(&db, id, 199, &format!("片段{id}"));
        }
        let bounded = snapshot_for_key(&db, "1:group:20:20");
        assert_eq!(bounded.input(), "片段18\n片段19\n回答我啊");
    }

    #[test]
    fn follow_up_query_stops_at_confirmed_own_answers_without_losing_later_questions() {
        for (state, completion, expected) in [
            ("confirmed", 3, "后续问题\n回答我啊"),
            ("submitted", 3, "我想喝什么\n后续问题\n回答我啊"),
            ("confirmed", 4, "我想喝什么\n后续问题\n回答我啊"),
        ] {
            let db = db();
            query_message(&db, 2, 2, "后续问题");
            query_message(&db, 3, 3, "旧问题的回答");
            db.execute(
                "UPDATE messages SET role='assistant',sender_id=1,inbound_position=1 WHERE id=3",
                [],
            )
            .unwrap();
            db.execute(
                "INSERT INTO outbox VALUES('1:group:20',3,?1,1,'ai',?2)",
                params![CONVERSATION, state],
            )
            .unwrap();
            db.execute(
                "INSERT INTO ai_recent_turns VALUES('1:group:20:3',?1)",
                [completion],
            )
            .unwrap();
            query_message(&db, 4, 4, "回答我啊");
            assert_eq!(
                snapshot_for_key(&db, "1:group:20:4").input(),
                expected,
                "{state} / {completion}"
            );
            // A confirmed reply to someone else cannot close this user's question.
            db.execute("UPDATE messages SET sender_id=8 WHERE id=2", [])
                .unwrap();
            db.execute("UPDATE messages SET inbound_position=2 WHERE id=3", [])
                .unwrap();
            assert_eq!(
                snapshot_for_key(&db, "1:group:20:4").input(),
                "我想喝什么\n回答我啊"
            );
        }
    }

    #[test]
    fn follow_up_query_revalidates_every_included_source_before_embedding() {
        for mutation in [
            "UPDATE messages SET content='改写' WHERE id=1",
            "UPDATE messages SET role='system' WHERE id=1",
            "UPDATE messages SET sender_id=8 WHERE id=1",
            "UPDATE messages SET scope='1:friend:7' WHERE id=1",
            "UPDATE messages SET conversation_id='other' WHERE id=1",
            "UPDATE messages SET epoch=0 WHERE id=1",
            "UPDATE messages SET access_revision=1 WHERE id=1",
            "UPDATE messages SET origin_plugin='memory_only' WHERE id=1",
            "UPDATE messages SET revoked=1 WHERE id=1",
            "INSERT INTO ai_revoked_sources VALUES('1:group:20:1',1)",
            "DELETE FROM messages WHERE id=1",
        ] {
            let db = db();
            query_message(&db, 2, 2, "回答我啊");
            let snapshot = snapshot_for_key(&db, "1:group:20:2");
            assert_eq!(snapshot.preceding.len(), 1);
            validate_query(&db, &snapshot).unwrap();
            db.execute(mutation, []).unwrap();
            assert!(validate_query(&db, &snapshot).is_err(), "{mutation}");
        }
    }

    fn index(db: &mut Connection, id: i64, subject: i64, namespace: &str, axis: usize) {
        let snapshot = index_snapshot(db, id, subject);
        commit_index(db, &snapshot, namespace, 64, &vector(axis)).unwrap();
    }

    fn cache_count(db: &Connection) -> i64 {
        db.query_row("SELECT COUNT(*) FROM ai_memory_vectors", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn semantic_recall_recovers_a_nonlexical_match_and_preserves_subject_scope() {
        let mut db = db();
        let own = add(&db, 7, "下午喜欢泡铁观音");
        let other = add(&db, 8, "其他人喜欢乌龙茶");
        index(&mut db, own, 7, "model-a", 0);
        index(&mut db, other, 8, "model-a", 0);
        let query = query_snapshot(&db);
        assert!(
            lexical(&db, &query.request, "热饮选哪一种", 5, 1000)
                .unwrap()
                .is_empty()
        );
        let found = semantic_candidates(&db, &query, "model-a", &vector(0), 20).unwrap();
        assert_eq!(found.iter().map(|m| m.id).collect::<Vec<_>>(), vec![own]);
        assert_eq!(fuse(vec![], found, 5, 1000, "")[0].subject_id, 7);
        let before: (String, i64, i64) = db
            .query_row(
                "SELECT summary,compacted_until,memory_processed_until FROM ai_conversations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(before, ("原摘要".into(), 90, 95));
    }

    #[test]
    fn shared_semantic_memory_crosses_groups_and_private_chats_but_not_people_or_bots() {
        let mut db = db();
        let own = add(&db, 7, "下午喜欢泡铁观音");
        let other = add(&db, 8, "其他人喜欢乌龙茶");
        index(&mut db, own, 7, "model-a", 0);
        index(&mut db, other, 8, "model-a", 0);
        for (scope, mode, owner, bot) in [
            ("1:group:21", "group", None, 1),
            ("1:friend:7", "user", Some(7), 1),
            ("2:group:20", "group", None, 2),
        ] {
            let id = match owner {
                Some(owner) => format!("{scope}:v0:user:{owner}"),
                None => format!("{scope}:v0:group"),
            };
            db.execute("INSERT INTO ai_conversations(id,scope,mode,owner_id,epoch,settings,summary,compacted_until,memory_processed_until,memory_revision) VALUES(?1,?2,?3,?4,1,'{}','',0,0,0)", params![id,scope,mode,owner]).unwrap();
            let key = format!("{scope}:1");
            db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,access_revision,revoked) VALUES(?1,?2,?3,1,7,'user','推荐热饮',1,0,0)",params![key,scope,id]).unwrap();
            let request = Request {
                guard: ContextGuard {
                    conversation_id: id,
                    epoch: 1,
                },
                bot_id: bot,
                ..request_for(7)
            };
            let boundary = boundary(&db, &request).unwrap();
            let source = query_source(&db, &request, &boundary, &key).unwrap();
            let snapshot = QuerySnapshot {
                request,
                boundary,
                source,
                preceding: vec![],
            };
            let found = semantic_candidates(&db, &snapshot, "model-a", &vector(0), 20).unwrap();
            if bot == 1 {
                assert_eq!(found.iter().map(|m| m.id).collect::<Vec<_>>(), vec![own]);
                assert_eq!(
                    pending_id(&db, &snapshot.request, "new-model", 64).unwrap(),
                    Some(own)
                );
                assert!(document(&db, &snapshot.request, other).is_err());
                assert_eq!(
                    lexical(&db, &snapshot.request, "铁观音", 5, 1000).unwrap()[0].id,
                    own
                );
            } else {
                assert!(found.is_empty());
                assert!(document(&db, &snapshot.request, own).is_err());
                assert!(
                    pending_id(&db, &snapshot.request, "new-model", 64)
                        .unwrap()
                        .is_none()
                );
            }
        }
    }

    #[test]
    fn remote_edits_and_sharing_switch_invalidate_semantic_snapshots_without_losing_cache() {
        let mut db = db();
        let own = add(&db, 7, "下午喜欢泡铁观音");
        index(&mut db, own, 7, "model-a", 0);
        db.execute("INSERT INTO ai_conversations(id,scope,mode,owner_id,epoch,settings,summary,compacted_until,memory_processed_until,memory_revision) VALUES('private','1:friend:7','user',7,1,'{}','',0,0,0)", []).unwrap();
        let before = query_snapshot(&db);
        // A write in another conversation must invalidate in-flight query/index work.
        db.execute("INSERT INTO ai_memories(conversation_id,subject_id,kind,text,evidence,created_at,updated_at,origin) VALUES('private',7,'event','来自私聊的新事实','[]',1,1,'auto')", []).unwrap();
        assert!(validate_query(&db, &before).is_err());
        let private_id = db.last_insert_rowid();
        assert!(document(&db, &request_for(7), private_id).is_ok());
        let before = query_snapshot(&db);
        memory::scope::configure(&db, false).unwrap();
        assert!(validate_query(&db, &before).is_err());
        assert!(document(&db, &request_for(7), private_id).is_err());
        assert!(document(&db, &request_for(7), own).is_ok());
        assert_eq!(cache_count(&db), 1);
        memory::scope::configure(&db, true).unwrap();
        assert!(document(&db, &request_for(7), private_id).is_ok());
        assert_eq!(cache_count(&db), 1);
    }

    #[test]
    fn semantic_model_dimension_and_document_versions_are_isolated() {
        let mut db = db();
        let id = add(&db, 7, "下午喜欢泡铁观音");
        index(&mut db, id, 7, "model-a", 0);
        let query = query_snapshot(&db);
        assert!(
            semantic_candidates(&db, &query, "model-b", &vector(0), 5)
                .unwrap()
                .is_empty()
        );
        let mut wrong_dimensions = vec![0.0; 128];
        wrong_dimensions[0] = 1.0;
        assert!(
            semantic_candidates(&db, &query, "model-a", &wrong_dimensions, 5)
                .unwrap()
                .is_empty()
        );
        db.execute(
            "UPDATE ai_memory_vectors SET document_hash='stale-text'",
            [],
        )
        .unwrap();
        assert!(
            semantic_candidates(&db, &query, "model-a", &vector(0), 5)
                .unwrap()
                .is_empty()
        );
        db.execute("UPDATE ai_memory_vectors SET memory_version=0", [])
            .unwrap();
        assert!(
            semantic_candidates(&db, &query, "model-a", &vector(0), 5)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn semantic_invalid_vectors_are_ignored_and_oversized_blobs_are_not_selected() {
        for blob in [
            vec![0u8; 256],
            vec![0u8; 257],
            f32::NAN.to_le_bytes().repeat(64),
        ] {
            let mut db = db();
            let id = add(&db, 7, "下午喜欢泡铁观音");
            index(&mut db, id, 7, "model-a", 0);
            db.execute("UPDATE ai_memory_vectors SET vector=?1", [blob])
                .unwrap();
            assert!(
                semantic_candidates(&db, &query_snapshot(&db), "model-a", &vector(0), 5)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn semantic_memory_edits_disable_expiry_and_delete_invalidate_only_cache_rows() {
        for mutation in [
            "UPDATE ai_memories SET text='变更后的内容'",
            "UPDATE ai_memories SET kind='fact'",
            "UPDATE ai_memories SET active=0",
            "UPDATE ai_memories SET expires_at=1",
            "UPDATE ai_memories SET version=version+1",
            "DELETE FROM ai_memories",
        ] {
            let mut db = db();
            let id = add(&db, 7, "下午喜欢泡铁观音");
            index(&mut db, id, 7, "model-a", 0);
            db.execute(
                "INSERT INTO ai_semantic_attempts VALUES(?1,'model-a',9999999999)",
                [id],
            )
            .unwrap();
            db.execute(mutation, []).unwrap();
            assert_eq!(cache_count(&db), 0, "{mutation}");
            let attempts: i64 = db
                .query_row("SELECT COUNT(*) FROM ai_semantic_attempts", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(attempts, 0);
            db.execute("UPDATE ai_memories SET active=1,expires_at=NULL", [])
                .unwrap();
            assert_eq!(cache_count(&db), 0, "reactivation must rebuild its vector");
        }
    }

    #[test]
    fn semantic_tombstones_reject_cached_vectors_and_lexical_fallback_without_raw_history() {
        let mut db = db();
        let id = add(&db, 7, "下午喜欢泡铁观音");
        db.execute("UPDATE ai_memories SET evidence='[{\"source_key\":\"old-source\",\"quote\":\"铁观音\"}]' WHERE id=?1",[id]).unwrap();
        db.execute(
            "INSERT INTO ai_memory_provenance VALUES(?1,'old-source','铁观音')",
            [id],
        )
        .unwrap();
        index(&mut db, id, 7, "model-a", 0);
        db.execute("INSERT INTO ai_revoked_sources VALUES('old-source',1)", [])
            .unwrap();
        let query = query_snapshot(&db);
        assert!(
            semantic_candidates(&db, &query, "model-a", &vector(0), 5)
                .unwrap()
                .is_empty()
        );
        assert!(
            lexical(&db, &query.request, "铁观音", 5, 1000)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            cache_count(&db),
            1,
            "a leftover cache row still cannot authorize recall"
        );
    }

    #[test]
    fn semantic_index_response_cannot_revive_a_changed_or_revoked_memory() {
        for mutation in [
            "INSERT INTO scope_plugin_access VALUES('1:group:20','ai',0,1)",
            "INSERT INTO scope_plugin_access VALUES('1:group:20','ai',1,2)",
            "UPDATE ai_conversations SET epoch=epoch+1",
            "UPDATE ai_conversations SET memory_revision=memory_revision+1",
            "UPDATE ai_conversations SET settings='{\"memory_read\":false}'",
            "UPDATE ai_conversations SET settings='{\"paused\":true}'",
            "UPDATE ai_scope_modes SET revision=1",
            "INSERT INTO scope_access VALUES('1:group:20',0,1)",
            "UPDATE plugin_states SET epoch=2",
            "UPDATE ai_memories SET text='改写后的内容'",
            "UPDATE ai_memories SET active=0",
            "UPDATE ai_memories SET expires_at=1",
            "INSERT INTO ai_revoked_sources VALUES('old-source',1)",
            "DELETE FROM ai_memories",
        ] {
            let mut db = db();
            let id = add(&db, 7, "下午喜欢泡铁观音");
            db.execute(
                "INSERT INTO ai_memory_provenance VALUES(?1,'old-source','铁观音')",
                [id],
            )
            .unwrap();
            let snapshot = index_snapshot(&db, id, 7);
            db.execute(mutation, []).unwrap();
            assert!(validate_index(&db, &snapshot).is_err(), "{mutation}");
            assert!(
                commit_index(&mut db, &snapshot, "model-a", 64, &vector(0)).is_err(),
                "{mutation}"
            );
            assert_eq!(cache_count(&db), 0);
        }
    }

    #[test]
    fn semantic_query_preflight_revalidates_human_source_after_waiting() {
        for mutation in [
            "INSERT INTO scope_plugin_access VALUES('1:group:20','ai',0,1)",
            "INSERT INTO scope_plugin_access VALUES('1:group:20','ai',1,2)",
            "UPDATE messages SET content='新的原文'",
            "UPDATE messages SET sender_id=8",
            "UPDATE messages SET epoch=0",
            "UPDATE messages SET access_revision=1",
            "INSERT INTO ai_revoked_sources VALUES('1:group:20:1',1)",
            "DELETE FROM messages",
            "UPDATE ai_conversations SET memory_revision=memory_revision+1",
        ] {
            let db = db();
            let snapshot = query_snapshot(&db);
            validate_query(&db, &snapshot).unwrap();
            db.execute(mutation, []).unwrap();
            assert!(validate_query(&db, &snapshot).is_err(), "{mutation}");
            let admissions: i64 = db
                .query_row("SELECT COUNT(*) FROM ai_semantic_admissions", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(admissions, 0);
        }
    }

    #[test]
    fn semantic_fusion_deduplicates_and_honors_original_count_and_token_budgets() {
        let db = db();
        let first = add(&db, 7, "喜欢绿茶");
        let second = add(&db, 7, "喜欢红茶");
        let third = add(&db, 7, "喜欢乌龙茶");
        let a = memory::get_memory(&db, first).unwrap().unwrap();
        let b = memory::get_memory(&db, second).unwrap().unwrap();
        let c = memory::get_memory(&db, third).unwrap().unwrap();
        let fused = fuse(vec![a.clone(), b.clone()], vec![a.clone(), c], 2, 1000, "");
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].id, first);
        assert_ne!(fused[0].id, fused[1].id);
        let budget = memory::estimate_tokens(&a.text) + 16;
        assert_eq!(
            fuse(vec![a.clone(), b.clone()], vec![a.clone()], 5, budget, "").len(),
            1
        );
        let mut home = b.clone();
        home.text = "居住在星港市".into();
        assert_eq!(
            fuse(
                vec![home.clone(), a.clone()],
                vec![a.clone()],
                1,
                1000,
                "聊点别的"
            )[0]
            .id,
            a.id
        );
        assert_eq!(
            fuse(
                vec![home.clone(), a.clone()],
                vec![a],
                1,
                1000,
                "我家天气怎么样"
            )[0]
            .id,
            home.id,
            "a lexical home fact must survive fusion with a preference found by both retrievers"
        );
        assert!(fuse(vec![b], vec![], 5, 1, "我家天气").is_empty());
    }

    #[test]
    fn semantic_request_quota_is_atomic_separate_and_failed_attempts_still_count() {
        let mut db = db();
        db.execute_batch("CREATE TABLE model_admissions(id INTEGER PRIMARY KEY,at INTEGER); INSERT INTO model_admissions VALUES(1,1);").unwrap();
        for _ in 0..REQUESTS_PER_HOUR {
            let tx = db.transaction().unwrap();
            reserve(&tx, "memory_index").unwrap();
            tx.commit().unwrap();
        }
        let tx = db.transaction().unwrap();
        assert!(reserve(&tx, "memory_query").is_err());
        tx.rollback().unwrap();
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM ai_semantic_admissions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, REQUESTS_PER_HOUR);
        let normal: i64 = db
            .query_row("SELECT COUNT(*) FROM model_admissions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(normal, 1);
        db.execute("UPDATE ai_semantic_admissions SET at=?1", [now() - 3601])
            .unwrap();
        reserve(&db, "memory_query").unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM ai_semantic_admissions", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn semantic_failure_backoff_leaves_other_memories_available() {
        let db = db();
        let first = add(&db, 7, "第一条记忆");
        let second = add(&db, 7, "第二条记忆");
        db.execute(
            "INSERT INTO ai_semantic_attempts VALUES(?1,'model-a',?2)",
            params![first, now() + 300],
        )
        .unwrap();
        assert_eq!(
            pending_id(&db, &request_for(7), "model-a", 64).unwrap(),
            Some(second)
        );
        db.execute("UPDATE ai_semantic_attempts SET retry_after=0", [])
            .unwrap();
        assert_eq!(
            pending_id(&db, &request_for(7), "model-a", 64).unwrap(),
            Some(first)
        );
    }

    #[test]
    fn semantic_vector_scan_and_ranked_candidates_have_fixed_bounds() {
        let mut db = db();
        for number in 0..=SCAN_LIMIT {
            add(&db, 7, &format!("事件 {number}"));
        }
        for id in 1..=(SCAN_LIMIT as i64 + 1) {
            index(&mut db, id, 7, "model-a", if id == 1 { 0 } else { 1 });
        }
        let query = query_snapshot(&db);
        assert!(
            semantic_candidates(&db, &query, "model-a", &vector(0), CANDIDATE_LIMIT)
                .unwrap()
                .is_empty(),
            "the oldest row is outside the fixed latest-vector scan window"
        );
        assert_eq!(
            semantic_candidates(&db, &query, "model-a", &vector(1), CANDIDATE_LIMIT)
                .unwrap()
                .len(),
            CANDIDATE_LIMIT
        );
    }

    #[test]
    fn semantic_cached_manual_memory_does_not_require_retained_original_messages() {
        let mut db = db();
        let id = add(&db, 7, "管理员补充的稳定事实");
        db.execute(
            "UPDATE ai_memories SET origin='manual',locked=1,evidence='[]' WHERE id=?1",
            [id],
        )
        .unwrap();
        index(&mut db, id, 7, "model-a", 0);
        let found =
            semantic_candidates(&db, &query_snapshot(&db), "model-a", &vector(0), 5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].origin, "manual");
        assert_eq!(
            serde_json::to_value(&found[0]).unwrap()["evidence"],
            json!([])
        );
    }

    #[tokio::test]
    async fn semantic_existing_memories_index_one_person_per_request_and_http_failure_uses_fts() {
        use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use tokio::sync::mpsc;
        #[derive(Clone)]
        struct Mock {
            fail: Arc<AtomicBool>,
            sent: mpsc::Sender<serde_json::Value>,
        }
        async fn embed(
            State(mock): State<Mock>,
            Json(body): Json<serde_json::Value>,
        ) -> (StatusCode, Json<serde_json::Value>) {
            mock.sent.send(body).await.unwrap();
            if mock.fail.load(Ordering::SeqCst) {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error":"synthetic temporary failure"})),
                )
            } else {
                (
                    StatusCode::OK,
                    Json(
                        json!({"object":"list","model":"Qwen/Qwen3-VL-Embedding-8B","data":[{"object":"embedding","index":0,"embedding":vector(0)}],"usage":{"prompt_tokens":3,"total_tokens":3}}),
                    ),
                )
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (sent, mut requests) = mpsc::channel(8);
        let fail = Arc::new(AtomicBool::new(false));
        let app = Router::new()
            .route(
                "/api/get_login_info",
                post(|| async { Json(json!({"status":"ok","retcode":0,"data":{"uin":1}})) }),
            )
            .route("/embeddings", post(embed))
            .with_state(Mock {
                fail: fail.clone(),
                sent,
            });
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.app.database = temp
            .path()
            .join("semantic.sqlite3")
            .to_string_lossy()
            .into_owned();
        config.milky.url = base.clone();
        config.milky.bot_id = Some(1);
        config.llm.api_key = "synthetic".into();
        config.llm.chat_completions_url = format!("{base}/chat/completions");
        config.llm.model = "synthetic".into();
        config.plugins.native.enabled.clear();
        config.embedding = crate::embedding::EmbeddingConfig {
            enabled: true,
            api_key: "synthetic".into(),
            embeddings_url: format!("{base}/embeddings"),
            dimensions: 64,
            ..Default::default()
        };
        let mut host = Host::create(config).await.unwrap();
        host.milky.login().await.unwrap();
        let (own,other)=host.store.call(|db| {
            db.execute("INSERT INTO ai_scope_modes VALUES('1:group:20','group',0)",[])?;
            db.execute("INSERT INTO ai_conversations(id,scope,mode,epoch,summary,compacted_until,memory_processed_until) VALUES(?1,'1:group:20','group',1,'原摘要',90,95)",[CONVERSATION])?;
            db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,learnable) VALUES('1:group:20:1','1:group:20',?1,1,7,'user','我想喝什么',1,1,1)",[CONVERSATION])?;
            Ok((add(db,7,"下午喜欢泡铁观音"),add(db,8,"另一个人常喝红茶")))
        }).await.unwrap();
        let cancel = CancellationToken::new();
        host.store.call(|db| {
            db.execute("INSERT INTO scope_plugin_access(scope,plugin,enabled,revision) VALUES('1:group:20','ai',0,1)",[])?;
            Ok(())
        }).await.unwrap();
        assert!(!index_next(&host, &cancel).await.unwrap());
        assert!(
            requests.try_recv().is_err(),
            "a disabled group must not send memory text for indexing"
        );
        host.store.call(|db| {
            db.execute("UPDATE scope_plugin_access SET enabled=1,revision=2 WHERE scope='1:group:20' AND plugin='ai'",[])?;
            Ok(())
        }).await.unwrap();
        assert!(index_next(&host, &cancel).await.unwrap());
        assert!(index_next(&host, &cancel).await.unwrap());
        assert!(!index_next(&host, &cancel).await.unwrap());
        let first = requests.recv().await.unwrap();
        let second = requests.recv().await.unwrap();
        assert!(first["input"]["text"].as_str().unwrap().contains("铁观音"));
        assert!(!first.to_string().contains("红茶"));
        assert!(second["input"]["text"].as_str().unwrap().contains("红茶"));
        assert!(!second.to_string().contains("铁观音"));
        assert!(requests.try_recv().is_err());
        let guard = ContextGuard {
            conversation_id: CONVERSATION.into(),
            epoch: 1,
        };
        let semantic = retrieve(
            &host,
            &guard,
            7,
            "1:group:20:1",
            "无共同关键词",
            5,
            1000,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(semantic.iter().map(|m| m.id).collect::<Vec<_>>(), vec![own]);
        assert_eq!(
            requests.recv().await.unwrap()["input"]["text"],
            "我想喝什么"
        );
        fail.store(true, Ordering::SeqCst);
        let recalled = retrieve(&host, &guard, 7, "1:group:20:1", "铁观音", 5, 1000, &cancel)
            .await
            .unwrap();
        assert_eq!(recalled.iter().map(|m| m.id).collect::<Vec<_>>(), vec![own]);
        assert!(!recalled.iter().any(|m| m.id == other));
        assert_eq!(
            requests.recv().await.unwrap()["input"]["text"],
            "我想喝什么"
        );
        let counts=host.store.call(|db|Ok((cache_count(db),db.query_row("SELECT COUNT(*) FROM ai_semantic_admissions",[],|r|r.get::<_,i64>(0))?,db.query_row("SELECT summary,compacted_until,memory_processed_until FROM ai_conversations WHERE id=?1",[CONVERSATION],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?)))?))).await.unwrap();
        assert_eq!(counts, (2, 4, ("原摘要".into(), 90, 95)));
        let usage=host.store.call(|db|{
            Ok(db.prepare("SELECT purpose,requests,input_tokens,output_tokens FROM usage_daily WHERE plugin_id='ai' ORDER BY purpose")?.query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,i64>(2)?,row.get::<_,i64>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
        }).await.unwrap();
        assert_eq!(
            usage,
            vec![
                ("memory_embedding_index".into(), 2, 6, 0),
                ("memory_embedding_query".into(), 1, 3, 0)
            ]
        );
        // A short follow-up must retain the unresolved question as recall input.
        host.store.call(|db| {
            db.execute("UPDATE messages SET content='我在哪个城市' WHERE key='1:group:20:1'",[])?;
            db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,inbound_position,learnable) VALUES('1:group:20:2','1:group:20',?1,1,7,'user','回答我啊',2,2,1)",[CONVERSATION])?;
            Ok(())
        }).await.unwrap();
        fail.store(false, Ordering::SeqCst);
        let recalled = retrieve(
            &host,
            &guard,
            7,
            "1:group:20:2",
            "回答我啊",
            5,
            1000,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(recalled.iter().map(|m| m.id).collect::<Vec<_>>(), vec![own]);
        assert_eq!(
            requests.recv().await.unwrap()["input"]["text"],
            "我在哪个城市\n回答我啊"
        );
        // The same pending question must work when embeddings are disabled.
        let embedding = Arc::get_mut(&mut host)
            .expect("sole test host")
            .embedding
            .take()
            .unwrap();
        let city = host
            .store
            .call(|db| {
                add(db, 8, "其他用户所在城市为他乡");
                Ok(add(db, 7, "人物背景：所在城市为测试城"))
            })
            .await
            .unwrap();
        let recalled = retrieve(
            &host,
            &guard,
            7,
            "1:group:20:2",
            "回答我啊",
            5,
            1000,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(
            recalled.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![city]
        );
        assert!(requests.try_recv().is_err());
        assert!(
            retrieve(&host, &guard, 7, "unused", "", 0, 1000, &cancel)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            retrieve(&host, &guard, 7, "unused", "", 5, 0, &cancel)
                .await
                .unwrap()
                .is_empty()
        );
        // Admission captures /ai once before the persisted permit is consumed.
        // Context preparation must retain that exact turn's lexical recall, but
        // a paused query or background index without the permit still fails.
        let scope = crate::types::Scope {
            bot_id: 1,
            scene: crate::types::Scene::Group,
            peer_id: 20,
        };
        let mut admitted = super::super::Conversation {
            guard: guard.clone(),
            scope: scope.clone(),
            owner_id: None,
            settings: AiSettings {
                paused: true,
                once: true,
                ..AiSettings::default()
            },
        };
        let incoming: crate::types::Incoming = serde_json::from_value(json!({
            "key":{"scope":{"bot_id":1,"scene":"group","peer_id":20},"message_seq":2},
            "sender_id":7,"text":"回答我啊","mentions":[],"quote_seq":null,"received_at":2
        }))
        .unwrap();
        let once = OnceQueryPermit::capture(&admitted, &incoming, false).unwrap();
        assert!(OnceQueryPermit::capture(&admitted, &incoming, true).is_none());
        admitted.settings.once = false;
        assert!(OnceQueryPermit::capture(&admitted, &incoming, false).is_none());
        host.store.call(|db| {
            db.execute("UPDATE ai_conversations SET settings='{\"paused\":true,\"once\":false}' WHERE id=?1",[CONVERSATION])?;
            assert!(boundary(db, &request_for(7)).is_err(), "background/index request has no once permit");
            Ok(())
        }).await.unwrap();
        assert!(
            retrieve(
                &host,
                &guard,
                7,
                "1:group:20:2",
                "回答我啊",
                5,
                1000,
                &cancel
            )
            .await
            .is_err()
        );
        let context_request = |permit| super::super::context::ContextRequest {
            guard: &guard,
            scope: &scope,
            source_key: "1:group:20:2",
            actor: 7,
            position: 2,
            received_at: 2,
            current: "回答我啊",
            settings: &admitted.settings,
            once_query: permit,
            allow_continuity: false,
        };
        assert!(
            super::super::context::prepare(&host, context_request(None), &cancel)
                .await
                .is_err()
        );
        let prepared = super::super::context::prepare(&host, context_request(Some(&once)), &cancel)
            .await
            .unwrap();
        assert!(
            prepared
                .memory_texts()
                .iter()
                .any(|text| text.contains(&format!("记忆 #{city} /")))
        );
        // The permit authorizes this exact query, independent of whether the
        // embedding service is enabled or succeeds. It never grants indexing.
        Arc::get_mut(&mut host).expect("sole test host").embedding = Some(embedding);
        assert!(
            super::super::context::prepare(&host, context_request(None), &cancel)
                .await
                .is_err()
        );
        assert!(index_subject(&host, &guard, 7, &cancel).await.is_err());
        assert!(!index_next(&host, &cancel).await.unwrap());
        assert!(requests.try_recv().is_err());
        let prepared = super::super::context::prepare(&host, context_request(Some(&once)), &cancel)
            .await
            .unwrap();
        assert!(
            prepared
                .memory_texts()
                .iter()
                .any(|text| text.contains(&format!("记忆 #{city} /")))
        );
        assert_eq!(
            requests.recv().await.unwrap()["input"]["text"],
            "我在哪个城市\n回答我啊"
        );
        fail.store(true, Ordering::SeqCst);
        let fallback = super::super::context::prepare(&host, context_request(Some(&once)), &cancel)
            .await
            .unwrap();
        assert!(
            fallback
                .memory_texts()
                .iter()
                .any(|text| text.contains(&format!("记忆 #{city} /")))
        );
        assert_eq!(
            requests.recv().await.unwrap()["input"]["text"],
            "我在哪个城市\n回答我啊"
        );
        fail.store(false, Ordering::SeqCst);
        for (subject, key) in [(8, "1:group:20:2"), (7, "1:group:20:1")] {
            assert!(
                retrieve_for_turn(
                    &host,
                    &guard,
                    subject,
                    key,
                    "城市",
                    5,
                    1000,
                    &cancel,
                    Some(&once)
                )
                .await
                .is_err()
            );
        }
        let stale = ContextGuard {
            epoch: guard.epoch + 1,
            ..guard.clone()
        };
        assert!(
            retrieve_for_turn(
                &host,
                &stale,
                7,
                "1:group:20:2",
                "城市",
                5,
                1000,
                &cancel,
                Some(&once)
            )
            .await
            .is_err()
        );
        host.store.call(|db| {
            db.execute("UPDATE ai_conversations SET settings='{\"paused\":true,\"once\":false,\"memory_read\":false}' WHERE id=?1",[CONVERSATION])?;
            Ok(())
        }).await.unwrap();
        assert!(
            retrieve_for_turn(
                &host,
                &guard,
                7,
                "1:group:20:2",
                "城市",
                5,
                1000,
                &cancel,
                Some(&once)
            )
            .await
            .is_err()
        );
        host.store
            .call(|db| {
                db.execute(
                    "UPDATE ai_conversations SET settings='{}' WHERE id=?1",
                    [CONVERSATION],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        Arc::get_mut(&mut host).expect("sole test host").embedding = None;
        host.store
            .call(|db| {
                db.execute(
                    "INSERT INTO ai_revoked_sources VALUES('1:group:20:1',1)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            retrieve(
                &host,
                &guard,
                7,
                "1:group:20:2",
                "回答我啊",
                5,
                1000,
                &cancel
            )
            .await
            .unwrap()
            .is_empty()
        );
        host.store
            .call(|db| {
                db.execute("UPDATE messages SET revoked=1 WHERE key='1:group:20:2'", [])?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            retrieve(
                &host,
                &guard,
                7,
                "1:group:20:2",
                "回答我啊",
                5,
                1000,
                &cancel
            )
            .await
            .is_err()
        );
        host.cancel.cancel();
        host.store.stop().await.unwrap();
        server.abort();
    }
}
