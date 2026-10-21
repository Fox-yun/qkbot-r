//! Per-account/group sticker library. Protocol resources are verified before a
//! bounded import; vectors, originals and thumbnails never grant a send capability.
use crate::{
    embedding::{cosine, decode_vector, encode_vector},
    host::Host,
    store::Store,
    types::{ContextGuard, Incoming, MessageKey, Scene, Scope, now},
};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

pub mod admin;

const SOURCE_LIMIT: usize = 8 * 1024 * 1024;
const THUMB_LIMIT: usize = 1024 * 1024;
const MAX_MESSAGE_IMAGES: usize = 2;
const CANDIDATE_LIMIT: usize = 4;
const MIGRATION: &str = r#"
CREATE TABLE ai_sticker_control(id INTEGER PRIMARY KEY CHECK(id=1),enabled INTEGER NOT NULL,revision INTEGER NOT NULL,cooldown_secs INTEGER NOT NULL);
CREATE TABLE ai_sticker_scopes(scope TEXT PRIMARY KEY,enabled INTEGER NOT NULL DEFAULT 1,revision INTEGER NOT NULL DEFAULT 1,last_sent_at INTEGER NOT NULL DEFAULT 0);
CREATE TABLE ai_stickers(id INTEGER PRIMARY KEY AUTOINCREMENT,scope TEXT NOT NULL,hash TEXT NOT NULL,mime TEXT NOT NULL,original_bytes INTEGER NOT NULL,thumbnail_bytes INTEGER NOT NULL,thumbnail_hash TEXT NOT NULL,fingerprint TEXT NOT NULL,dimensions INTEGER NOT NULL,vector BLOB NOT NULL,revision INTEGER NOT NULL DEFAULT 1,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,retry_after INTEGER NOT NULL DEFAULT 0,UNIQUE(scope,hash));
CREATE INDEX ai_stickers_scope ON ai_stickers(scope,updated_at,id);
CREATE TABLE ai_sticker_sources(sticker_id INTEGER NOT NULL REFERENCES ai_stickers(id) ON DELETE CASCADE,source_key TEXT NOT NULL,scope TEXT NOT NULL,conversation_id TEXT NOT NULL,epoch INTEGER NOT NULL,PRIMARY KEY(sticker_id,source_key));
CREATE INDEX ai_sticker_source ON ai_sticker_sources(source_key);
CREATE TABLE ai_sticker_revoked(source_key TEXT PRIMARY KEY,revoked_at INTEGER NOT NULL);
CREATE TABLE ai_sticker_admissions(at INTEGER NOT NULL);
CREATE INDEX ai_sticker_admissions_time ON ai_sticker_admissions(at);
"#;

const ADMIN_MIGRATION: &str = r#"
ALTER TABLE ai_stickers ADD COLUMN reindex_requested INTEGER NOT NULL DEFAULT 0;
CREATE TABLE ai_sticker_deletions(scope TEXT NOT NULL,hash TEXT NOT NULL,source_cutoff INTEGER NOT NULL,deleted_at INTEGER NOT NULL,PRIMARY KEY(scope,hash));
CREATE TABLE ai_sticker_admin_receipts(admin_id TEXT NOT NULL,session_id TEXT NOT NULL,request_id TEXT NOT NULL,payload_hash TEXT NOT NULL,result_json TEXT NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(admin_id,session_id,request_id));
CREATE INDEX ai_sticker_admin_receipts_age ON ai_sticker_admin_receipts(created_at);
"#;

/// Shared administration initializes schema without starting a Host or changing
/// persisted controls. Keep migration v1 byte-for-byte stable for live upgrades.
pub async fn init_store(store: &Store) -> Result<()> {
    store.migrate("ai_stickers", 1, MIGRATION).await?;
    store.migrate("ai_stickers", 2, ADMIN_MIGRATION).await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StickerConfig {
    pub enabled: bool,
    pub auto_collect: bool,
    pub max_per_group: usize,
    pub max_group_bytes: usize,
    pub max_total_bytes: usize,
    pub max_total_items: usize,
    pub queue_capacity: usize,
    pub requests_per_hour: usize,
    pub send_cooldown_secs: u64,
}

impl Default for StickerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_collect: true,
            max_per_group: 256,
            max_group_bytes: 128 * 1024 * 1024,
            max_total_bytes: 512 * 1024 * 1024,
            max_total_items: 4096,
            queue_capacity: 32,
            requests_per_hour: 120,
            send_cooldown_secs: 60,
        }
    }
}

impl StickerConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=1024).contains(&self.max_per_group),
            "sticker group capacity out of range"
        );
        ensure!(
            (1..=8192).contains(&self.max_total_items)
                && self.max_per_group <= self.max_total_items,
            "sticker total capacity out of range"
        );
        ensure!(
            (1024 * 1024..=2 * 1024 * 1024 * 1024).contains(&self.max_group_bytes),
            "sticker group byte capacity out of range"
        );
        ensure!(
            self.max_total_bytes >= self.max_group_bytes
                && self.max_total_bytes <= 8 * 1024 * 1024 * 1024usize,
            "sticker total byte capacity out of range"
        );
        ensure!(
            (1..=128).contains(&self.queue_capacity),
            "sticker queue capacity out of range"
        );
        ensure!(
            (1..=10000).contains(&self.requests_per_hour),
            "sticker request capacity out of range"
        );
        ensure!(
            (1..=86400).contains(&self.send_cooldown_secs),
            "sticker cooldown out of range"
        );
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: i64,
    pub revision: i64,
    pub hash: String,
    pub thumbnail_jpeg: Vec<u8>,
    pub score: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchStatus {
    Ready,
    Empty,
    Cooldown,
    IndexPending,
    Unavailable,
    Disabled,
}

impl SearchStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Empty => "empty",
            Self::Cooldown => "cooldown",
            Self::IndexPending => "index_pending",
            Self::Unavailable => "unavailable",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub candidates: Vec<Candidate>,
    pub status: SearchStatus,
}

impl SearchResult {
    fn without_candidates(status: SearchStatus) -> Self {
        Self {
            candidates: Vec::new(),
            status,
        }
    }
}

pub struct StoredImage {
    pub bytes: Vec<u8>,
    pub mime: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ScopeStamp {
    access: i64,
    plugin: i64,
    library: i64,
    control: i64,
    plugin_epoch: i64,
}

#[derive(Clone, Debug)]
struct Job {
    key: MessageKey,
    sender: i64,
    resource: String,
    automatic: bool,
    stamp: ScopeStamp,
    context: ContextGuard,
    received_at: i64,
}

struct Import {
    hash: String,
    mime: String,
    original_bytes: usize,
    thumbnail_bytes: usize,
    thumbnail_hash: String,
    fingerprint: String,
    dimensions: usize,
    vector: Vec<u8>,
}

#[derive(Clone)]
struct Reindex {
    scope: Scope,
    id: i64,
    revision: i64,
    hash: String,
    thumbnail_hash: String,
    thumbnail_bytes: usize,
    stamp: ScopeStamp,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    pending: HashSet<(String, String)>,
}

struct Pending<'a> {
    queue: &'a Mutex<Queue>,
    identity: (String, String),
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.pending.remove(&self.identity);
        }
    }
}

pub struct StickerStore {
    store: Store,
    config: StickerConfig,
    directory: PathBuf,
    queue: Mutex<Queue>,
    worker: tokio::sync::Mutex<()>,
    gc_cursor: AtomicUsize,
    turns: AtomicUsize,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn group(scope: &Scope) -> Result<()> {
    ensure!(
        scope.scene == Scene::Group && scope.bot_id > 0 && scope.peer_id > 0,
        "sticker scope must be a group"
    );
    Ok(())
}

fn enabled(db: &Connection, scope: &Scope) -> Result<bool> {
    group(scope)?;
    let global: bool = db.query_row(
        "SELECT enabled FROM ai_sticker_control WHERE id=1",
        [],
        |r| r.get(0),
    )?;
    let local: bool = db
        .query_row(
            "SELECT enabled FROM ai_sticker_scopes WHERE scope=?1",
            [scope.key()],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(true);
    let plugin: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM plugin_states WHERE id='ai' AND enabled=1)",
        [],
        |r| r.get(0),
    )?;
    Ok(global
        && local
        && plugin
        && crate::access::allowed_revision(db, &scope.key())?.is_some()
        && crate::catalog::allowed_revision(db, &scope.key(), "ai")?.is_some())
}

fn stamp(db: &Connection, scope: &Scope, plugin_epoch: i64) -> Result<ScopeStamp> {
    ensure!(enabled(db, scope)?, "sticker collection disabled");
    crate::host::check_plugin(db, "ai", plugin_epoch)?;
    Ok(ScopeStamp {
        access: crate::access::require_enabled(db, &scope.key())?,
        plugin: crate::catalog::allowed_revision(db, &scope.key(), "ai")?
            .context("sticker AI disabled")?,
        library: db
            .query_row(
                "SELECT revision FROM ai_sticker_scopes WHERE scope=?1",
                [scope.key()],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0),
        control: db.query_row(
            "SELECT revision FROM ai_sticker_control WHERE id=1",
            [],
            |r| r.get(0),
        )?,
        plugin_epoch,
    })
}

fn source_available(db: &Connection, key: &str, scope: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM messages m WHERE m.key=?1 AND m.scope=?2 AND m.revoked=0 AND NOT EXISTS(SELECT 1 FROM ai_revoked_sources r WHERE r.source_key=m.key) AND NOT EXISTS(SELECT 1 FROM ai_sticker_revoked r WHERE r.source_key=m.key))",params![key,scope],|r|r.get(0))?)
}

fn check_job(db: &Connection, job: &Job) -> Result<()> {
    ensure!(
        stamp(db, &job.key.scope, job.stamp.plugin_epoch)? == job.stamp,
        "sticker permissions changed"
    );
    crate::host::check_guard(db, &job.context)?;
    ensure!(
        source_available(db, &job.key.key(), &job.key.scope.key())?,
        "sticker source unavailable"
    );
    let source: (i64,i64,String,i64,i64) = db.query_row("SELECT sender_id,access_revision,conversation_id,epoch,received_at FROM messages WHERE key=?1",[job.key.key()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    ensure!(
        source
            == (
                job.sender,
                job.stamp.access,
                job.context.conversation_id.clone(),
                job.context.epoch,
                job.received_at
            ),
        "sticker source changed"
    );
    Ok(())
}

fn check_turn(
    db: &Connection,
    scope: &Scope,
    expected: &ScopeStamp,
    guard: &ContextGuard,
    source: &str,
) -> Result<()> {
    ensure!(
        stamp(db, scope, expected.plugin_epoch)? == *expected,
        "sticker permissions changed"
    );
    crate::host::check_guard(db, guard)?;
    ensure!(
        source_available(db, source, &scope.key())?,
        "sticker query source unavailable"
    );
    let belongs: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE key=?1 AND conversation_id=?2 AND epoch=?3 AND access_revision=?4)",params![source,guard.conversation_id,guard.epoch,expected.access],|r|r.get(0))?;
    ensure!(belongs, "sticker query source changed");
    Ok(())
}

/// Called by the host inside its final send authorization transaction. Candidate
/// IDs, hashes and revisions originate in this library, never in model arguments.
pub fn check_candidate(
    db: &Connection,
    scope: &Scope,
    id: i64,
    revision: i64,
    content_hash: &str,
) -> Result<()> {
    check_stored(db, scope, id, revision, content_hash)?;
    let ready: bool = db.query_row("SELECT COALESCE((SELECT last_sent_at FROM ai_sticker_scopes WHERE scope=?1),0)+cooldown_secs<=?2 FROM ai_sticker_control WHERE id=1",params![scope.key(),now()],|r|r.get(0))?;
    ensure!(ready, "sticker send cooldown");
    Ok(())
}

pub(super) fn check_stored(
    db: &Connection,
    scope: &Scope,
    id: i64,
    revision: i64,
    content_hash: &str,
) -> Result<()> {
    ensure!(
        enabled(db, scope)? && valid_hash(content_hash),
        "sticker candidate unavailable"
    );
    let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM ai_stickers WHERE id=?1 AND scope=?2 AND revision=?3 AND hash=?4)",params![id,scope.key(),revision,content_hash],|r|r.get(0))?;
    ensure!(exists, "sticker candidate changed or revoked");
    ensure!(
        source_valid(db, &scope.key(), id)?,
        "sticker source unavailable"
    );
    Ok(())
}

fn source_valid(db: &Connection, scope: &str, id: i64) -> Result<bool> {
    // Ordinary raw retention is allowed; an epoch change needs a matching raw
    // row carried forward by settings, never an inference after forgetting.
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM ai_sticker_sources p JOIN ai_conversations c ON c.id=p.conversation_id AND c.scope=p.scope LEFT JOIN messages m ON m.key=p.source_key WHERE p.sticker_id=?1 AND p.scope=?2 AND ((m.key IS NULL AND c.epoch=p.epoch) OR (m.scope=p.scope AND m.conversation_id=p.conversation_id AND m.epoch=c.epoch AND m.revoked=0)) AND NOT EXISTS(SELECT 1 FROM ai_revoked_sources r WHERE r.source_key=p.source_key) AND NOT EXISTS(SELECT 1 FROM ai_sticker_revoked r WHERE r.source_key=p.source_key))",params![id,scope],|r|r.get(0))?)
}

/// Read under one snapshot. A temporary search restriction does not mean that
/// this group's library is empty; another group's records never affect it.
fn search_availability(
    db: &Connection,
    scope: &Scope,
    fingerprint: &str,
    dimensions: usize,
    at: i64,
) -> Result<SearchStatus> {
    let key = scope.key();
    let (total, indexed): (i64, i64) = db.query_row(
        "SELECT COUNT(*),COUNT(CASE WHEN fingerprint=?2 AND dimensions=?3 THEN 1 END) FROM ai_stickers WHERE scope=?1",
        params![key, fingerprint, dimensions],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if total == 0 {
        return Ok(SearchStatus::Empty);
    }
    let ready: bool = db.query_row(
        "SELECT COALESCE((SELECT last_sent_at FROM ai_sticker_scopes WHERE scope=?1),0)+cooldown_secs<=?2 FROM ai_sticker_control WHERE id=1",
        params![key, at],
        |row| row.get(0),
    )?;
    Ok(if !ready {
        SearchStatus::Cooldown
    } else if indexed == 0 {
        SearchStatus::IndexPending
    } else {
        SearchStatus::Ready
    })
}

fn finish_search(db: &Connection, scope: &Scope, mut candidates: Vec<Candidate>) -> SearchResult {
    candidates.retain(|candidate| {
        check_candidate(db, scope, candidate.id, candidate.revision, &candidate.hash).is_ok()
    });
    SearchResult {
        status: if candidates.is_empty() {
            SearchStatus::Unavailable
        } else {
            SearchStatus::Ready
        },
        candidates,
    }
}

fn check_not_deleted(db: &Connection, job: &Job, content_hash: &str) -> Result<()> {
    ensure!(db.query_row("SELECT EXISTS(SELECT 1 FROM messages m WHERE m.key=?1 AND m.scope=?2 AND NOT EXISTS(SELECT 1 FROM ai_sticker_deletions d WHERE d.scope=m.scope AND d.hash=?3 AND m.id<=d.source_cutoff))",params![job.key.key(),job.key.scope.key(),content_hash],|r|r.get::<_,bool>(0))?,"sticker source predates library deletion");
    Ok(())
}

/// Call only at the host's send admission boundary, never during preparation.
/// An ambiguous/failed admitted send conservatively consumes the cooldown.
pub fn mark_sent(
    db: &Connection,
    scope: &Scope,
    id: i64,
    revision: i64,
    content_hash: &str,
) -> Result<()> {
    check_candidate(db, scope, id, revision, content_hash)?;
    db.execute("INSERT INTO ai_sticker_scopes(scope,last_sent_at) VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET last_sent_at=excluded.last_sent_at",params![scope.key(),now()])?;
    Ok(())
}

fn resources(message: &Incoming, automatic: bool) -> Vec<String> {
    let mut seen = HashSet::new();
    message.raw_message["segments"]
        .as_array()
        .into_iter()
        .flatten()
        .take(512)
        .filter(|segment| {
            segment["type"] == "image" && (!automatic || segment["data"]["sub_type"] == "sticker")
        })
        .filter_map(|segment| segment["data"]["resource_id"].as_str())
        .filter(|id| !id.is_empty() && id.len() <= 4096 && !id.chars().any(char::is_control))
        .filter(|id| seen.insert((*id).to_owned()))
        .take(MAX_MESSAGE_IMAGES)
        .map(str::to_owned)
        .collect()
}

impl StickerStore {
    pub fn new(store: Store, config: StickerConfig, directory: PathBuf) -> Result<Self> {
        config.validate()?;
        ensure!(
            !directory.as_os_str().is_empty(),
            "sticker directory missing"
        );
        Ok(Self {
            store,
            config,
            directory,
            queue: Mutex::new(Queue::default()),
            worker: tokio::sync::Mutex::new(()),
            gc_cursor: AtomicUsize::new(0),
            turns: AtomicUsize::new(0),
        })
    }

    pub async fn init(&self) -> Result<()> {
        init_store(&self.store).await?;
        let active = self.config.enabled;
        let cooldown = i64::try_from(self.config.send_cooldown_secs)?;
        self.store.call(move|db| {
            db.execute("INSERT INTO ai_sticker_control(id,enabled,revision,cooldown_secs) VALUES(1,?1,1,?2) ON CONFLICT(id) DO UPDATE SET revision=revision+(enabled!=excluded.enabled OR cooldown_secs!=excluded.cooldown_secs),enabled=excluded.enabled,cooldown_secs=excluded.cooldown_secs",params![active,cooldown])?;
            Ok(())
        }).await
    }

    pub async fn enabled(&self, scope: &Scope) -> Result<bool> {
        if !self.config.enabled || scope.scene != Scene::Group {
            return Ok(false);
        }
        let scope = scope.clone();
        self.store.call(move |db| enabled(db, &scope)).await
    }

    pub async fn set_enabled(&self, scope: &Scope, value: bool) -> Result<()> {
        group(scope)?;
        let scope = scope.key();
        self.store.call(move|db| {
            db.execute("INSERT INTO ai_sticker_scopes(scope,enabled,revision) VALUES(?1,?2,1) ON CONFLICT(scope) DO UPDATE SET revision=revision+(enabled!=excluded.enabled),enabled=excluded.enabled",params![scope,value])?; Ok(())
        }).await
    }

    pub async fn observe(&self, host: &Host, message: &Incoming) -> Result<usize> {
        if !self.config.auto_collect {
            return Ok(0);
        }
        self.enqueue(host, message, true).await
    }

    /// The caller must authorize manual saving of this exact source message.
    pub async fn save(&self, host: &Host, message: &Incoming) -> Result<usize> {
        self.enqueue(host, message, false).await
    }

    async fn enqueue(&self, host: &Host, message: &Incoming, automatic: bool) -> Result<usize> {
        if !self.config.enabled
            || host.embedding.is_none()
            || !host.config.plugins.ai.enabled
            || message.key.scope.scene != Scene::Group
            || message.notification.is_some()
            || (automatic && message.sender_id == message.key.scope.bot_id)
            || host.cancel.is_cancelled()
        {
            return Ok(0);
        }
        let resources = resources(message, automatic);
        if resources.is_empty() || !self.enabled(&message.key.scope).await? {
            return Ok(0);
        }
        host.milky.scope_generation(&message.key.scope)?;
        let key = message.key.clone();
        let sender = message.sender_id;
        let received_at = message.received_at;
        let epoch = *host.epochs.get("ai").context("sticker AI inactive")?;
        let prototype = self
            .store
            .call(move |db| {
                let tx = db.unchecked_transaction()?;
                let stamp = stamp(&tx, &key.scope, epoch)?;
                let context = tx.query_row(
                    "SELECT conversation_id,epoch FROM messages WHERE key=?1",
                    [key.key()],
                    |r| {
                        Ok(ContextGuard {
                            conversation_id: r.get(0)?,
                            epoch: r.get(1)?,
                        })
                    },
                )?;
                let job = Job {
                    key,
                    sender,
                    resource: String::new(),
                    automatic,
                    stamp,
                    context,
                    received_at,
                };
                check_job(&tx, &job)?;
                tx.commit()?;
                Ok(job)
            })
            .await?;
        let mut queue = self
            .queue
            .lock()
            .map_err(|_| anyhow::anyhow!("sticker queue unavailable"))?;
        let mut count = 0;
        for resource in resources {
            if queue.pending.len() >= self.config.queue_capacity {
                break;
            }
            let identity = (prototype.key.key(), resource.clone());
            if queue.pending.insert(identity) {
                let mut job = prototype.clone();
                job.resource = resource;
                queue.jobs.push_back(job);
                count += 1;
            }
        }
        Ok(count)
    }

    pub async fn tick(&self, host: &Arc<Host>, cancel: &CancellationToken) -> Result<()> {
        let Ok(_worker) = self.worker.try_lock() else {
            return Ok(());
        };
        self.cleanup_unlocked().await?;
        if !self.config.enabled
            || host.embedding.is_none()
            || !host.config.plugins.ai.enabled
            || cancel.is_cancelled()
            || host.cancel.is_cancelled()
        {
            let mut queue = self
                .queue
                .lock()
                .map_err(|_| anyhow::anyhow!("sticker queue unavailable"))?;
            queue.jobs.clear();
            queue.pending.clear();
            return Ok(());
        }
        // Alternate priority so a busy group's import queue cannot starve a
        // model-change rebuild. Each tick still performs at most one image call.
        if self.turns.fetch_add(1, Ordering::Relaxed).is_multiple_of(2)
            && self.reindex_one(host, cancel).await?
        {
            return Ok(());
        }
        let job = self
            .queue
            .lock()
            .map_err(|_| anyhow::anyhow!("sticker queue unavailable"))?
            .jobs
            .pop_front();
        let Some(job) = job else {
            return self.reindex_one(host, cancel).await.map(|_| ());
        };
        let _pending = Pending {
            queue: &self.queue,
            identity: (job.key.key(), job.resource.clone()),
        };
        let result = tokio::select! {
            biased;
            _=cancel.cancelled()=>Err(anyhow::anyhow!("sticker import cancelled")),
            _=host.cancel.cancelled()=>Err(anyhow::anyhow!("sticker import cancelled")),
            result=tokio::time::timeout(Duration::from_secs(45),self.collect(host,&job,cancel))=>match result {Ok(result)=>result,Err(_)=>Err(anyhow::anyhow!("sticker import timed out"))},
        };
        if result.is_err() {
            tracing::warn!(
                reason = "bounded_import_failed",
                "sticker collection skipped"
            );
        }
        // A failed import is not retried indefinitely or allowed to stall chat.
        Ok(())
    }

    async fn collect(&self, host: &Host, job: &Job, cancel: &CancellationToken) -> Result<()> {
        let embedding = host
            .embedding
            .as_ref()
            .context("sticker embedding disabled")?;
        let generation = host.milky.scope_generation(&job.key.scope)?;
        self.validate_job(job).await?;
        // Verify the top-level image classification and resolve its URL in one
        // bounded lookup. Each retry rechecks the same message and resource.
        let url = host
            .milky
            .get_image_resource_temp_url(&job.key, &job.resource, job.automatic)
            .await?;
        let original = super::vision::download(&url).await?;
        let mime = match image::guess_format(&original)? {
            image::ImageFormat::Png => "image/png",
            image::ImageFormat::Jpeg => "image/jpeg",
            image::ImageFormat::Gif => "image/gif",
            image::ImageFormat::WebP => "image/webp",
            _ => anyhow::bail!("sticker image format unsupported"),
        }
        .to_owned();
        let data = super::vision::decode(original.clone(), cancel).await?;
        let thumbnail = base64::engine::general_purpose::STANDARD.decode(
            data.strip_prefix("data:image/jpeg;base64,")
                .context("sticker thumbnail invalid")?,
        )?;
        ensure!(
            original.len() <= SOURCE_LIMIT && thumbnail.len() <= THUMB_LIMIT,
            "sticker image too large"
        );
        host.milky.check_generation(generation)?;
        let content_hash = hash(&original);
        let fingerprint = embedding.fingerprint().to_owned();
        let dimensions = embedding.dimensions();
        let paths = self.paths(&job.key.scope, &content_hash)?;
        let prior_job = job.clone();
        let prior_hash = content_hash.clone();
        let prior_space = fingerprint.clone();
        let prior=self.store.call(move|db|{
            check_job(db,&prior_job)?;
            check_not_deleted(db,&prior_job,&prior_hash)?;
            Ok(db.query_row("SELECT id FROM ai_stickers WHERE scope=?1 AND hash=?2 AND fingerprint=?3 AND dimensions=?4",params![prior_job.key.scope.key(),prior_hash,prior_space,dimensions],|r|r.get::<_,i64>(0)).optional()?)
        }).await?;
        if let Some(id) = prior {
            write_blob(&paths.0, &original).await?;
            write_blob(&paths.1, &thumbnail).await?;
            host.milky.check_generation(generation)?;
            let job = job.clone();
            let content_hash = content_hash.clone();
            return self.store.call(move|db|{
                let tx=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;check_job(&tx,&job)?;check_not_deleted(&tx,&job,&content_hash)?;
                ensure!(tx.query_row("SELECT EXISTS(SELECT 1 FROM ai_stickers WHERE id=?1 AND scope=?2 AND hash=?3 AND fingerprint=?4 AND dimensions=?5)",params![id,job.key.scope.key(),content_hash,fingerprint,dimensions],|r|r.get::<_,bool>(0))?,"sticker duplicate changed");
                attach_source(&tx,id,&job)?;tx.commit()?;Ok(())
            }).await;
        }
        let capacity_job = job.clone();
        let capacity_config = self.config.clone();
        let prospective_bytes = original.len() + thumbnail.len();
        let capacity_hash = content_hash.clone();
        self.store
            .call(move |db| {
                check_job(db, &capacity_job)?;
                check_not_deleted(db, &capacity_job, &capacity_hash)?;
                check_new_capacity(
                    db,
                    &capacity_job.key.scope,
                    &capacity_hash,
                    &capacity_config,
                    prospective_bytes,
                )
            })
            .await?;
        let response = embedding
            .embed_jpeg(&thumbnail, cancel, async {
                host.milky.check_generation(generation)?;
                self.admit_job(job, &content_hash).await
            })
            .await?;
        self.usage(embedding.fingerprint(), response.input_tokens)
            .await?;
        host.milky.check_generation(generation)?;
        ensure!(
            !cancel.is_cancelled() && !host.cancel.is_cancelled(),
            "sticker import cancelled"
        );
        self.validate_job(job).await?;
        let vector = encode_vector(&response.vector)?;
        ensure!(
            response.vector.len() == dimensions,
            "sticker embedding dimensions changed"
        );
        write_blob(&paths.0, &original).await?;
        write_blob(&paths.1, &thumbnail).await?;
        host.milky.check_generation(generation)?;
        let job = job.clone();
        let config = self.config.clone();
        let image = Import {
            hash: content_hash,
            mime,
            original_bytes: original.len(),
            thumbnail_bytes: thumbnail.len(),
            thumbnail_hash: hash(&thumbnail),
            fingerprint,
            dimensions,
            vector,
        };
        self.store
            .call(move |db| commit_import(db, &job, &image, &config))
            .await
    }

    /// Rebuild at most one existing thumbnail after an embedding-space change.
    /// No old chat text is loaded and no new provenance is granted by reindexing.
    async fn reindex_one(&self, host: &Host, cancel: &CancellationToken) -> Result<bool> {
        let embedding = host
            .embedding
            .as_ref()
            .context("sticker embedding disabled")?;
        let fingerprint = embedding.fingerprint().to_owned();
        let dimensions = embedding.dimensions();
        let epoch = *host.epochs.get("ai").context("sticker AI inactive")?;
        let candidate = self
            .store
            .call(move |db| next_reindex(db, &fingerprint, dimensions, epoch))
            .await?;
        let Some(candidate) = candidate else {
            return Ok(false);
        };
        let result = tokio::select! {
            biased;
            _=cancel.cancelled()=>Err(anyhow::anyhow!("sticker reindex cancelled")),
            _=host.cancel.cancelled()=>Err(anyhow::anyhow!("sticker reindex cancelled")),
            result=tokio::time::timeout(Duration::from_secs(25),self.reindex(host,&candidate,cancel))=>match result {Ok(result)=>result,Err(_)=>Err(anyhow::anyhow!("sticker reindex timed out"))},
        };
        if result.is_err() {
            self.store.call(move|db|{
                db.execute("UPDATE ai_stickers SET retry_after=?4 WHERE id=?1 AND revision=?2 AND hash=?3",params![candidate.id,candidate.revision,candidate.hash,now()+300])?;Ok(())
            }).await?;
            tracing::debug!(
                reason = "bounded_reindex_failed",
                "sticker reindex deferred"
            );
        }
        Ok(true)
    }

    async fn reindex(
        &self,
        host: &Host,
        candidate: &Reindex,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let embedding = host
            .embedding
            .as_ref()
            .context("sticker embedding disabled")?;
        let generation = host.milky.scope_generation(&candidate.scope)?;
        let (_, path) = self.paths(&candidate.scope, &candidate.hash)?;
        let thumbnail = read_blob(&path, THUMB_LIMIT).await?;
        ensure!(
            thumbnail.len() == candidate.thumbnail_bytes
                && hash(&thumbnail) == candidate.thumbnail_hash,
            "sticker thumbnail changed"
        );
        let response = embedding
            .embed_jpeg(&thumbnail, cancel, async {
                host.milky.check_generation(generation)?;
                let candidate = candidate.clone();
                let cap = self.config.requests_per_hour;
                self.store
                    .call(move |db| {
                        let tx =
                            db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                        check_reindex(&tx, &candidate)?;
                        admit(&tx, cap)?;
                        tx.commit()?;
                        Ok(())
                    })
                    .await
            })
            .await?;
        self.usage(embedding.fingerprint(), response.input_tokens)
            .await?;
        host.milky.check_generation(generation)?;
        ensure!(
            !cancel.is_cancelled() && !host.cancel.is_cancelled(),
            "sticker reindex cancelled"
        );
        let candidate = candidate.clone();
        let fingerprint = embedding.fingerprint().to_owned();
        let dimensions = embedding.dimensions();
        let vector = encode_vector(&response.vector)?;
        self.store
            .call(move |db| commit_reindex(db, &candidate, &fingerprint, dimensions, &vector))
            .await
    }

    async fn validate_job(&self, job: &Job) -> Result<()> {
        let job = job.clone();
        self.store.call(move |db| check_job(db, &job)).await
    }

    async fn admit_job(&self, job: &Job, content_hash: &str) -> Result<()> {
        let job = job.clone();
        let content_hash = content_hash.to_owned();
        let cap = self.config.requests_per_hour;
        self.store
            .call(move |db| {
                let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                check_job(&tx, &job)?;
                check_not_deleted(&tx, &job, &content_hash)?;
                admit(&tx, cap)?;
                tx.commit()?;
                Ok(())
            })
            .await
    }

    async fn usage(&self, model: &str, tokens: u64) -> Result<()> {
        let model = model.to_owned();
        let tokens = i64::try_from(tokens)?;
        self.store.call(move|db|{db.execute("INSERT INTO usage_daily(day,plugin_id,purpose,model,requests,input_tokens,output_tokens) VALUES(date('now'),'ai','sticker_embedding',?1,1,?2,0) ON CONFLICT(day,plugin_id,purpose,model) DO UPDATE SET requests=requests+1,input_tokens=input_tokens+excluded.input_tokens",params![model,tokens])?;Ok(())}).await
    }

    pub async fn search(
        &self,
        host: &Host,
        scope: &Scope,
        guard: &ContextGuard,
        source_key: &str,
        query: &str,
        cancel: &CancellationToken,
    ) -> Result<SearchResult> {
        if !self.config.enabled
            || !host.config.plugins.ai.enabled
            || host.embedding.is_none()
            || scope.scene != Scene::Group
            || query.trim().is_empty()
            || !self.enabled(scope).await?
        {
            return Ok(SearchResult::without_candidates(SearchStatus::Disabled));
        }
        ensure!(query.len() <= 8192, "sticker query too large");
        let embedding = host.embedding.as_ref().expect("checked embedding");
        let generation = host.milky.scope_generation(scope)?;
        let physical = scope.clone();
        let context = guard.clone();
        let source = source_key.to_owned();
        let epoch = *host.epochs.get("ai").context("sticker AI inactive")?;
        let expected = self
            .store
            .call(move |db| {
                let expected = stamp(db, &physical, epoch)?;
                check_turn(db, &physical, &expected, &context, &source)?;
                Ok(expected)
            })
            .await?;
        let fingerprint = embedding.fingerprint().to_owned();
        let dimensions = embedding.dimensions();
        let physical = scope.clone();
        let space = fingerprint.clone();
        let availability = self
            .store
            .call(move |db| {
                let tx = db.unchecked_transaction()?;
                let status = search_availability(&tx, &physical, &space, dimensions, now())?;
                tx.commit()?;
                Ok(status)
            })
            .await?;
        if availability != SearchStatus::Ready {
            return Ok(SearchResult::without_candidates(availability));
        }
        let response = embedding
            .embed_text(query, cancel, async {
                host.milky.check_generation(generation)?;
                let scope = scope.clone();
                let guard = guard.clone();
                let source = source_key.to_owned();
                let expected = expected.clone();
                let cap = self.config.requests_per_hour;
                self.store
                    .call(move |db| {
                        let tx =
                            db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                        check_turn(&tx, &scope, &expected, &guard, &source)?;
                        admit(&tx, cap)?;
                        tx.commit()?;
                        Ok(())
                    })
                    .await
            })
            .await?;
        self.usage(&fingerprint, response.input_tokens).await?;
        host.milky.check_generation(generation)?;
        let physical = scope.clone();
        let context = guard.clone();
        let source = source_key.to_owned();
        let rows_expected = expected.clone();
        let rows=self.store.call(move|db|{
            let tx=db.unchecked_transaction()?;check_turn(&tx,&physical,&rows_expected,&context,&source)?;
            let rows=tx.prepare("SELECT id,revision,hash,thumbnail_hash,thumbnail_bytes,vector FROM ai_stickers WHERE scope=?1 AND fingerprint=?2 AND dimensions=?3 ORDER BY id LIMIT 1024")?
                .query_map(params![physical.key(),fingerprint,dimensions],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,usize>(4)?,r.get::<_,Vec<u8>>(5)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let rows=rows.into_iter().filter(|row|check_candidate(&tx,&physical,row.0,row.1,&row.2).is_ok()).collect::<Vec<_>>();tx.commit()?;Ok(rows)
        }).await?;
        let mut ranked = rows
            .into_iter()
            .filter_map(|row| {
                let vector = decode_vector(&row.5, dimensions).ok()?;
                let score = cosine(&response.vector, &vector)?;
                Some((row.0, row.1, row.2, row.3, row.4, score))
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|a, b| b.5.total_cmp(&a.5).then(a.0.cmp(&b.0)));
        let mut candidates = Vec::new();
        for (id, revision, content_hash, thumbnail_hash, size, score) in
            ranked.into_iter().take(CANDIDATE_LIMIT)
        {
            let (_, path) = self.paths(scope, &content_hash)?;
            let Ok(thumbnail) = read_blob(&path, THUMB_LIMIT).await else {
                continue;
            };
            if thumbnail.len() != size || hash(&thumbnail) != thumbnail_hash {
                continue;
            }
            candidates.push(Candidate {
                id,
                revision,
                hash: content_hash,
                thumbnail_jpeg: thumbnail,
                score,
            });
        }
        host.milky.check_generation(generation)?;
        ensure!(
            !cancel.is_cancelled() && !host.cancel.is_cancelled(),
            "sticker search cancelled"
        );
        let physical = scope.clone();
        let context = guard.clone();
        let source = source_key.to_owned();
        self.store
            .call(move |db| {
                let tx = db.unchecked_transaction()?;
                check_turn(&tx, &physical, &expected, &context, &source)?;
                let result = finish_search(&tx, &physical, candidates);
                tx.commit()?;
                Ok(result)
            })
            .await
    }

    pub async fn read(
        &self,
        host: &Host,
        scope: &Scope,
        candidate: &Candidate,
    ) -> Result<StoredImage> {
        ensure!(
            self.config.enabled
                && host.config.plugins.ai.enabled
                && host.embedding.is_some()
                && !host.cancel.is_cancelled(),
            "sticker library disabled"
        );
        let generation = host.milky.scope_generation(scope)?;
        let physical = scope.clone();
        let id = candidate.id;
        let revision = candidate.revision;
        let content_hash = candidate.hash.clone();
        let (mime, size) = self
            .store
            .call(move |db| {
                check_candidate(db, &physical, id, revision, &content_hash)?;
                Ok(db.query_row(
                    "SELECT mime,original_bytes FROM ai_stickers WHERE id=?1",
                    [id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, usize>(1)?)),
                )?)
            })
            .await?;
        let (path, _) = self.paths(scope, &candidate.hash)?;
        let bytes = read_blob(&path, SOURCE_LIMIT).await?;
        ensure!(
            bytes.len() == size && hash(&bytes) == candidate.hash,
            "sticker file changed"
        );
        let physical = scope.clone();
        let content_hash = candidate.hash.clone();
        self.store
            .call(move |db| check_candidate(db, &physical, id, revision, &content_hash))
            .await?;
        host.milky.check_generation(generation)?;
        Ok(StoredImage { bytes, mime })
    }

    pub async fn revoke(&self, source_key: &str) -> Result<()> {
        ensure!(
            !source_key.is_empty() && source_key.len() <= 128,
            "invalid sticker source key"
        );
        let source = source_key.to_owned();
        self.store
            .call(move |db| {
                let tx = db.transaction()?;
                tx.execute(
                    "INSERT OR IGNORE INTO ai_sticker_revoked(source_key,revoked_at) VALUES(?1,?2)",
                    params![source, now()],
                )?;
                tx.execute(
                    "DELETE FROM ai_sticker_sources WHERE source_key=?1",
                    [source],
                )?;
                tx.commit()?;
                Ok(())
            })
            .await
    }

    fn paths(&self, scope: &Scope, content_hash: &str) -> Result<(PathBuf, PathBuf)> {
        group(scope)?;
        ensure!(valid_hash(content_hash), "invalid sticker content hash");
        let directory = self.directory.join(hash(scope.key().as_bytes()));
        Ok((
            directory.join(format!("{content_hash}.original")),
            directory.join(format!("{content_hash}.jpg")),
        ))
    }

    pub async fn cleanup(&self) -> Result<()> {
        // Import files precede their metadata commit. The same worker lock keeps
        // maintenance from mistaking that short interval for an orphaned file.
        let Ok(_worker) = self.worker.try_lock() else {
            return Ok(());
        };
        self.cleanup_unlocked().await
    }

    async fn cleanup_unlocked(&self) -> Result<()> {
        let removed=self.store.call(|db|{
            let tx=db.transaction()?;
            carry_forward_sources(&tx)?;
            tx.execute("DELETE FROM ai_sticker_sources WHERE NOT EXISTS(SELECT 1 FROM ai_conversations c WHERE c.id=ai_sticker_sources.conversation_id AND c.scope=ai_sticker_sources.scope AND c.epoch=ai_sticker_sources.epoch) OR EXISTS(SELECT 1 FROM messages m WHERE m.key=ai_sticker_sources.source_key AND (m.scope!=ai_sticker_sources.scope OR m.conversation_id!=ai_sticker_sources.conversation_id OR m.epoch!=ai_sticker_sources.epoch OR m.revoked!=0)) OR EXISTS(SELECT 1 FROM ai_revoked_sources r WHERE r.source_key=ai_sticker_sources.source_key) OR EXISTS(SELECT 1 FROM ai_sticker_revoked r WHERE r.source_key=ai_sticker_sources.source_key)",[])?;
            let removed=tx.prepare("SELECT scope,hash FROM ai_stickers s WHERE NOT EXISTS(SELECT 1 FROM ai_sticker_sources p WHERE p.sticker_id=s.id) LIMIT 128")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for (scope,hash) in &removed {tx.execute("DELETE FROM ai_stickers WHERE scope=?1 AND hash=?2",params![scope,hash])?;}
            tx.execute("DELETE FROM ai_sticker_admissions WHERE at<=?1",[now()-3600])?;
            tx.commit()?;Ok(removed)
        }).await?;
        for (scope, content_hash) in removed {
            ensure!(valid_hash(&content_hash), "invalid sticker cleanup hash");
            let directory = self.directory.join(hash(scope.as_bytes()));
            for suffix in ["original", "jpg"] {
                let _ = tokio::fs::remove_file(directory.join(format!("{content_hash}.{suffix}")))
                    .await;
            }
        }
        self.remove_orphans().await
    }

    async fn remove_orphans(&self) -> Result<()> {
        let root = match tokio::fs::symlink_metadata(&self.directory).await {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            root.is_dir() && !root.file_type().is_symlink(),
            "sticker directory invalid"
        );
        let live = self
            .store
            .call(|db| {
                Ok(db
                    .prepare("SELECT scope,hash FROM ai_stickers")?
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?
            .into_iter()
            .map(|(scope, content)| (hash(scope.as_bytes()), content))
            .collect::<HashSet<_>>();
        let mut directories = tokio::fs::read_dir(&self.directory).await?;
        let mut inspected = 0;
        let skip = self.gc_cursor.load(Ordering::Relaxed);
        while let Some(directory) = directories.next_entry().await? {
            let name = directory.file_name().to_string_lossy().into_owned();
            if !valid_hash(&name) || !directory.file_type().await?.is_dir() {
                continue;
            }
            let mut files = tokio::fs::read_dir(directory.path()).await?;
            while let Some(file) = files.next_entry().await? {
                if inspected < skip {
                    inspected += 1;
                    continue;
                }
                if inspected >= skip.saturating_add(256) {
                    self.gc_cursor.store(inspected, Ordering::Relaxed);
                    return Ok(());
                }
                inspected += 1;
                let filename = file.file_name().to_string_lossy().into_owned();
                let Some((content, suffix)) = filename.rsplit_once('.') else {
                    continue;
                };
                if valid_hash(content)
                    && matches!(suffix, "original" | "jpg")
                    && !live.contains(&(name.clone(), content.to_owned()))
                    && file.file_type().await?.is_file()
                {
                    // Only our own content-addressed files can be removed.
                    let _ = tokio::fs::remove_file(file.path()).await;
                }
            }
        }
        self.gc_cursor.store(0, Ordering::Relaxed);
        Ok(())
    }
}

fn admit(db: &Connection, cap: usize) -> Result<()> {
    db.execute(
        "DELETE FROM ai_sticker_admissions WHERE at<=?1",
        [now() - 3600],
    )?;
    let count: i64 = db.query_row("SELECT COUNT(*) FROM ai_sticker_admissions", [], |r| {
        r.get(0)
    })?;
    ensure!(count < (cap as i64), "sticker embedding quota exhausted");
    db.execute("INSERT INTO ai_sticker_admissions(at) VALUES(?1)", [now()])?;
    Ok(())
}

fn carry_forward_sources(db: &Connection) -> Result<()> {
    // Ordinary settings changes explicitly carry retained raw messages into the
    // new epoch. That existing row is the only proof allowing provenance to
    // follow; forgotten or expired sources cannot infer a new conversation epoch.
    db.execute("UPDATE ai_sticker_sources AS p SET epoch=(SELECT m.epoch FROM messages m JOIN ai_conversations c ON c.id=m.conversation_id AND c.scope=m.scope AND c.epoch=m.epoch WHERE m.key=p.source_key AND m.scope=p.scope AND m.conversation_id=p.conversation_id AND m.revoked=0) WHERE EXISTS(SELECT 1 FROM messages m JOIN ai_conversations c ON c.id=m.conversation_id AND c.scope=m.scope AND c.epoch=m.epoch WHERE m.key=p.source_key AND m.scope=p.scope AND m.conversation_id=p.conversation_id AND m.revoked=0 AND m.epoch!=p.epoch) AND NOT EXISTS(SELECT 1 FROM ai_revoked_sources r WHERE r.source_key=p.source_key) AND NOT EXISTS(SELECT 1 FROM ai_sticker_revoked r WHERE r.source_key=p.source_key)", [])?;
    Ok(())
}

fn next_reindex(
    db: &Connection,
    fingerprint: &str,
    dimensions: usize,
    epoch: i64,
) -> Result<Option<Reindex>> {
    let row=db.query_row("SELECT id,scope,revision,hash,thumbnail_hash,thumbnail_bytes FROM ai_stickers s WHERE (reindex_requested=1 OR fingerprint!=?1 OR dimensions!=?2) AND retry_after<=?3 AND NOT EXISTS(SELECT 1 FROM scope_access a WHERE a.scope=s.scope AND a.enabled=0) AND NOT EXISTS(SELECT 1 FROM scope_plugin_access a WHERE a.scope=s.scope AND a.plugin='ai' AND a.enabled=0) AND NOT EXISTS(SELECT 1 FROM ai_sticker_scopes a WHERE a.scope=s.scope AND a.enabled=0) ORDER BY retry_after,id LIMIT 1",params![fingerprint,dimensions,now()],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,usize>(5)?))).optional()?;
    let Some((id, scope, revision, hash, thumbnail_hash, thumbnail_bytes)) = row else {
        return Ok(None);
    };
    let (bot, peer) = scope
        .split_once(":group:")
        .context("sticker scope invalid")?;
    let scope = Scope {
        bot_id: bot.parse()?,
        scene: Scene::Group,
        peer_id: peer.parse()?,
    };
    let candidate = Reindex {
        stamp: stamp(db, &scope, epoch)?,
        scope,
        id,
        revision,
        hash,
        thumbnail_hash,
        thumbnail_bytes,
    };
    check_reindex(db, &candidate)?;
    Ok(Some(candidate))
}

fn check_reindex(db: &Connection, candidate: &Reindex) -> Result<()> {
    ensure!(
        stamp(db, &candidate.scope, candidate.stamp.plugin_epoch)? == candidate.stamp,
        "sticker reindex permissions changed"
    );
    check_stored(
        db,
        &candidate.scope,
        candidate.id,
        candidate.revision,
        &candidate.hash,
    )?;
    ensure!(db.query_row("SELECT EXISTS(SELECT 1 FROM ai_stickers WHERE id=?1 AND thumbnail_hash=?2 AND thumbnail_bytes=?3)",params![candidate.id,candidate.thumbnail_hash,candidate.thumbnail_bytes],|r|r.get::<_,bool>(0))?,"sticker thumbnail changed");
    Ok(())
}

fn commit_reindex(
    db: &mut Connection,
    candidate: &Reindex,
    fingerprint: &str,
    dimensions: usize,
    vector: &[u8],
) -> Result<()> {
    decode_vector(vector, dimensions)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    check_reindex(&tx, candidate)?;
    ensure!(tx.execute("UPDATE ai_stickers SET fingerprint=?4,dimensions=?5,vector=?6,revision=revision+1,updated_at=?7,retry_after=0,reindex_requested=0 WHERE id=?1 AND revision=?2 AND hash=?3",params![candidate.id,candidate.revision,candidate.hash,fingerprint,dimensions,vector,now()])?==1,"sticker changed before reindex commit");
    tx.commit()?;
    Ok(())
}

fn check_new_capacity(
    db: &Connection,
    scope: &Scope,
    content_hash: &str,
    config: &StickerConfig,
    size: usize,
) -> Result<()> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM ai_stickers WHERE scope=?1 AND hash=?2)",
        params![scope.key(), content_hash],
        |r| r.get(0),
    )?;
    if !exists {
        check_capacity(db, scope, config, size)?;
    }
    Ok(())
}

fn check_capacity(
    db: &Connection,
    scope: &Scope,
    config: &StickerConfig,
    size: usize,
) -> Result<()> {
    let (group_count,group_bytes):(i64,i64)=db.query_row("SELECT COUNT(*),COALESCE(SUM(original_bytes+thumbnail_bytes),0) FROM ai_stickers WHERE scope=?1",[scope.key()],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let (count, bytes): (i64, i64) = db.query_row(
        "SELECT COUNT(*),COALESCE(SUM(original_bytes+thumbnail_bytes),0) FROM ai_stickers",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(
        (group_count as usize) < config.max_per_group
            && (count as usize) < config.max_total_items
            && (group_bytes as usize).saturating_add(size) <= config.max_group_bytes
            && (bytes as usize).saturating_add(size) <= config.max_total_bytes,
        "sticker library capacity exceeded"
    );
    Ok(())
}

fn commit_import(
    db: &mut Connection,
    job: &Job,
    image: &Import,
    config: &StickerConfig,
) -> Result<()> {
    ensure!(
        valid_hash(&image.hash)
            && valid_hash(&image.thumbnail_hash)
            && image.original_bytes > 0
            && image.original_bytes <= SOURCE_LIMIT
            && image.thumbnail_bytes > 0
            && image.thumbnail_bytes <= THUMB_LIMIT,
        "sticker import metadata invalid"
    );
    decode_vector(&image.vector, image.dimensions)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    check_job(&tx, job)?;
    check_not_deleted(&tx, job, &image.hash)?;
    let scope = job.key.scope.key();
    check_new_capacity(
        &tx,
        &job.key.scope,
        &image.hash,
        config,
        image.original_bytes + image.thumbnail_bytes,
    )?;
    tx.execute("INSERT INTO ai_stickers(scope,hash,mime,original_bytes,thumbnail_bytes,thumbnail_hash,fingerprint,dimensions,vector,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10) ON CONFLICT(scope,hash) DO UPDATE SET revision=revision+1,fingerprint=excluded.fingerprint,dimensions=excluded.dimensions,vector=excluded.vector,updated_at=excluded.updated_at",params![scope,image.hash,image.mime,image.original_bytes,image.thumbnail_bytes,image.thumbnail_hash,image.fingerprint,image.dimensions,image.vector,now()])?;
    let id: i64 = tx.query_row(
        "SELECT id FROM ai_stickers WHERE scope=?1 AND hash=?2",
        params![scope, image.hash],
        |r| r.get(0),
    )?;
    attach_source(&tx, id, job)?;
    tx.commit()?;
    Ok(())
}

fn attach_source(db: &Connection, id: i64, job: &Job) -> Result<()> {
    let changed=db.execute("INSERT OR IGNORE INTO ai_sticker_sources(sticker_id,source_key,scope,conversation_id,epoch) VALUES(?1,?2,?3,?4,?5)",params![id,job.key.key(),job.key.scope.key(),job.context.conversation_id,job.context.epoch])?;
    // Retain a bounded set of recent provenance keys. Old valid files do not
    // acquire an unbounded metadata tail when a popular sticker is repeated.
    db.execute("DELETE FROM ai_sticker_sources WHERE sticker_id=?1 AND source_key NOT IN(SELECT p.source_key FROM ai_sticker_sources p WHERE p.sticker_id=?1 ORDER BY p.rowid DESC LIMIT 32)",[id])?;
    if changed > 0 {
        db.execute(
            "UPDATE ai_stickers SET revision=revision+1,updated_at=?2 WHERE id=?1",
            params![id, now()],
        )?;
    }
    Ok(())
}

async fn read_blob(path: &Path, cap: usize) -> Result<Vec<u8>> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() > 0
            && metadata.len() <= cap as u64,
        "sticker file unavailable"
    );
    let file = tokio::fs::File::open(path).await?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(cap as u64 + 1).read_to_end(&mut bytes).await?;
    ensure!(bytes.len() <= cap, "sticker file exceeds limit");
    Ok(bytes)
}

async fn write_blob(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().context("sticker directory missing")?;
    tokio::fs::create_dir_all(directory).await?;
    ensure!(
        !tokio::fs::symlink_metadata(directory)
            .await?
            .file_type()
            .is_symlink(),
        "sticker directory invalid"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).await?;
    }
    if tokio::fs::try_exists(path).await? {
        ensure!(
            read_blob(path, SOURCE_LIMIT).await? == bytes,
            "sticker stored content differs"
        );
        return Ok(());
    }
    let temporary = tempfile::Builder::new()
        .prefix(".sticker-")
        .tempfile_in(directory)?;
    let mut output = tokio::fs::File::from_std(temporary.reopen()?);
    output.write_all(bytes).await?;
    output.sync_all().await?;
    drop(output);
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                read_blob(path, SOURCE_LIMIT).await? == bytes,
                "sticker stored content differs"
            );
            Ok(())
        }
        Err(_) => anyhow::bail!("sticker file commit failed"),
    }
}

#[cfg(test)]
mod tests;
