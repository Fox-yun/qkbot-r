//! Opaque, expiring media URLs. The HTTP server never accepts filesystem paths.
use crate::{
    host::Host,
    types::{ImageType, Incoming, OutgoingMessage, OutgoingSegment, Scope, now},
};
use anyhow::{Context, Result, ensure};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    ffi::OsString,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;

const FILE_LIMIT: usize = 8 * 1024 * 1024;
const MEAL_MANIFEST: &str = include_str!("../assets/native/meal/menu.json");

#[derive(serde::Deserialize)]
struct BundledMeal {
    url: String,
    #[serde(default)]
    sha256: String,
}

fn bundled_meal_hash(manifest: &str, source_url: &str) -> Result<String> {
    let entries: Vec<BundledMeal> = serde_json::from_str(manifest).context("内置餐图清单无效")?;
    let entry = entries
        .into_iter()
        .find(|entry| entry.url == source_url)
        .context("图片不在内置餐图清单中")?;
    ensure!(
        entry.sha256.len() == 64
            && entry
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "内置餐图清单缺少有效校验值，请更新完整资源包"
    );
    Ok(entry.sha256)
}

fn meal_images_directory(
    override_dir: Option<OsString>,
    working_dir: &std::path::Path,
    executable: Option<&std::path::Path>,
) -> Result<PathBuf> {
    if let Some(path) = override_dir {
        ensure!(!path.is_empty(), "QKBOT_MEAL_IMAGES_DIR 不能为空");
        let path = PathBuf::from(path);
        ensure!(path.is_dir(), "QKBOT_MEAL_IMAGES_DIR 指定的餐图目录不存在");
        return Ok(path);
    }
    let mut candidates = vec![working_dir.join("assets/native/meal/images")];
    if let Some(parent) = executable.and_then(std::path::Path::parent) {
        candidates.push(parent.join("assets/native/meal/images"));
    }
    candidates.push(PathBuf::from("/usr/share/qkbot/meal-images"));
    candidates
        .into_iter()
        .find(|path| path.is_dir())
        .context("内置餐图资源目录不存在，请安装完整资源包或设置 QKBOT_MEAL_IMAGES_DIR")
}

async fn checked_meal_image(
    directory: &std::path::Path,
    sha256: &str,
) -> Result<(PathBuf, &'static str, usize)> {
    // The path component comes exclusively from the embedded manifest.
    validate_id(sha256)?;
    let path = directory.join(sha256);
    let file = tokio::fs::File::open(&path)
        .await
        .context("内置餐图文件缺失，请更新完整资源包")?;
    let metadata = file.metadata().await?;
    ensure!(
        metadata.is_file() && metadata.len() > 0 && metadata.len() <= FILE_LIMIT as u64,
        "内置餐图为空或超过8 MiB"
    );
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(FILE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= FILE_LIMIT, "内置餐图超过8 MiB");
    let mime = image_mime(&bytes).context("内置餐图格式无效")?;
    ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == sha256,
        "内置餐图校验失败，请更新完整资源包"
    );
    Ok((path, mime, bytes.len()))
}
#[derive(Clone, PartialEq, Eq)]
struct Owner {
    scope: Scope,
    plugin: String,
    source: Option<String>,
    access_revision: i64,
    plugin_revision: i64,
    epoch: i64,
}
struct Entry {
    path: PathBuf,
    mime: String,
    expires: i64,
    size: usize,
    bot_id: Option<i64>,
    owner: Option<Owner>,
    _temporary: Option<tempfile::TempPath>,
}
pub struct MediaStore {
    entries: Mutex<HashMap<String, Entry>>,
    pub rendering: Arc<Semaphore>,
    io: Semaphore,
    saved_write: tokio::sync::Mutex<()>,
}
impl Default for MediaStore {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            rendering: Arc::new(Semaphore::new(2)),
            io: Semaphore::new(2),
            saved_write: tokio::sync::Mutex::new(()),
        }
    }
}
fn directory(host: &Host) -> PathBuf {
    PathBuf::from(&host.config.nonebot.media_dir).join("native")
}
fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "媒体编号无效"
    );
    Ok(())
}
async fn temporary_file(dir: &std::path::Path, bytes: &[u8]) -> Result<tempfile::TempPath> {
    use tokio::io::AsyncWriteExt;
    tokio::fs::create_dir_all(dir).await?;
    let file = tempfile::Builder::new()
        .prefix(".qkbot-native-")
        .suffix(".media")
        .tempfile_in(dir)?;
    let mut output = tokio::fs::File::from_std(file.reopen()?);
    let path = file.into_temp_path();
    output.write_all(bytes).await?;
    output.flush().await?;
    drop(output);
    Ok(path)
}
async fn source(host: &Host, message: &Incoming, plugin: &str) -> Result<()> {
    ensure!(!host.cancel.is_cancelled(), "机器人正在停止");
    ensure!(
        host.milky.verified_bot_id() == Some(message.key.scope.bot_id),
        "机器人账号未验证"
    );
    let key = message.key.key();
    let plugin = plugin.to_owned();
    host.store
        .call(move |db| crate::catalog::check_message(db, &key, &plugin))
        .await
}
impl MediaStore {
    /// Bind generated capabilities before Milky can fetch them. An opaque URL
    /// alone never grants another plugin or another chat access to a resource.
    pub async fn bind(
        &self,
        host: &Host,
        scope: &Scope,
        plugin: &str,
        position: i64,
        message: &OutgoingMessage,
    ) -> Result<()> {
        let mut uris = vec![];
        media_uris(&message.segments, &mut uris);
        let base = reqwest::Url::parse(&format!("http://{}/", host.config.nonebot.listen))?;
        let mut ids = vec![];
        for uri in uris {
            let parsed = reqwest::Url::parse(uri)?;
            if parsed.origin() == base.origin()
                && let Some(id) = parsed.path().strip_prefix("/native-media/")
            {
                ensure!(
                    parsed.query().is_none()
                        && id.len() == 32
                        && id.bytes().all(|b| b.is_ascii_hexdigit()),
                    "媒体地址无效"
                );
                ids.push(id.to_owned());
            }
        }
        if ids.is_empty() {
            return Ok(());
        }
        ensure!(
            !host.cancel.is_cancelled() && host.milky.verified_bot_id() == Some(scope.bot_id),
            "媒体所属账号已失效"
        );
        let physical = scope.clone();
        let plugin = plugin.to_owned();
        let epoch = *host.epochs.get(&plugin).context("媒体所属插件未启用")?;
        let owner = host
            .store
            .call(move |db| {
                let access_revision = crate::access::require_enabled(db, &physical.key())?;
                let plugin_revision =
                    crate::catalog::allowed_revision(db, &physical.key(), &plugin)?
                        .context("媒体所属插件已关闭")?;
                crate::host::check_plugin(db, &plugin, epoch)?;
                let source = if position > 0 {
                    let key: String = db.query_row(
                        "SELECT key FROM messages WHERE id=?1 AND scope=?2",
                        rusqlite::params![position, physical.key()],
                        |r| r.get(0),
                    )?;
                    crate::access::message_revision(db, &physical.key(), &key)?;
                    if plugin != "host" && plugin != "ai" {
                        crate::catalog::check_message(db, &key, &plugin)?;
                    }
                    Some(key)
                } else {
                    None
                };
                Ok(Owner {
                    scope: physical,
                    plugin,
                    source,
                    access_revision,
                    plugin_revision,
                    epoch,
                })
            })
            .await?;
        let mut entries = self.entries.lock().unwrap();
        for id in &ids {
            let entry = entries.get(id).context("媒体不存在或已过期")?;
            ensure!(
                entry.expires > now() && entry.bot_id == Some(scope.bot_id),
                "媒体所属账号已变化或资源已过期"
            );
            ensure!(
                entry.owner.as_ref().is_none_or(|prior| prior == &owner),
                "媒体不能跨任务复用"
            );
        }
        for id in ids {
            entries.get_mut(&id).expect("validated media entry").owner = Some(owner.clone());
        }
        Ok(())
    }
    pub async fn bytes(&self, host: &Host, bytes: Vec<u8>, mime: &str) -> Result<String> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= 20 * 1024 * 1024,
            "媒体为空或超过20 MiB"
        );
        ensure!(
            matches!(
                mime,
                "image/png"
                    | "image/jpeg"
                    | "image/gif"
                    | "image/webp"
                    | "audio/mpeg"
                    | "audio/ogg"
                    | "audio/wav"
                    | "audio/silk"
                    | "audio/pcm"
                    | "audio/aac"
                    | "video/mp4"
            ),
            "媒体格式不支持"
        );
        let _permit = self.io.acquire().await?;
        let dir = directory(host).join("temporary");
        let temporary = temporary_file(&dir, &bytes).await?;
        self.register(
            host,
            temporary.to_path_buf(),
            mime,
            Some(temporary),
            bytes.len(),
        )
        .await
    }
    async fn register(
        &self,
        host: &Host,
        path: PathBuf,
        mime: &str,
        temporary: Option<tempfile::TempPath>,
        size: usize,
    ) -> Result<String> {
        self.cleanup(host).await;
        ensure!(!host.cancel.is_cancelled(), "机器人正在停止");
        let id = uuid::Uuid::new_v4().simple().to_string();
        {
            let mut entries = self.entries.lock().unwrap();
            ensure!(
                entries.len() < 64
                    && entries.values().map(|e| e.size).sum::<usize>() + size <= 64 * 1024 * 1024,
                "媒体缓存已满，请稍后重试"
            );
            entries.insert(
                id.clone(),
                Entry {
                    path,
                    mime: mime.into(),
                    expires: now() + 300,
                    size,
                    bot_id: host.milky.verified_bot_id(),
                    owner: None,
                    _temporary: temporary,
                },
            );
        }
        Ok(format!(
            "http://{}/native-media/{id}",
            host.config.nonebot.listen
        ))
    }
    pub async fn png(&self, host: &Host, bytes: Vec<u8>) -> Result<OutgoingMessage> {
        ensure!(
            bytes.len() <= FILE_LIMIT && bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "绘图输出无效或过大"
        );
        let _permit = self.io.acquire().await?;
        let dir = directory(host).join("temporary");
        let temporary = temporary_file(&dir, &bytes).await?;
        let uri = self
            .register(
                host,
                temporary.to_path_buf(),
                "image/png",
                Some(temporary),
                bytes.len(),
            )
            .await?;
        Ok(OutgoingMessage {
            segments: vec![OutgoingSegment::Image {
                uri,
                sub_type: ImageType::Normal,
                summary: None,
            }],
        })
    }
    /// Only QQ-provided image URLs may be imported from chat commands.
    pub async fn persist_url(
        &self,
        host: &Host,
        message: &Incoming,
        plugin: &str,
        url: &str,
    ) -> Result<String> {
        source(host, message, plugin).await?;
        crate::types::validate_media_uri(url)?;
        let parsed = reqwest::Url::parse(url)?;
        let hostname = parsed.host_str().unwrap_or("").to_ascii_lowercase();
        ensure!(
            parsed.scheme() == "https"
                && ["qpic.cn", "qq.com", "qq.com.cn", "qlogo.cn"]
                    .iter()
                    .any(|suffix| hostname == *suffix || hostname.ends_with(&format!(".{suffix}"))),
            "仅可保存 QQ 消息中的图片资源"
        );
        let _permit = self.io.acquire().await?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()?;
        let response = client.get(parsed).send().await?.error_for_status()?;
        ensure!(
            response.content_length().unwrap_or(0) <= FILE_LIMIT as u64,
            "图片超过8 MiB"
        );
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(part) = stream.next().await {
            let part = part?;
            ensure!(bytes.len() + part.len() <= FILE_LIMIT, "图片超过8 MiB");
            bytes.extend_from_slice(&part);
        }
        ensure!(image_mime(&bytes).is_some(), "资源不是支持的图片");
        source(host, message, plugin).await?;
        let id = format!("{:x}", Sha256::digest(&bytes));
        // Serialize quota checks and writes, including identical hashes.
        let _write = self.saved_write.lock().await;
        source(host, message, plugin).await?;
        let dir = directory(host).join("saved");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(&id);
        if !tokio::fs::try_exists(&path).await? {
            let mut scan = tokio::fs::read_dir(&dir).await?;
            let mut total = 0u64;
            let mut count = 0usize;
            while let Some(entry) = scan.next_entry().await? {
                let meta = entry.metadata().await?;
                if meta.is_file() {
                    total += meta.len();
                    count += 1;
                }
            }
            ensure!(
                count < 512 && total + bytes.len() as u64 <= 256 * 1024 * 1024,
                "自定义图片库已满"
            );
            let temporary = temporary_file(&dir, &bytes).await?;
            temporary
                .persist_noclobber(path)
                .map_err(|_| anyhow::anyhow!("无法保存图片资源"))?;
        }
        Ok(id)
    }
    pub async fn url(
        &self,
        host: &Host,
        message: &Incoming,
        plugin: &str,
        id: &str,
    ) -> Result<String> {
        source(host, message, plugin).await?;
        validate_id(id)?;
        let path = directory(host).join("saved").join(id);
        let size = tokio::fs::metadata(&path)
            .await
            .context("保存的图片不存在")?
            .len() as usize;
        ensure!(size <= FILE_LIMIT, "图片文件超限");
        let mut file = tokio::fs::File::open(&path).await?;
        let mut prefix = [0u8; 16];
        use tokio::io::AsyncReadExt;
        let n = file.read(&mut prefix).await?;
        self.register(
            host,
            path,
            image_mime(&prefix[..n]).context("图片格式无效")?,
            None,
            size,
        )
        .await
    }
    /// Publish only pinned, packaged original menu images. These immutable
    /// assets do not enter the administrator's imported-image quota.
    pub async fn bundled_meal(
        &self,
        host: &Host,
        message: &Incoming,
        source_url: &str,
    ) -> Result<String> {
        source(host, message, "meal").await?;
        let hash = bundled_meal_hash(MEAL_MANIFEST, source_url)?;
        let directory = meal_images_directory(
            std::env::var_os("QKBOT_MEAL_IMAGES_DIR"),
            &std::env::current_dir()?,
            std::env::current_exe().ok().as_deref(),
        )?;
        let _permit = self.io.acquire().await?;
        let (path, mime, size) = checked_meal_image(&directory, &hash).await?;
        // Disable, account switch or host reload may happen during disk I/O.
        source(host, message, "meal").await?;
        self.register(host, path, mime, None, size).await
    }
    pub async fn cleanup(&self, host: &Host) {
        let expired: Vec<_> = {
            let mut entries = self.entries.lock().unwrap();
            let ids: Vec<_> = entries
                .iter()
                .filter(|(_, e)| e.expires <= now() || host.cancel.is_cancelled())
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| entries.remove(&id))
                .collect()
        };
        drop(expired);
    }
}
fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[255, 216, 255]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}
pub(crate) fn media_uris<'a>(segments: &'a [OutgoingSegment], uris: &mut Vec<&'a str>) {
    for segment in segments {
        match segment {
            OutgoingSegment::Image { uri, .. }
            | OutgoingSegment::Record { uri }
            | OutgoingSegment::Video {
                uri,
                thumb_uri: None,
            } => uris.push(uri),
            OutgoingSegment::Video {
                uri,
                thumb_uri: Some(thumb),
            } => {
                uris.push(uri);
                uris.push(thumb);
            }
            OutgoingSegment::Forward { messages, .. } => {
                for message in messages {
                    media_uris(&message.segments, uris)
                }
            }
            _ => {}
        }
    }
}
async fn current_owner(host: &Host, owner: Owner) -> Result<()> {
    ensure!(
        !host.cancel.is_cancelled() && host.milky.verified_bot_id() == Some(owner.scope.bot_id),
        "媒体所属账号已失效"
    );
    host.store
        .call(move |db| {
            crate::access::check_revision(db, &owner.scope.key(), owner.access_revision)?;
            crate::catalog::check_revision(
                db,
                &owner.scope.key(),
                &owner.plugin,
                owner.plugin_revision,
            )?;
            crate::host::check_plugin(db, &owner.plugin, owner.epoch)?;
            if let Some(source) = owner.source {
                crate::access::message_revision(db, &owner.scope.key(), &source)?;
                if owner.plugin != "host" && owner.plugin != "ai" {
                    crate::catalog::check_message(db, &source, &owner.plugin)?;
                }
            }
            Ok(())
        })
        .await
}
pub(crate) fn local_authority(headers: &HeaderMap, host: &Host) -> bool {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case(&host.config.nonebot.listen))
}
pub async fn download(
    State(host): State<Arc<Host>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    // A DNS alias must not turn a local capability into an apparently remote URL
    // that bypasses the sender's task-ownership checks.
    if !local_authority(&headers, &host) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if host.cancel.is_cancelled() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let entry = host
        .media
        .entries
        .lock()
        .unwrap()
        .get(&id)
        .filter(|e| e.expires > now())
        .and_then(|e| {
            e.owner
                .clone()
                .map(|owner| (e.path.clone(), e.mime.clone(), e.size, owner))
        });
    let Some((path, mime, size, owner)) = entry else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if current_owner(&host, owner.clone()).await.is_err() {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::File::open(path).await {
        Ok(file) => {
            use tokio::io::AsyncReadExt;
            if file.metadata().await.map(|m| m.len()).ok() != Some(size as u64)
                || current_owner(&host, owner).await.is_err()
            {
                return StatusCode::NOT_FOUND.into_response();
            }
            (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-store".into()),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
                ],
                axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(
                    file.take(size as u64),
                )),
            )
                .into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, types::Scene};
    use std::sync::atomic::{AtomicI64, Ordering};

    #[test]
    fn bundled_meal_urls_are_exact_manifest_entries_not_paths_or_redirects() {
        let entries: Vec<BundledMeal> = serde_json::from_str(MEAL_MANIFEST).unwrap();
        let first = &entries[0];
        assert_eq!(
            bundled_meal_hash(MEAL_MANIFEST, &first.url).unwrap(),
            first.sha256
        );
        for invalid in [
            "file:///etc/passwd".to_owned(),
            "../../config.toml".to_owned(),
            format!("{}?other=1", first.url),
            first
                .url
                .replace("raw.githubusercontent.com", "example.test"),
        ] {
            assert!(bundled_meal_hash(MEAL_MANIFEST, &invalid).is_err());
        }
        let invalid = serde_json::json!([{"url":"fixture","sha256":"../../private"}]);
        assert!(bundled_meal_hash(&invalid.to_string(), "fixture").is_err());
    }

    #[test]
    fn bundled_meal_directory_override_never_falls_back() {
        let temp = tempfile::tempdir().unwrap();
        let working = temp.path().join("working");
        let cwd_images = working.join("assets/native/meal/images");
        std::fs::create_dir_all(&cwd_images).unwrap();
        let executable = temp.path().join("app/qkbot");
        let executable_images = executable
            .parent()
            .unwrap()
            .join("assets/native/meal/images");
        std::fs::create_dir_all(&executable_images).unwrap();
        assert_eq!(
            meal_images_directory(None, &working, Some(&executable)).unwrap(),
            cwd_images
        );
        assert_eq!(
            meal_images_directory(None, &temp.path().join("other"), Some(&executable)).unwrap(),
            executable_images
        );
        assert_eq!(
            meal_images_directory(Some(executable_images.clone().into()), &working, None).unwrap(),
            executable_images
        );
        assert!(
            meal_images_directory(Some(temp.path().join("missing").into()), &working, None)
                .is_err()
        );
        assert!(meal_images_directory(Some(OsString::new()), &working, None).is_err());
    }

    #[tokio::test]
    async fn bundled_meal_assets_are_complete_decodable_and_content_addressed() {
        let entries: Vec<BundledMeal> = serde_json::from_str(MEAL_MANIFEST).unwrap();
        assert_eq!(entries.len(), 151);
        let images = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/native/meal/images");
        for entry in entries {
            let hash = bundled_meal_hash(MEAL_MANIFEST, &entry.url).unwrap();
            let (path, _, size) = checked_meal_image(&images, &hash).await.unwrap();
            assert!(size > 0 && size <= FILE_LIMIT);
            let bytes = std::fs::read(path).unwrap();
            let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
                .with_guessed_format()
                .unwrap();
            let mut limits = image::Limits::default();
            limits.max_alloc = Some(64 * 1024 * 1024);
            reader.limits(limits);
            let decoded = reader.decode().unwrap();
            assert!(decoded.width() > 0 && decoded.height() > 0);
        }
    }

    #[tokio::test]
    async fn bundled_meal_missing_corrupt_wrong_format_and_oversize_files_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let entries: Vec<BundledMeal> = serde_json::from_str(MEAL_MANIFEST).unwrap();
        let hash = &entries[0].sha256;
        assert!(
            checked_meal_image(temp.path(), hash)
                .await
                .unwrap_err()
                .to_string()
                .contains("缺失")
        );
        std::fs::write(temp.path().join(hash), b"\xff\xd8\xffwrong content").unwrap();
        assert!(
            checked_meal_image(temp.path(), hash)
                .await
                .unwrap_err()
                .to_string()
                .contains("校验")
        );
        let invalid = b"<html>upstream error</html>";
        let invalid_hash = format!("{:x}", Sha256::digest(invalid));
        std::fs::write(temp.path().join(&invalid_hash), invalid).unwrap();
        assert!(
            checked_meal_image(temp.path(), &invalid_hash)
                .await
                .unwrap_err()
                .to_string()
                .contains("格式")
        );
        let large = std::fs::File::create(temp.path().join(hash)).unwrap();
        large.set_len(FILE_LIMIT as u64 + 1).unwrap();
        drop(large);
        assert!(
            checked_meal_image(temp.path(), hash)
                .await
                .unwrap_err()
                .to_string()
                .contains("8 MiB")
        );
        assert!(
            checked_meal_image(temp.path(), "../config.toml")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn generated_capability_requires_binding_and_survives_neither_toggle_nor_account_change()
    {
        let directory = tempfile::tempdir().unwrap();
        let account = Arc::new(AtomicI64::new(123));
        let state = account.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let cancel = tokio_util::sync::CancellationToken::new();
        let shutdown = cancel.clone();
        let peer = tokio::spawn(async move {
            let app=axum::Router::new().route("/api/get_login_info",axum::routing::post(move|| {
                let state=state.clone();async move {axum::Json(serde_json::json!({"status":"ok","retcode":0,"data":{"uin":state.load(Ordering::SeqCst),"nickname":"fixture"}}))}
            }));
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        let mut config = Config::default();
        config.plugins.ai.enabled = false;
        config.plugins.native.enabled = vec!["meal".into()];
        config.app.database = directory
            .path()
            .join("fixture.sqlite")
            .to_string_lossy()
            .into();
        config.nonebot.media_dir = directory.path().join("media").to_string_lossy().into();
        config.milky.url = url;
        let host = Host::create(config).await.unwrap();
        host.milky.login().await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.config.nonebot.listen.parse().unwrap());
        let uri = host
            .media
            .bytes(&host, b"\x89PNG\r\n\x1a\nfixture".to_vec(), "image/png")
            .await
            .unwrap();
        let id = uri.rsplit('/').next().unwrap().to_owned();
        let path = host.media.entries.lock().unwrap()[&id].path.clone();
        let get = |headers: HeaderMap| download(State(host.clone()), Path(id.clone()), headers);
        assert_eq!(get(headers.clone()).await.status(), StatusCode::NOT_FOUND);
        let scope = Scope {
            bot_id: 123,
            scene: Scene::Group,
            peer_id: 456,
        };
        let incoming = Incoming {
            key: crate::types::MessageKey {
                scope: scope.clone(),
                message_seq: 1,
            },
            sender_id: 789,
            sender_nickname: None,
            sender_card: None,
            text: "/吃什么".into(),
            mentions: Vec::new(),
            quote_seq: None,
            received_at: now(),
            raw_message: serde_json::Value::Null,
            notification: None,
        };
        let saved = incoming.clone();
        let position = host.store.call(move |db| {
            db.execute(
                "INSERT INTO messages(key,scope,conversation_id,epoch,sender_id,role,content,received_at,command_plugin,plugin_revision,access_revision)
                 VALUES(?1,?2,'fixture',1,789,'user','/吃什么',?3,'meal',0,0)",
                rusqlite::params![saved.key.key(),saved.key.scope.key(),now()],
            )?;
            Ok(db.last_insert_rowid())
        }).await.unwrap();
        let entries: Vec<BundledMeal> = serde_json::from_str(MEAL_MANIFEST).unwrap();
        assert!(
            host.media
                .bundled_meal(&host, &incoming, "file:///private")
                .await
                .is_err()
        );
        use crate::plugin::Plugin;
        let bundled_body = crate::plugins::native::MealPlugin
            .reply(&host, &incoming)
            .await
            .unwrap();
        assert!(
            bundled_body.segments.iter().any(
                |segment| matches!(segment, OutgoingSegment::Text { text } if !text.is_empty())
            )
        );
        let images: Vec<_> = bundled_body
            .segments
            .iter()
            .filter_map(|segment| match segment {
                OutgoingSegment::Image { uri, .. } => Some(uri),
                _ => None,
            })
            .collect();
        assert_eq!(images.len(), 1);
        let bundled_uri = images[0];
        assert!(
            bundled_uri.starts_with(&format!(
                "http://{}/native-media/",
                host.config.nonebot.listen
            )),
            "the real meal reply must use packaged local media, never an upstream URL"
        );
        let bundled_id = bundled_uri.rsplit('/').next().unwrap().to_owned();
        let bundled_path = host.media.entries.lock().unwrap()[&bundled_id].path.clone();
        let get_bundled =
            |headers: HeaderMap| download(State(host.clone()), Path(bundled_id.clone()), headers);
        assert_eq!(
            get_bundled(headers.clone()).await.status(),
            StatusCode::NOT_FOUND
        );
        host.media
            .bind(&host, &scope, "meal", position, &bundled_body)
            .await
            .unwrap();
        let response = get_bundled(headers.clone()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), FILE_LIMIT)
            .await
            .unwrap();
        let downloaded_hash = format!("{:x}", Sha256::digest(&body));
        assert!(entries.iter().any(|entry| entry.sha256 == downloaded_hash));
        assert!(!directory.path().join("media/native/saved").exists());
        crate::catalog::set_enabled(&host.store, scope.clone(), "meal".into(), false)
            .await
            .unwrap();
        assert!(
            host.media
                .bundled_meal(&host, &incoming, &entries[0].url)
                .await
                .is_err()
        );
        assert_eq!(
            get_bundled(headers.clone()).await.status(),
            StatusCode::NOT_FOUND
        );
        crate::catalog::set_enabled(&host.store, scope.clone(), "meal".into(), true)
            .await
            .unwrap();
        assert_eq!(
            get_bundled(headers.clone()).await.status(),
            StatusCode::NOT_FOUND
        );
        assert!(
            host.media
                .bundled_meal(&host, &incoming, &entries[0].url)
                .await
                .is_err()
        );
        let message = OutgoingMessage {
            segments: vec![OutgoingSegment::Image {
                uri: uri.clone(),
                sub_type: ImageType::Normal,
                summary: None,
            }],
        };
        host.media
            .bind(&host, &scope, "host", 0, &message)
            .await
            .unwrap();
        assert_eq!(get(headers.clone()).await.status(), StatusCode::OK);
        let other = Scope {
            peer_id: 999,
            ..scope.clone()
        };
        assert!(
            host.media
                .bind(&host, &other, "host", 0, &message)
                .await
                .is_err()
        );
        let disguised = OutgoingMessage {
            segments: vec![OutgoingSegment::Image {
                uri: uri.replace("127.0.0.1", "127.1"),
                sub_type: ImageType::Normal,
                summary: None,
            }],
        };
        assert!(
            host.media
                .bind(&host, &other, "host", 0, &disguised)
                .await
                .is_err()
        );
        let mut alias = headers.clone();
        alias.insert(header::HOST, "127.0.0.1.example.test:3212".parse().unwrap());
        assert_eq!(get(alias).await.status(), StatusCode::NOT_FOUND);
        crate::catalog::set_enabled(&host.store, scope.clone(), "host".into(), false)
            .await
            .unwrap();
        crate::catalog::set_enabled(&host.store, scope.clone(), "host".into(), true)
            .await
            .unwrap();
        assert_eq!(get(headers.clone()).await.status(), StatusCode::NOT_FOUND);
        assert!(
            host.media
                .bind(&host, &scope, "host", 0, &message)
                .await
                .is_err()
        );
        account.store(124, Ordering::SeqCst);
        // A live transport rejects an account switch and revokes verification.
        assert!(host.milky.login().await.is_err());
        assert_eq!(host.milky.verified_bot_id(), None);
        assert_eq!(get(headers).await.status(), StatusCode::NOT_FOUND);
        host.cancel.cancel();
        host.media.cleanup(&host).await;
        assert!(!path.exists());
        assert!(
            bundled_path.is_file(),
            "cleanup must preserve packaged read-only assets"
        );
        host.store.stop().await.unwrap();
        cancel.cancel();
        peer.await.unwrap();
    }

    #[test]
    fn forwarded_video_thumbnail_is_part_of_the_capability_check() {
        let message = OutgoingMessage {
            segments: vec![OutgoingSegment::Forward {
                messages: vec![crate::types::OutgoingForwardedMessage {
                    user_id: 1,
                    sender_name: "test".into(),
                    time: None,
                    segments: vec![OutgoingSegment::Video {
                        uri: "https://example.test/video".into(),
                        thumb_uri: Some("https://example.test/thumb".into()),
                    }],
                }],
                title: None,
                summary: None,
                prompt: None,
                preview: None,
            }],
        };
        let mut uris = vec![];
        media_uris(&message.segments, &mut uris);
        assert_eq!(
            uris,
            ["https://example.test/video", "https://example.test/thumb"]
        );
    }
}
