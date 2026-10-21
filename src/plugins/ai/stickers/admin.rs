//! Authenticated administration over a shared Store; never starts a Host or
//! contacts a model. The HTTP layer supplies the authenticated actor.
use super::{
    SOURCE_LIMIT, StoredImage, THUMB_LIMIT, group, hash, read_blob, source_valid, valid_hash,
};
use crate::{
    store::Store,
    types::{Scene, Scope, now},
};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const PAGE_SIZE: usize = 20;

#[derive(Debug)]
pub enum AdminError {
    Invalid(&'static str),
    NotFound(&'static str),
    Conflict(&'static str),
}
impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(s) | Self::NotFound(s) | Self::Conflict(s) => f.write_str(s),
        }
    }
}
impl std::error::Error for AdminError {}

/// Never deserialize this from request JSON: the authenticated session owns it.
#[derive(Clone, Debug)]
pub struct AdminActor {
    pub admin_id: String,
    pub session_id: String,
}

#[derive(Clone, Debug)]
pub struct IndexTarget {
    pub fingerprint: String,
    pub model: String,
    pub dimensions: usize,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum AssetKind {
    Preview,
    Original,
}

#[derive(Clone)]
pub struct StickerAdmin {
    store: Store,
    directory: PathBuf,
    target: Option<IndexTarget>,
}

impl StickerAdmin {
    pub fn new(store: Store, directory: PathBuf, target: Option<IndexTarget>) -> Result<Self> {
        if !directory.is_absolute() {
            return Err(AdminError::Invalid("sticker directory must be absolute").into());
        }
        if target.as_ref().is_some_and(|target| {
            target.fingerprint.is_empty()
                || target.fingerprint.len() > 256
                || target.model.len() > 256
                || !(64..=4096).contains(&target.dimensions)
        }) {
            return Err(AdminError::Invalid("invalid embedding target").into());
        }
        Ok(Self {
            store,
            directory,
            target,
        })
    }

    pub async fn groups(&self, page: usize) -> Result<Value> {
        let offset = page_offset(page)?;
        let target = self.target.clone();
        self.store.call(move |db| {
            let tx = db.unchecked_transaction()?;
            let total: usize = tx.query_row("SELECT COUNT(DISTINCT scope) FROM ai_stickers", [], |r|r.get(0))?;
            let scopes = tx.prepare("SELECT DISTINCT scope FROM ai_stickers ORDER BY scope LIMIT ?1 OFFSET ?2")?
                .query_map(params![PAGE_SIZE,offset], |r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let mut groups = Vec::with_capacity(scopes.len());
            for scope in scopes {
                let physical = parse_scope(&scope)?;
                let rows = index_rows(&tx, &scope)?;
                let mut bytes = 0usize; let mut pending = 0; let mut failed = 0;
                for row in &rows {
                    bytes = bytes.saturating_add(row.original_bytes).saturating_add(row.thumbnail_bytes);
                    match row.status(target.as_ref()) { "pending" => pending += 1, "failed" => failed += 1, _ => {} }
                }
                groups.push(json!({"scope":scope,"bot_id":physical.bot_id,"group_id":physical.peer_id,
                    "count":rows.len(),"bytes":bytes,"pending":pending,"failed":failed}));
            }
            tx.commit()?;
            Ok(json!({"groups":groups,"page":page,"total":total,"total_pages":pages(total),"index":index_view(target.as_ref())}))
        }).await
    }

    pub async fn items(&self, scope: &str, page: usize) -> Result<Value> {
        let scope = parse_scope(scope)?.key();
        let offset = page_offset(page)?;
        let target = self.target.clone();
        self.store.call(move |db| {
            let tx = db.unchecked_transaction()?;
            let total: usize = tx.query_row("SELECT COUNT(*) FROM ai_stickers WHERE scope=?1", [&scope], |r|r.get(0))?;
            let rows = tx.prepare("SELECT id,revision,mime,original_bytes,thumbnail_bytes,created_at,updated_at,fingerprint,dimensions,retry_after,reindex_requested FROM ai_stickers WHERE scope=?1 ORDER BY id DESC LIMIT ?2 OFFSET ?3")?
                .query_map(params![scope,PAGE_SIZE,offset], IndexRow::from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            let mut items = Vec::with_capacity(rows.len());
            for row in rows {
                items.push(json!({"id":row.id,"revision":row.revision,"mime":row.mime,
                    "bytes":row.original_bytes,"original_bytes":row.original_bytes,"thumbnail_bytes":row.thumbnail_bytes,
                    "created_at":row.created_at,"updated_at":row.updated_at,"index_status":row.status(target.as_ref()),
                    "reindex_requested":row.reindex_requested,"retry_after":row.retry_after,
                    "source_available":source_valid(&tx,&scope,row.id)?}));
            }
            tx.commit()?;
            Ok(json!({"scope":scope,"items":items,"page":page,"total":total,"total_pages":pages(total),"index":index_view(target.as_ref())}))
        }).await
    }

    pub async fn image(
        &self,
        scope: &str,
        id: i64,
        revision: Option<i64>,
        variant: AssetKind,
    ) -> Result<StoredImage> {
        let scope = parse_scope(scope)?.key();
        validate_identity(id, revision)?;
        let key = scope.clone();
        let snapshot = self
            .store
            .call(move |db| asset_snapshot(db, &key, id, revision))
            .await?;
        let directory = self.directory.join(hash(scope.as_bytes()));
        check_directory(&self.directory).await?;
        check_directory(&directory).await?;
        let (suffix, expected_hash, size, mime, cap) = match variant {
            AssetKind::Preview => (
                "jpg",
                &snapshot.thumbnail_hash,
                snapshot.thumbnail_bytes,
                "image/jpeg",
                THUMB_LIMIT,
            ),
            AssetKind::Original => (
                "original",
                &snapshot.hash,
                snapshot.original_bytes,
                snapshot.mime.as_str(),
                SOURCE_LIMIT,
            ),
        };
        let path = directory.join(format!("{}.{}", snapshot.hash, suffix));
        let bytes = read_blob(&path, cap)
            .await
            .map_err(|_| AdminError::NotFound("sticker asset unavailable"))?;
        if bytes.len() != size || hash(&bytes) != *expected_hash {
            return Err(AdminError::Conflict("sticker asset changed").into());
        }
        let mime = mime.to_owned();
        self.store
            .call(move |db| {
                let current = asset_snapshot(db, &scope, id, Some(snapshot.revision))?;
                if current.hash != snapshot.hash
                    || current.thumbnail_hash != snapshot.thumbnail_hash
                    || current.original_bytes != snapshot.original_bytes
                    || current.thumbnail_bytes != snapshot.thumbnail_bytes
                    || current.mime != snapshot.mime
                {
                    return Err(AdminError::Conflict("sticker asset changed").into());
                }
                Ok(())
            })
            .await?;
        Ok(StoredImage { bytes, mime })
    }

    pub async fn delete(
        &self,
        actor: &AdminActor,
        request_id: &str,
        scope: &str,
        id: i64,
        revision: i64,
    ) -> Result<Value> {
        let scope = parse_scope(scope)?.key();
        validate_identity(id, Some(revision))?;
        validate_actor(actor, request_id)?;
        let actor = actor.clone();
        let request_id = request_id.to_owned();
        self.store
            .call(move |db| delete_entry(db, &actor, &request_id, &scope, id, revision))
            .await
    }

    pub async fn reindex(
        &self,
        actor: &AdminActor,
        request_id: &str,
        scope: &str,
        id: Option<i64>,
        revision: Option<i64>,
    ) -> Result<Value> {
        let scope = parse_scope(scope)?.key();
        validate_actor(actor, request_id)?;
        match (id, revision) {
            (Some(id), Some(revision)) => validate_identity(id, Some(revision))?,
            (None, None) => {}
            _ => {
                return Err(
                    AdminError::Invalid("single-image reindex requires its revision").into(),
                );
            }
        }
        let actor = actor.clone();
        let request_id = request_id.to_owned();
        self.store
            .call(move |db| request_reindex(db, &actor, &request_id, &scope, id, revision))
            .await
    }
}

fn pages(total: usize) -> usize {
    total.div_ceil(PAGE_SIZE).max(1)
}
fn page_offset(page: usize) -> Result<usize> {
    if !(1..=1_000_000).contains(&page) {
        return Err(AdminError::Invalid("invalid sticker page").into());
    }
    Ok((page - 1) * PAGE_SIZE)
}
fn parse_scope(value: &str) -> Result<Scope> {
    let Some((bot, peer)) = value.split_once(":group:") else {
        return Err(AdminError::Invalid("invalid group scope").into());
    };
    let bot_id = bot
        .parse::<i64>()
        .map_err(|_| AdminError::Invalid("invalid group scope"))?;
    let peer_id = peer
        .parse::<i64>()
        .map_err(|_| AdminError::Invalid("invalid group scope"))?;
    let scope = Scope {
        bot_id,
        scene: Scene::Group,
        peer_id,
    };
    if group(&scope).is_err() || scope.key() != value {
        return Err(AdminError::Invalid("invalid group scope").into());
    }
    Ok(scope)
}
fn validate_identity(id: i64, revision: Option<i64>) -> Result<()> {
    if id <= 0 || revision.is_some_and(|revision| revision <= 0) {
        return Err(AdminError::Invalid("invalid sticker identity").into());
    }
    Ok(())
}
fn validate_actor(actor: &AdminActor, request_id: &str) -> Result<()> {
    if [
        actor.admin_id.as_str(),
        actor.session_id.as_str(),
        request_id,
    ]
    .iter()
    .any(|s| s.is_empty() || s.len() > 128 || s.chars().any(char::is_control))
    {
        return Err(AdminError::Invalid("invalid administration operation").into());
    }
    Ok(())
}
fn index_view(target: Option<&IndexTarget>) -> Value {
    match target {
        Some(target) => {
            json!({"enabled":target.enabled,"model":target.model,"dimensions":target.dimensions})
        }
        None => json!({"enabled":false,"model":null,"dimensions":null}),
    }
}

struct IndexRow {
    id: i64,
    revision: i64,
    mime: String,
    original_bytes: usize,
    thumbnail_bytes: usize,
    created_at: i64,
    updated_at: i64,
    fingerprint: String,
    dimensions: usize,
    retry_after: i64,
    reindex_requested: bool,
}
impl IndexRow {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            revision: r.get(1)?,
            mime: r.get(2)?,
            original_bytes: r.get(3)?,
            thumbnail_bytes: r.get(4)?,
            created_at: r.get(5)?,
            updated_at: r.get(6)?,
            fingerprint: r.get(7)?,
            dimensions: r.get(8)?,
            retry_after: r.get(9)?,
            reindex_requested: r.get(10)?,
        })
    }
    fn status(&self, target: Option<&IndexTarget>) -> &'static str {
        let pending = self.reindex_requested
            || target.is_some_and(|t| {
                t.fingerprint != self.fingerprint || t.dimensions != self.dimensions
            });
        if pending && self.retry_after > now() {
            "failed"
        } else if pending {
            "pending"
        } else {
            "ready"
        }
    }
}
fn index_rows(db: &Connection, scope: &str) -> Result<Vec<IndexRow>> {
    Ok(db.prepare("SELECT id,revision,mime,original_bytes,thumbnail_bytes,created_at,updated_at,fingerprint,dimensions,retry_after,reindex_requested FROM ai_stickers WHERE scope=?1")?
        .query_map([scope],IndexRow::from_row)?.collect::<rusqlite::Result<Vec<_>>>()?)
}

struct AssetSnapshot {
    revision: i64,
    hash: String,
    thumbnail_hash: String,
    mime: String,
    original_bytes: usize,
    thumbnail_bytes: usize,
}
fn entry(db: &Connection, scope: &str, id: i64, revision: Option<i64>) -> Result<AssetSnapshot> {
    let row=db.query_row("SELECT revision,hash,thumbnail_hash,mime,original_bytes,thumbnail_bytes FROM ai_stickers WHERE scope=?1 AND id=?2",params![scope,id],|r|Ok(AssetSnapshot{revision:r.get(0)?,hash:r.get(1)?,thumbnail_hash:r.get(2)?,mime:r.get(3)?,original_bytes:r.get(4)?,thumbnail_bytes:r.get(5)?})).optional()?.ok_or(AdminError::NotFound("sticker not found"))?;
    if revision.is_some_and(|revision| revision != row.revision) {
        return Err(AdminError::Conflict("sticker revision changed").into());
    }
    Ok(row)
}
fn asset_snapshot(
    db: &Connection,
    scope: &str,
    id: i64,
    revision: Option<i64>,
) -> Result<AssetSnapshot> {
    let row = entry(db, scope, id, revision)?;
    if !valid_hash(&row.hash)
        || !valid_hash(&row.thumbnail_hash)
        || !matches!(
            row.mime.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        )
        || row.original_bytes == 0
        || row.original_bytes > SOURCE_LIMIT
        || row.thumbnail_bytes == 0
        || row.thumbnail_bytes > THUMB_LIMIT
    {
        return Err(AdminError::Conflict("sticker asset invalid").into());
    }
    if !source_valid(db, scope, id)? {
        return Err(AdminError::NotFound("sticker source unavailable").into());
    }
    Ok(row)
}
async fn check_directory(path: &Path) -> Result<()> {
    let value = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| AdminError::NotFound("sticker asset unavailable"))?;
    if !value.is_dir() || value.file_type().is_symlink() {
        return Err(AdminError::NotFound("sticker asset unavailable").into());
    }
    Ok(())
}

fn payload(action: &str, scope: &str, id: Option<i64>, revision: Option<i64>) -> String {
    hash(
        serde_json::to_string(&json!([action, scope, id, revision]))
            .expect("serializable operation")
            .as_bytes(),
    )
}
fn receipt(
    db: &Connection,
    actor: &AdminActor,
    request_id: &str,
    payload_hash: &str,
) -> Result<Option<Value>> {
    let row=db.query_row("SELECT payload_hash,result_json FROM ai_sticker_admin_receipts WHERE admin_id=?1 AND session_id=?2 AND request_id=?3",params![actor.admin_id,actor.session_id,request_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?;
    if let Some((stored, result)) = row {
        if stored != payload_hash {
            return Err(
                AdminError::Conflict("request identifier reused with different arguments").into(),
            );
        }
        return Ok(Some(serde_json::from_str(&result)?));
    }
    Ok(None)
}
fn save_receipt(
    db: &Connection,
    actor: &AdminActor,
    request_id: &str,
    payload_hash: &str,
    result: &Value,
) -> Result<()> {
    db.execute("INSERT INTO ai_sticker_admin_receipts(admin_id,session_id,request_id,payload_hash,result_json,created_at) VALUES(?1,?2,?3,?4,?5,?6)",params![actor.admin_id,actor.session_id,request_id,payload_hash,serde_json::to_string(result)?,now()])?;
    Ok(())
}

pub(super) fn delete_entry(
    db: &mut Connection,
    actor: &AdminActor,
    request_id: &str,
    scope: &str,
    id: i64,
    revision: i64,
) -> Result<Value> {
    let payload_hash = payload("delete", scope, Some(id), Some(revision));
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(result) = receipt(&tx, actor, request_id, &payload_hash)? {
        return Ok(result);
    }
    let item = entry(&tx, scope, id, Some(revision))?;
    // Message IDs are reception-order watermarks, independent of timestamp
    // precision. Already present sources (including queued jobs) cannot reimport.
    let cutoff: i64 = tx.query_row("SELECT COALESCE(MAX(id),0) FROM messages", [], |r| r.get(0))?;
    tx.execute("INSERT INTO ai_sticker_deletions(scope,hash,source_cutoff,deleted_at) VALUES(?1,?2,?3,?4) ON CONFLICT(scope,hash) DO UPDATE SET source_cutoff=MAX(source_cutoff,excluded.source_cutoff),deleted_at=excluded.deleted_at",params![scope,item.hash,cutoff,now()])?;
    tx.execute(
        "DELETE FROM ai_stickers WHERE scope=?1 AND id=?2 AND revision=?3",
        params![scope, id, revision],
    )?;
    let result = json!({"request_id":request_id,"scope":scope,"action":"delete","affected":1,"receipt":"已删除图片及其向量；旧消息不会重新入库，之后新收到的同图可以再次入库。"});
    save_receipt(&tx, actor, request_id, &payload_hash, &result)?;
    tx.commit()?;
    // Files remain until the existing worker-serialized orphan cleanup. No
    // synchronous unlink can race a new import using the same content path.
    Ok(result)
}

pub(super) fn request_reindex(
    db: &mut Connection,
    actor: &AdminActor,
    request_id: &str,
    scope: &str,
    id: Option<i64>,
    revision: Option<i64>,
) -> Result<Value> {
    let payload_hash = payload("reindex", scope, id, revision);
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(result) = receipt(&tx, actor, request_id, &payload_hash)? {
        return Ok(result);
    }
    if let Some(id) = id {
        entry(&tx, scope, id, revision)?;
    }
    let affected=tx.execute("UPDATE ai_stickers SET reindex_requested=1,retry_after=0,revision=revision+1,updated_at=?3 WHERE scope=?1 AND (?2 IS NULL OR id=?2)",params![scope,id,now()])?;
    let result = json!({"request_id":request_id,"scope":scope,"action":"reindex","affected":affected,"receipt":"已保存向量重建任务；服务可用时后台处理，完成前保留现有向量。"});
    save_receipt(&tx, actor, request_id, &payload_hash, &result)?;
    tx.commit()?;
    Ok(result)
}
