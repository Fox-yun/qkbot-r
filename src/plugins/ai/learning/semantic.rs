//! Rebuildable vector cache for source-backed learning notes. The parent tables
//! remain authoritative; neither embeddings nor cache failures change notes.
use super::Note;
use crate::{
    config::AiSettings,
    embedding::{Embedding, EmbeddingError, cosine, decode_vector, encode_vector},
    host::Host,
    store::Store,
    types::{ContextGuard, Scene, Scope, now},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};
use tokio_util::sync::CancellationToken;

const CANDIDATE_LIMIT: usize = 128;
const REQUESTS_PER_HOUR: i64 = 120;
const INDEX_RETRY: i64 = 300;
const QUERY_RETRY: i64 = 30;
const MIGRATION: &str = r#"
CREATE TABLE ai_learning_vectors(
 note_id INTEGER NOT NULL,namespace TEXT NOT NULL,note_version INTEGER NOT NULL,
 document_hash TEXT NOT NULL,dimensions INTEGER NOT NULL,vector BLOB NOT NULL,
 indexed_at INTEGER NOT NULL,PRIMARY KEY(note_id,namespace));
CREATE TABLE ai_learning_semantic_attempts(
 note_id INTEGER NOT NULL,namespace TEXT NOT NULL,retry_after INTEGER NOT NULL,
 PRIMARY KEY(note_id,namespace));
CREATE TABLE ai_learning_semantic_admissions(id INTEGER PRIMARY KEY,at INTEGER NOT NULL,purpose TEXT NOT NULL);
CREATE INDEX ai_learning_semantic_admission_time ON ai_learning_semantic_admissions(at);
CREATE TABLE ai_learning_semantic_state(namespace TEXT PRIMARY KEY,query_retry_after INTEGER NOT NULL DEFAULT 0);
CREATE TRIGGER ai_learning_vector_note_deleted AFTER DELETE ON ai_learning_notes BEGIN
 DELETE FROM ai_learning_vectors WHERE note_id=OLD.id;
 DELETE FROM ai_learning_semantic_attempts WHERE note_id=OLD.id;
END;
CREATE TRIGGER ai_learning_vector_note_changed AFTER UPDATE ON ai_learning_notes BEGIN
 DELETE FROM ai_learning_vectors WHERE note_id=OLD.id;
 DELETE FROM ai_learning_semantic_attempts WHERE note_id=OLD.id;
END;
CREATE TRIGGER ai_learning_vector_source_added AFTER INSERT ON ai_learning_sources BEGIN
 DELETE FROM ai_learning_vectors WHERE note_id=NEW.note_id;
 DELETE FROM ai_learning_semantic_attempts WHERE note_id=NEW.note_id;
END;
CREATE TRIGGER ai_learning_vector_source_changed AFTER UPDATE ON ai_learning_sources BEGIN
 DELETE FROM ai_learning_vectors WHERE note_id IN (OLD.note_id,NEW.note_id);
 DELETE FROM ai_learning_semantic_attempts WHERE note_id IN (OLD.note_id,NEW.note_id);
END;
CREATE TRIGGER ai_learning_vector_source_deleted AFTER DELETE ON ai_learning_sources BEGIN
 DELETE FROM ai_learning_vectors WHERE note_id=OLD.note_id;
 DELETE FROM ai_learning_semantic_attempts WHERE note_id=OLD.note_id;
END;
CREATE TRIGGER ai_learning_vector_source_revoked AFTER INSERT ON ai_revoked_sources BEGIN
 DELETE FROM ai_learning_vectors WHERE note_id IN (SELECT note_id FROM ai_learning_sources WHERE source_key=NEW.source_key);
 DELETE FROM ai_learning_semantic_attempts WHERE note_id IN (SELECT note_id FROM ai_learning_sources WHERE source_key=NEW.source_key);
END;
"#;

pub(super) async fn init(store: &Store) -> Result<()> {
    store.migrate("ai_learning_semantic", 1, MIGRATION).await
}

#[derive(Clone)]
struct Request {
    bot: i64,
    plugin_epoch: i64,
    defaults: AiSettings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Policy {
    scope: String,
    access_revision: i64,
    plugin_revision: i64,
    mode: String,
    mode_revision: i64,
    conversations: Vec<(String, i64, String)>,
}

#[derive(Clone)]
struct Document {
    id: i64,
    version: i64,
    hash: String,
    proof_hash: String,
    input: String,
    policies: Vec<Policy>,
}

#[derive(Clone)]
struct IndexSnapshot {
    request: Request,
    revision: i64,
    document: Document,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct QuerySource {
    id: i64,
    key: String,
    text: String,
    received_at: i64,
    access_revision: i64,
}

#[derive(Clone)]
struct QuerySnapshot {
    request: Request,
    guard: ContextGuard,
    scope: Scope,
    actor: i64,
    policy: Policy,
    source: QuerySource,
}

#[derive(Clone)]
struct Candidate {
    snapshot: IndexSnapshot,
    vector: Vec<f32>,
}

fn namespace(embedding: &Embedding) -> String {
    format!("{}:learning-note-text-v1", embedding.fingerprint())
}

fn enabled(host: &Host) -> bool {
    host.config.plugins.ai.enabled
        && host.config.plugins.ai.learning.enabled
        && host.config.plugins.ai.learning.semantic_recall
        && host.config.embedding.enabled
        && host.embedding.is_some()
        && !host.cancel.is_cancelled()
}

fn request(host: &Host) -> Result<Request> {
    ensure!(
        host.config.plugins.ai.enabled
            && host.config.plugins.ai.learning.enabled
            && !host.cancel.is_cancelled(),
        "learning recall is disabled"
    );
    Ok(Request {
        bot: host
            .milky
            .verified_bot_id()
            .context("learning account is unverified")?,
        plugin_epoch: *host.epochs.get("ai").context("learning AI is disabled")?,
        defaults: host.config.plugins.ai.defaults.clone(),
    })
}

fn state(db: &Connection, request: &Request) -> Result<i64> {
    crate::host::check_plugin(db, "ai", request.plugin_epoch)?;
    db.query_row(
        "SELECT revision FROM ai_learning_state WHERE bot_id=?1 AND enabled=1",
        [request.bot],
        |r| r.get(0),
    )
    .optional()?
    .context("learning is disabled for this account")
}

fn policy(
    db: &Connection,
    request: &Request,
    scope: &str,
    conversations: &BTreeSet<String>,
) -> Result<Policy> {
    super::parse_group(scope, request.bot)?;
    let access_revision = crate::access::require_enabled(db, scope)?;
    let plugin_revision = crate::catalog::allowed_revision(db, scope, "ai")?
        .context("learning AI is disabled for this group")?;
    let (mode, mode_revision) = db
        .query_row(
            "SELECT mode,revision FROM ai_scope_modes WHERE scope=?1",
            [scope],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?
        .unwrap_or((request.defaults.mode.clone(), 0));
    let mut contexts = Vec::new();
    for id in conversations {
        let (actual_scope, epoch, settings): (String, i64, String) = db.query_row(
            "SELECT scope,epoch,settings FROM ai_conversations WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        ensure!(actual_scope == scope, "learning source group changed");
        contexts.push((id.clone(), epoch, settings));
    }
    Ok(Policy {
        scope: scope.into(),
        access_revision,
        plugin_revision,
        mode,
        mode_revision,
        conversations: contexts,
    })
}

fn note(db: &Connection, request: &Request, id: i64) -> Result<Note> {
    let mut note = db.query_row("SELECT id,version,kind,text,scope,locked,deleted,invalidated,updated_at FROM ai_learning_notes WHERE id=?1 AND bot_id=?2 AND deleted=0 AND invalidated=0", params![id,request.bot], |r|Ok(Note {id:r.get(0)?,version:r.get(1)?,kind:r.get(2)?,text:r.get(3)?,scope:r.get(4)?,locked:r.get(5)?,deleted:r.get(6)?,invalidated:r.get(7)?,updated_at:r.get(8)?,sources:Vec::new()})).optional()?.context("learning note is no longer eligible")?;
    note.sources = super::sources(db, id)?;
    super::valid_note(db, &note, request.bot, &request.defaults)?;
    Ok(note)
}

fn document(db: &Connection, request: &Request, id: i64) -> Result<Document> {
    let note = note(db, request, id)?;
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if note.scope != "shared" {
        groups.entry(note.scope.clone()).or_default();
    }
    for source in &note.sources {
        groups
            .entry(source.scope.clone())
            .or_default()
            .insert(source.conversation_id.clone());
    }
    let policies = groups
        .into_iter()
        .map(|(scope, conversations)| policy(db, request, &scope, &conversations))
        .collect::<Result<Vec<_>>>()?;
    let input = format!("{}: {}", note.kind, note.text);
    Ok(Document {
        id: note.id,
        version: note.version,
        hash: super::hash(&input),
        proof_hash: super::hash(&serde_json::to_string(&note)?),
        input,
        policies,
    })
}

fn validate_index(db: &Connection, snapshot: &IndexSnapshot) -> Result<()> {
    ensure!(
        state(db, &snapshot.request)? == snapshot.revision,
        "learning note collection changed"
    );
    let current = document(db, &snapshot.request, snapshot.document.id)?;
    ensure!(
        current.version == snapshot.document.version
            && current.hash == snapshot.document.hash
            && current.proof_hash == snapshot.document.proof_hash
            && current.policies == snapshot.document.policies,
        "learning index source or permissions changed"
    );
    Ok(())
}

fn query_snapshot(
    db: &Connection,
    request: Request,
    guard: ContextGuard,
    scope: Scope,
    key: &str,
    actor: i64,
) -> Result<QuerySnapshot> {
    state(db, &request)?;
    ensure!(
        scope.scene == Scene::Group
            && scope.bot_id == request.bot
            && actor > 0
            && actor != request.bot,
        "learning query must be a current human group message"
    );
    crate::host::check_guard(db, &guard)?;
    let policy = policy(
        db,
        &request,
        &scope.key(),
        &BTreeSet::from([guard.conversation_id.clone()]),
    )?;
    let (mode, owner, settings): (String, Option<i64>, String) = db.query_row(
        "SELECT mode,owner_id,settings FROM ai_conversations WHERE id=?1",
        [&guard.conversation_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let expected = match (policy.mode.as_str(), owner) {
        ("group", None) => format!("{}:v{}:group", scope.key(), policy.mode_revision),
        ("user", Some(owner)) if owner == actor => {
            format!("{}:v{}:user:{owner}", scope.key(), policy.mode_revision)
        }
        _ => anyhow::bail!("learning query conversation mode changed"),
    };
    ensure!(
        guard.conversation_id == expected && mode == policy.mode,
        "learning query conversation changed"
    );
    let settings = super::super::resolve_settings(&request.defaults, &settings)?;
    ensure!(
        settings.memory_read && !settings.paused,
        "learning reads are disabled for this conversation"
    );
    let source = db.query_row("SELECT id,key,content,received_at,access_revision FROM messages WHERE key=?1 AND conversation_id=?2 AND epoch=?3 AND scope=?4 AND sender_id=?5 AND role='user' AND revoked=0 AND COALESCE(origin_plugin,'') IN ('','memory_only') AND NOT EXISTS(SELECT 1 FROM ai_revoked_sources WHERE source_key=?1)",params![key,guard.conversation_id,guard.epoch,scope.key(),actor],|r|Ok(QuerySource {id:r.get(0)?,key:r.get(1)?,text:r.get(2)?,received_at:r.get(3)?,access_revision:r.get(4)?})).optional()?.context("learning query source is unavailable")?;
    ensure!(
        source.access_revision == policy.access_revision,
        "learning query access changed"
    );
    Ok(QuerySnapshot {
        request,
        guard,
        scope,
        actor,
        policy,
        source,
    })
}

fn validate_query(db: &Connection, snapshot: &QuerySnapshot) -> Result<()> {
    let current = query_snapshot(
        db,
        snapshot.request.clone(),
        snapshot.guard.clone(),
        snapshot.scope.clone(),
        &snapshot.source.key,
        snapshot.actor,
    )?;
    ensure!(
        current.policy == snapshot.policy && current.source == snapshot.source,
        "learning query source or permissions changed"
    );
    Ok(())
}

fn account(host: &Host, bot: i64, generation: u64, cancel: &CancellationToken) -> Result<()> {
    ensure!(
        !cancel.is_cancelled()
            && !host.cancel.is_cancelled()
            && host.milky.verified_bot_id() == Some(bot),
        "learning embedding cancelled or account changed"
    );
    host.milky.check_generation(generation)
}

/// Also protects the parent's non-vector fallback. This intentionally has no
/// embedding/semantic-switch dependency and never requires query learnability.
pub(super) async fn check_recall(
    host: &Host,
    guard: &ContextGuard,
    scope: &Scope,
    source_key: &str,
    actor: i64,
    cancel: &CancellationToken,
) -> Result<()> {
    let request = request(host)?;
    ensure!(
        scope.scene == Scene::Group && scope.bot_id == request.bot,
        "learning recall is group-only"
    );
    let generation = host.milky.scope_generation(scope)?;
    account(host, request.bot, generation, cancel)?;
    let bot = request.bot;
    let guard = guard.clone();
    let scope = scope.clone();
    let key = source_key.to_owned();
    host.store
        .call(move |db| {
            query_snapshot(db, request, guard, scope, &key, actor)?;
            Ok(())
        })
        .await?;
    account(host, bot, generation, cancel)
}

fn prune_namespaces(db: &Connection, ns: &str) -> Result<()> {
    for table in [
        "ai_learning_vectors",
        "ai_learning_semantic_attempts",
        "ai_learning_semantic_state",
    ] {
        db.execute(&format!("DELETE FROM {table} WHERE namespace!=?1"), [ns])?;
    }
    Ok(())
}

fn reserve(db: &Connection, purpose: &str) -> Result<()> {
    db.execute(
        "DELETE FROM ai_learning_semantic_admissions WHERE at<=?1",
        [now().saturating_sub(3600)],
    )?;
    let count: i64 = db.query_row(
        "SELECT count(*) FROM ai_learning_semantic_admissions",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        count < REQUESTS_PER_HOUR,
        "learning embedding quota exhausted"
    );
    db.execute(
        "INSERT INTO ai_learning_semantic_admissions(at,purpose) VALUES(?1,?2)",
        params![now(), purpose],
    )?;
    Ok(())
}

fn cached(
    db: &Connection,
    doc: &Document,
    ns: &str,
    dimensions: usize,
) -> Result<Option<Vec<f32>>> {
    let row:Option<(i64,String,usize,Vec<u8>)> = db.query_row("SELECT note_version,document_hash,dimensions,vector FROM ai_learning_vectors WHERE note_id=?1 AND namespace=?2",params![doc.id,ns],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((version, hash, stored_dimensions, bytes)) = row else {
        return Ok(None);
    };
    if version != doc.version || hash != doc.hash || dimensions != stored_dimensions {
        return Ok(None);
    }
    Ok(decode_vector(&bytes, dimensions).ok())
}

fn pending(
    db: &Connection,
    request: Request,
    ns: &str,
    dimensions: usize,
) -> Result<Option<IndexSnapshot>> {
    let revision = state(db, &request)?;
    prune_namespaces(db, ns)?;
    let ids = db.prepare("SELECT n.id FROM ai_learning_notes n LEFT JOIN ai_learning_semantic_attempts a ON a.note_id=n.id AND a.namespace=?2 WHERE n.bot_id=?1 AND n.deleted=0 AND n.invalidated=0 AND COALESCE(a.retry_after,0)<=?3 ORDER BY n.id LIMIT 128")?.query_map(params![request.bot,ns,now()],|r|r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let Ok(document) = document(db, &request, id) else {
            continue;
        };
        if cached(db, &document, ns, dimensions)?.is_none() {
            return Ok(Some(IndexSnapshot {
                request,
                revision,
                document,
            }));
        }
    }
    Ok(None)
}

fn candidates(
    db: &Connection,
    query: &QuerySnapshot,
    ns: &str,
    dimensions: usize,
) -> Result<Vec<Candidate>> {
    validate_query(db, query)?;
    let revision = state(db, &query.request)?;
    let ids = db.prepare("SELECT n.id FROM ai_learning_notes n JOIN ai_learning_vectors v ON v.note_id=n.id AND v.namespace=?3 AND v.note_version=n.version AND v.dimensions=?4 WHERE n.bot_id=?1 AND n.deleted=0 AND n.invalidated=0 AND (n.scope='shared' OR n.scope=?2) ORDER BY n.id LIMIT 128")?.query_map(params![query.request.bot,query.scope.key(),ns,dimensions],|r|r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut result = Vec::new();
    for id in ids {
        let Ok(document) = document(db, &query.request, id) else {
            continue;
        };
        if let Some(vector) = cached(db, &document, ns, dimensions)? {
            result.push(Candidate {
                snapshot: IndexSnapshot {
                    request: query.request.clone(),
                    revision,
                    document,
                },
                vector,
            });
        }
    }
    Ok(result)
}

fn validate_candidates(
    db: &Connection,
    candidates: &[Candidate],
    ns: &str,
    dimensions: usize,
) -> Result<()> {
    ensure!(
        !candidates.is_empty() && candidates.len() <= CANDIDATE_LIMIT,
        "learning vector candidates unavailable"
    );
    for candidate in candidates {
        validate_index(db, &candidate.snapshot)?;
        ensure!(
            cached(db, &candidate.snapshot.document, ns, dimensions)?.is_some(),
            "learning vector cache changed"
        );
    }
    Ok(())
}

fn ranked(candidates: &[Candidate], vector: &[f32], limit: usize) -> Vec<i64> {
    let mut scores = candidates
        .iter()
        .filter_map(|c| {
            cosine(&c.vector, vector)
                .filter(|score| *score > 0.0)
                .map(|score| (score, c.snapshot.document.id))
        })
        .collect::<Vec<_>>();
    scores.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scores
        .into_iter()
        .take(limit.min(CANDIDATE_LIMIT))
        .map(|(_, id)| id)
        .collect()
}

fn commit_index(
    db: &Connection,
    snapshot: &IndexSnapshot,
    ns: &str,
    dimensions: usize,
    vector: &[f32],
) -> Result<()> {
    validate_index(db, snapshot)?;
    ensure!(
        vector.len() == dimensions,
        "learning vector dimensions changed"
    );
    let encoded = encode_vector(vector)?;
    db.execute("INSERT INTO ai_learning_vectors(note_id,namespace,note_version,document_hash,dimensions,vector,indexed_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(note_id,namespace) DO UPDATE SET note_version=excluded.note_version,document_hash=excluded.document_hash,dimensions=excluded.dimensions,vector=excluded.vector,indexed_at=excluded.indexed_at",params![snapshot.document.id,ns,snapshot.document.version,snapshot.document.hash,dimensions,encoded,now()])?;
    db.execute(
        "DELETE FROM ai_learning_semantic_attempts WHERE note_id=?1 AND namespace=?2",
        params![snapshot.document.id, ns],
    )?;
    Ok(())
}

fn failure_reason(error: &anyhow::Error) -> &'static str {
    match error.downcast_ref::<EmbeddingError>() {
        Some(EmbeddingError::Timeout) => "timeout",
        Some(EmbeddingError::HttpStatus(_)) => "http_status",
        Some(EmbeddingError::Transport) => "transport",
        Some(EmbeddingError::Cancelled) => "cancelled",
        Some(EmbeddingError::SourceChanged) => "source_or_quota_changed",
        Some(EmbeddingError::Input) => "input",
        Some(
            EmbeddingError::Vector | EmbeddingError::Response | EmbeddingError::ResponseTooLarge,
        ) => "response",
        _ => "unavailable",
    }
}

async fn usage(host: &Host, purpose: &'static str, tokens: u64) {
    let Ok(tokens) = i64::try_from(tokens) else {
        return;
    };
    let model = host.config.embedding.model.clone();
    if host.store.call(move |db| {
        db.execute("INSERT INTO usage_daily(day,plugin_id,purpose,model,requests,input_tokens,output_tokens) VALUES(date('now'),'ai',?1,?2,1,?3,0) ON CONFLICT(day,plugin_id,purpose,model) DO UPDATE SET requests=requests+1,input_tokens=input_tokens+excluded.input_tokens",params![purpose,model,tokens])?;
        Ok(())
    }).await.is_err() {tracing::warn!("learning embedding usage could not be recorded");}
}

/// At most one valid note per maintenance tick. The client shares the existing
/// bounded embedding lane; admission happens only after that lane is acquired.
pub async fn index_next(host: &Host, cancel: &CancellationToken) -> Result<bool> {
    if !enabled(host) || cancel.is_cancelled() {
        return Ok(false);
    }
    let Some(bot) = host.milky.verified_bot_id() else {
        return Ok(false);
    };
    let embedding = host.embedding.as_ref().expect("enabled embedding");
    let request = request(host)?;
    // Account verification is independent of any particular physical group.
    let generation = host.milky.scope_generation(&Scope {
        bot_id: bot,
        scene: Scene::Group,
        peer_id: bot,
    })?;
    let ns = namespace(embedding);
    let dimensions = embedding.dimensions();
    let prepare_ns = ns.clone();
    let snapshot = host
        .store
        .call(move |db| {
            let tx = db.transaction()?;
            let result = pending(&tx, request, &prepare_ns, dimensions)?;
            tx.commit()?;
            Ok(result)
        })
        .await?;
    let Some(snapshot) = snapshot else {
        return Ok(false);
    };
    let outbound = snapshot.clone();
    let response = embedding
        .embed_text(&snapshot.document.input, cancel, async {
            account(host, bot, generation, cancel)?;
            host.store
                .call(move |db| {
                    let tx = db.transaction()?;
                    validate_index(&tx, &outbound)?;
                    reserve(&tx, "index")?;
                    tx.commit()?;
                    Ok(())
                })
                .await?;
            account(host, bot, generation, cancel)
        })
        .await;
    if let Ok(response) = &response {
        usage(host, "learning_embedding_index", response.input_tokens).await;
    }
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            let failed = snapshot.clone();
            let failed_ns = ns.clone();
            let _ = host.store.call(move |db| {
                if validate_index(db,&failed).is_ok() {db.execute("INSERT INTO ai_learning_semantic_attempts(note_id,namespace,retry_after) VALUES(?1,?2,?3) ON CONFLICT(note_id,namespace) DO UPDATE SET retry_after=excluded.retry_after",params![failed.document.id,failed_ns,now().saturating_add(INDEX_RETRY)])?;}
                Ok(())
            }).await;
            tracing::debug!(
                reason = failure_reason(&error),
                "learning vector indexing deferred"
            );
            return Ok(false);
        }
    };
    account(host, bot, generation, cancel)?;
    let milky = host.milky.clone();
    let stop = host.cancel.clone();
    let cancel = cancel.clone();
    let result = host
        .store
        .call(move |db| {
            ensure!(
                !stop.is_cancelled()
                    && !cancel.is_cancelled()
                    && milky.verified_bot_id() == Some(bot),
                "learning index cancelled"
            );
            milky.check_generation(generation)?;
            let tx = db.transaction()?;
            commit_index(&tx, &snapshot, &ns, dimensions, &response.vector)?;
            tx.commit()?;
            Ok(())
        })
        .await;
    if result.is_err() {
        tracing::debug!("learning vector result rejected after source change");
    }
    Ok(result.is_ok())
}

/// None selects ordinary recall; authorization/source failures remain errors so
/// an invalid current message cannot be revived through a lexical fallback.
pub(super) async fn ranked_ids(
    host: &Host,
    guard: &ContextGuard,
    scope: &Scope,
    source_key: &str,
    actor: i64,
    cancel: &CancellationToken,
) -> Result<Option<Vec<i64>>> {
    if !enabled(host) {
        return Ok(None);
    }
    let request = request(host)?;
    ensure!(
        scope.scene == Scene::Group && scope.bot_id == request.bot,
        "learning recall is group-only"
    );
    let bot = request.bot;
    let generation = host.milky.scope_generation(scope)?;
    account(host, bot, generation, cancel)?;
    let embedding = host.embedding.as_ref().expect("enabled embedding");
    let ns = namespace(embedding);
    let dimensions = embedding.dimensions();
    let guard = guard.clone();
    let scope = scope.clone();
    let key = source_key.to_owned();
    let query_ns = ns.clone();
    let prepared = host
        .store
        .call(move |db| {
            let query = query_snapshot(db, request, guard, scope, &key, actor)?;
            let retry: i64 = db
                .query_row(
                    "SELECT query_retry_after FROM ai_learning_semantic_state WHERE namespace=?1",
                    [&query_ns],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            let found = if retry > now() {
                Vec::new()
            } else {
                candidates(db, &query, &query_ns, dimensions)?
            };
            Ok((query, found))
        })
        .await?;
    let (snapshot, found) = prepared;
    if found.is_empty() {
        account(host, bot, generation, cancel)?;
        return Ok(None);
    }
    let outbound = snapshot.clone();
    let outbound_candidates = found.clone();
    let outbound_ns = ns.clone();
    let response = embedding
        .embed_text(&snapshot.source.text, cancel, async {
            account(host, bot, generation, cancel)?;
            host.store
                .call(move |db| {
                    let tx = db.transaction()?;
                    validate_query(&tx, &outbound)?;
                    validate_candidates(&tx, &outbound_candidates, &outbound_ns, dimensions)?;
                    reserve(&tx, "query")?;
                    tx.commit()?;
                    Ok(())
                })
                .await?;
            account(host, bot, generation, cancel)
        })
        .await;
    if let Ok(response) = &response {
        usage(host, "learning_embedding_query", response.input_tokens).await;
    }
    account(host, bot, generation, cancel)?;
    let recheck = snapshot.clone();
    host.store
        .call(move |db| validate_query(db, &recheck))
        .await?;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            if matches!(
                error.downcast_ref::<EmbeddingError>(),
                Some(
                    EmbeddingError::Timeout
                        | EmbeddingError::Transport
                        | EmbeddingError::HttpStatus(_)
                        | EmbeddingError::Response
                        | EmbeddingError::Vector
                        | EmbeddingError::ResponseTooLarge
                )
            ) {
                let failed_ns = ns.clone();
                let _=host.store.call(move |db| {db.execute("INSERT INTO ai_learning_semantic_state(namespace,query_retry_after) VALUES(?1,?2) ON CONFLICT(namespace) DO UPDATE SET query_retry_after=excluded.query_retry_after",params![failed_ns,now().saturating_add(QUERY_RETRY)])?;Ok(())}).await;
            }
            tracing::debug!(
                reason = failure_reason(&error),
                "learning vector query unavailable; using ordinary recall"
            );
            return Ok(None);
        }
    };
    let milky = host.milky.clone();
    let stop = host.cancel.clone();
    let commit_cancel = cancel.clone();
    let result = host
        .store
        .call(move |db| {
            ensure!(
                !stop.is_cancelled()
                    && !commit_cancel.is_cancelled()
                    && milky.verified_bot_id() == Some(bot),
                "learning query cancelled"
            );
            milky.check_generation(generation)?;
            validate_query(db, &snapshot)?;
            if validate_candidates(db, &found, &ns, dimensions).is_err() {
                return Ok(None);
            }
            ensure!(
                response.vector.len() == dimensions,
                "learning query dimensions changed"
            );
            // Keep all bounded semantic candidates. The parent applies recall_limit
            // only after fusion and byte-budget packing, so an oversized first hit
            // cannot hide a smaller relevant second hit.
            Ok(Some(ranked(&found, &response.vector, CANDIDATE_LIMIT)))
        })
        .await?;
    account(host, bot, generation, cancel)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> Store {
        let store = Store::open(":memory:", 32).await.unwrap();
        crate::plugins::ai::memory::MemoryStore::new(store.clone(), 100)
            .init()
            .await
            .unwrap();
        store.call(|db| {
            db.execute("INSERT INTO plugin_states(id,epoch,enabled) VALUES('ai',1,1)",[])?;
            db.execute("INSERT INTO ai_learning_state(bot_id,enabled,watermark) VALUES(1,1,0),(2,1,0)",[])?;
            db.execute("INSERT INTO ai_conversations(id,scope,mode) VALUES('1:group:20:v0:group','1:group:20','group'),('1:group:30:v0:group','1:group:30','group')",[])?;
            db.execute("INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,learnable,origin_plugin) VALUES('1:group:20:1','1:group:20','1:group:20:v0:group',1,7,'user','怎样表达更清楚',?1,0,'memory_only')",[now()])?;
            Ok(())
        }).await.unwrap();
        store
    }

    fn req(bot: i64) -> Request {
        Request {
            bot,
            plugin_epoch: 1,
            defaults: AiSettings::default(),
        }
    }
    fn guard() -> ContextGuard {
        ContextGuard {
            conversation_id: "1:group:20:v0:group".into(),
            epoch: 1,
        }
    }
    fn scope() -> Scope {
        Scope {
            bot_id: 1,
            scene: Scene::Group,
            peer_id: 20,
        }
    }
    fn query(db: &Connection) -> Result<QuerySnapshot> {
        query_snapshot(db, req(1), guard(), scope(), "1:group:20:1", 7)
    }
    fn vector(a: f32, b: f32) -> Vec<f32> {
        let mut result = vec![0.0; 64];
        result[0] = a;
        result[1] = b;
        result
    }
    fn add(db: &Connection, bot: i64, scope: &str, text: &str) -> Result<i64> {
        db.execute("INSERT INTO ai_learning_notes(bot_id,kind,text,scope,updated_at) VALUES(?1,'experience',?2,?3,?4)",params![bot,text,scope,now()])?;
        Ok(db.last_insert_rowid())
    }
    fn index(
        db: &Connection,
        bot: i64,
        id: i64,
        ns: &str,
        vector: &[f32],
    ) -> Result<IndexSnapshot> {
        let request = req(bot);
        let snapshot = IndexSnapshot {
            revision: state(db, &request)?,
            document: document(db, &request, id)?,
            request,
        };
        commit_index(db, &snapshot, ns, 64, vector)?;
        Ok(snapshot)
    }

    #[tokio::test]
    async fn ranking_is_positive_bounded_namespace_and_group_account_scoped() {
        let store = fixture().await;
        store.call(|db| {
            let old=add(db,1,"shared","旧而相关的经验")?;
            let second=add(db,1,"1:group:20","同群相关经验")?;
            let unrelated=add(db,1,"shared","新而无关的经验")?;
            let other_group=add(db,1,"1:group:30","其他群专属经验")?;
            let other_bot=add(db,2,"shared","其他机器人经验")?;
            for (bot,id,vec) in [(1,old,vector(1.0,0.0)),(1,second,vector(0.8,0.6)),(1,unrelated,vector(-1.0,0.0)),(1,other_group,vector(1.0,0.0)),(2,other_bot,vector(1.0,0.0))] {index(db,bot,id,"model-a",&vec)?;}
            let query=query(db)?;
            let found=candidates(db,&query,"model-a",64)?;
            assert_eq!(ranked(&found,&vector(1.0,0.0),CANDIDATE_LIMIT),vec![old,second],"retain the smaller second candidate for the parent's budget packing even when recall_limit is one");
            assert!(candidates(db,&query,"model-b",64)?.is_empty());
            assert!(candidates(db,&query,"model-a",128)?.is_empty());
            db.execute("UPDATE ai_learning_vectors SET document_hash='wrong' WHERE note_id=?1",[old])?;
            assert_eq!(ranked(&candidates(db,&query,"model-a",64)?,&vector(1.0,0.0),CANDIDATE_LIMIT),vec![second]);
            assert_eq!(pending(db,req(1),"model-a",64)?.unwrap().document.id,old,"bad hashes must be rebuilt");
            prune_namespaces(db,"model-b")?;
            let remaining:i64=db.query_row("SELECT count(*) FROM ai_learning_vectors",[],|r|r.get(0))?;
            assert_eq!(remaining,0);
            assert_eq!(db.query_row("SELECT count(*) FROM ai_learning_notes",[],|r|r.get::<_,i64>(0))?,5,"cache rebuild never removes authoritative notes");
            Ok(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn note_updates_rollbacks_and_source_tombstones_clear_cache_and_reject_late_vectors() {
        let store = fixture().await;
        store.call(|db| {
            db.execute("UPDATE messages SET learnable=1,origin_plugin=NULL WHERE key='1:group:20:1'",[])?;
            let (mut source,_)=super::super::read_source(db,"1:group:20:1",1,&AiSettings::default())?;
            source.quote="表达更清楚".into();
            let id=add(db,1,"shared","先表达结论")?;
            super::super::insert_sources(db,id,&[source])?;
            let first=index(db,1,id,"model-a",&vector(1.0,0.0))?;
            db.execute("UPDATE ai_learning_notes SET text='给出一个例子',version=version+1 WHERE id=?1",[id])?;
            assert!(commit_index(db,&first,"model-a",64,&vector(1.0,0.0)).is_err());
            assert_eq!(db.query_row("SELECT count(*) FROM ai_learning_vectors",[],|r|r.get::<_,i64>(0))?,0);
            let second=index(db,1,id,"model-a",&vector(1.0,0.0))?;
            db.execute("UPDATE ai_learning_notes SET text='先表达结论',version=version+1 WHERE id=?1",[id])?;
            assert!(commit_index(db,&second,"model-a",64,&vector(1.0,0.0)).is_err(),"rolling back text does not revive an old-version cache");
            let third=index(db,1,id,"model-a",&vector(1.0,0.0))?;
            db.execute("INSERT INTO ai_revoked_sources(source_key,revoked_at) VALUES('1:group:20:1',?1)",[now()])?;
            assert_eq!(db.query_row("SELECT count(*) FROM ai_learning_vectors",[],|r|r.get::<_,i64>(0))?,0);
            assert!(commit_index(db,&third,"model-a",64,&vector(1.0,0.0)).is_err());
            let manual=add(db,1,"shared","人工补充")?;
            index(db,1,manual,"model-a",&vector(1.0,0.0))?;
            db.execute("DELETE FROM ai_learning_notes WHERE id=?1",[manual])?;
            assert_eq!(db.query_row("SELECT count(*) FROM ai_learning_vectors",[],|r|r.get::<_,i64>(0))?,0);
            Ok(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn query_requires_current_human_permissions_but_not_learnability() {
        let store = fixture().await;
        store.call(|db| {
            let snapshot=query(db)?;
            validate_query(db,&snapshot)?;
            assert!(query_snapshot(db,req(1),guard(),scope(),"1:group:20:1",1).is_err());
            assert!(query_snapshot(db,req(1),guard(),Scope {scene:Scene::Friend,..scope()},"1:group:20:1",7).is_err());
            db.execute("UPDATE messages SET content='换了问题' WHERE key='1:group:20:1'",[])?;
            assert!(validate_query(db,&snapshot).is_err());
            let current=query(db)?;
            db.execute("UPDATE ai_conversations SET settings='{\"memory_read\":false}' WHERE id=?1",[&current.guard.conversation_id])?;
            assert!(query(db).is_err());
            db.execute("UPDATE ai_conversations SET settings='{}' WHERE id=?1",[&current.guard.conversation_id])?;
            db.execute("INSERT INTO scope_access(scope,bot_id,scene,peer_id,enabled,revision) VALUES('1:group:20',1,'group',20,1,1)",[])?;
            assert!(query(db).is_err(),"old access revision cannot send a query after a scope toggle");
            db.execute("UPDATE messages SET access_revision=1 WHERE key='1:group:20:1'",[])?;
            query(db)?;
            db.execute("INSERT INTO ai_scope_modes(scope,mode,revision) VALUES('1:group:20','group',1)",[])?;
            assert!(query(db).is_err());
            Ok(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn embedding_quota_is_shared_by_index_and_query_and_backoff_does_not_change_notes() {
        let store = fixture().await;
        store.call(|db| {
            for number in 0..REQUESTS_PER_HOUR {reserve(db,if number%2==0 {"index"} else {"query"})?;}
            assert!(reserve(db,"query").is_err());
            db.execute("UPDATE ai_learning_semantic_admissions SET at=?1",[now().saturating_sub(3600)])?;
            reserve(db,"index")?;
            assert_eq!(db.query_row("SELECT count(*) FROM ai_learning_semantic_admissions",[],|r|r.get::<_,i64>(0))?,1);
            let first=add(db,1,"shared","第一条")?;
            let second=add(db,1,"shared","第二条")?;
            db.execute("INSERT INTO ai_learning_semantic_attempts(note_id,namespace,retry_after) VALUES(?1,'model-a',?2)",params![first,now()+INDEX_RETRY])?;
            assert_eq!(pending(db,req(1),"model-a",64)?.unwrap().document.id,second);
            assert_eq!(note(db,&req(1),first)?.text,"第一条");
            db.execute("UPDATE ai_learning_state SET enabled=0 WHERE bot_id=1",[])?;
            assert!(pending(db,req(1),"model-a",64).is_err());
            Ok(())
        }).await.unwrap();
    }
}
