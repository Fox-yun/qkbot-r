//! Native equivalents of sticker-saver, poke and fakemsg. Untrusted message
//! segments never grant filesystem access or a cross-conversation resource lookup.
use crate::{
    config::FakeMsgConfig,
    host::Host,
    plugin::{Plugin, PluginSpec},
    types::{
        ImageType, Incoming, MessageKey, OutgoingForwardedMessage, OutgoingMessage,
        OutgoingSegment, Scene, now,
    },
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use rusqlite::params;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub struct StickerSavePlugin;
#[async_trait]
impl Plugin for StickerSavePlugin {
    fn spec(&self) -> PluginSpec {
        PluginSpec {
            id: "sticker_save",
            commands: &[
                "/sticker",
                "/save",
                ".save",
                "/保存图片",
                "/保存表情",
                "/保存",
            ],
            tools: &[],
            dependencies: &["host"],
        }
    }
    async fn command(&self, host: &Host, message: &Incoming) -> Result<String> {
        Ok(sticker_reply(host, message).await?.text_content())
    }
    async fn reply(&self, host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
        sticker_reply(host, message).await
    }
}
async fn sticker_reply(host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
    let Some(sequence) = message.quote_seq else {
        return Ok(OutgoingMessage::text(
            "请回复一条表情或图片，再发送 /save 或 /保存表情。",
        ));
    };
    let key = MessageKey {
        scope: message.key.scope.clone(),
        message_seq: sequence,
    };
    let raw = host.milky.get_message_raw(&key).await?;
    let Some(resource) = image_resources(&raw).into_iter().next() else {
        return Ok(OutgoingMessage::text("未在回复内容中检测到表情或图片。"));
    };
    let uri = host.milky.get_resource_temp_url(&key, &resource).await?;
    // The upstream short-link service is not required; expose the verified QQ
    // temporary URL directly instead of sending chat resources to a third party.
    Ok(OutgoingMessage {
        segments: vec![
            OutgoingSegment::Text {
                text: "表情：".into(),
            },
            OutgoingSegment::Image {
                uri: uri.clone(),
                sub_type: ImageType::Normal,
                summary: None,
            },
            OutgoingSegment::Text {
                text: format!("原始链接（临时有效）：{uri}"),
            },
        ],
    })
}

fn image_resources(message: &Value) -> Vec<String> {
    message["segments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|segment| segment["type"] == "image")
        .filter_map(|segment| segment["data"]["resource_id"].as_str())
        .filter(|id| !id.is_empty() && id.len() <= 4096)
        .take(8)
        .map(str::to_owned)
        .collect()
}

pub struct MockForwardPlugin;
#[async_trait]
impl Plugin for MockForwardPlugin {
    fn spec(&self) -> PluginSpec {
        PluginSpec {
            id: "mock_forward",
            commands: &["/mock-forward"],
            tools: &[],
            dependencies: &["host"],
        }
    }
    fn claims(&self, message: &Incoming) -> bool {
        if message.notification.is_some() {
            return false;
        }
        let text = forward_input(message);
        let real_mention = message.raw_message["segments"]
            .as_array()
            .and_then(|segments| segments.first())
            .is_some_and(|segment| segment["type"] == "mention");
        text.starts_with("/mock-forward")
            || (text.contains('|') || real_mention)
                && text
                    .split_once('说')
                    .is_some_and(|(id, _)| valid_qq(id.trim()))
    }
    async fn command(&self, host: &Host, message: &Incoming) -> Result<String> {
        Ok(fake_reply(host, message).await?.text_content())
    }
    async fn reply(&self, host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
        fake_reply(host, message).await
    }
}

fn valid_qq(value: &str) -> bool {
    (6..=10).contains(&value.len())
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<i64>().is_ok_and(|id| id > 0)
}
fn forward_input(message: &Incoming) -> String {
    let Some(segments) = message.raw_message["segments"].as_array() else {
        return message.text.clone();
    };
    let mut out = String::new();
    for segment in segments {
        match segment["type"].as_str() {
            Some("text") => out.push_str(segment["data"]["text"].as_str().unwrap_or("")),
            Some("mention") => {
                if let Some(id) = segment["data"]["user_id"].as_i64() {
                    out.push_str(&id.to_string());
                }
            }
            _ => {}
        }
    }
    out
}

/// Explicit simulated lines are also used by the image renderer.
pub fn parse_mock_lines(input: &str) -> Result<Vec<(String, String)>> {
    ensure!(input.len() <= 8192, "转发内容最多8192字节");
    let mut lines = vec![];
    for part in input.split('|') {
        let (name, text) = part
            .split_once([':', '：'])
            .context("格式：名字:内容 | 名字:内容")?;
        let name = name.trim();
        let text = text.trim();
        ensure!(
            !name.is_empty()
                && name.chars().count() <= 48
                && !name.chars().any(char::is_control)
                && !text.is_empty()
                && text.chars().count() <= 1000,
            "发送者姓名或文本为空/过长"
        );
        lines.push((name.to_owned(), text.to_owned()));
        ensure!(lines.len() <= 16, "最多16条转发消息");
    }
    ensure!(!lines.is_empty(), "转发内容不能为空");
    Ok(lines)
}

fn parse_original_fake(input: &str, config: &FakeMsgConfig) -> Result<Vec<(i64, String)>> {
    ensure!(input.len() <= 8192, "转发内容最多8192字节");
    ensure!(
        !config.user_split.is_empty() && !config.message_split.is_empty(),
        "转发分隔符为空"
    );
    let mut lines = vec![];
    for user in input
        .split(&config.user_split)
        .filter(|part| !part.trim().is_empty())
    {
        let (id, text) = user
            .trim()
            .split_once('说')
            .context("消息格式错误，缺少“说”")?;
        ensure!(valid_qq(id.trim()), "QQ号须为6–10位数字");
        let id = id.trim().parse()?;
        for text in text
            .split(&config.message_split)
            .filter(|part| !part.is_empty())
        {
            ensure!(
                text.chars().count() <= 1000 && !text.contains("[CQ:"),
                "消息内容过长或包含不支持的CQ码"
            );
            lines.push((id, text.to_owned()));
            ensure!(lines.len() <= 16, "最多16条转发消息");
        }
    }
    ensure!(!lines.is_empty(), "转发内容不能为空");
    Ok(lines)
}

async fn fake_reply(host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
    let text = forward_input(message);
    let mut nodes = vec![];
    let explicit = text.strip_prefix("/mock-forward").map(str::trim);
    let original = explicit.unwrap_or(&text);
    let original_style = original
        .split_once('说')
        .is_some_and(|(id, _)| valid_qq(id.trim()));
    if let Some(input) = explicit.filter(|_| !original_style) {
        if input.trim().is_empty() {
            return Ok(OutgoingMessage::text(
                "/mock-forward 名字:内容 | 名字:内容\n原版语法：QQ号说内容|QQ号说内容；也支持真实@某人说内容。",
            ));
        }
        for (name, text) in parse_mock_lines(input.trim())? {
            nodes.push(OutgoingForwardedMessage {
                user_id: message.key.scope.bot_id,
                sender_name: name,
                time: None,
                segments: vec![OutgoingSegment::Text { text }],
            });
        }
    } else {
        let config = &host.config.plugins.native.fakemsg;
        let lines = parse_original_fake(original, config)?;
        for (id, _) in &lines {
            ensure!(
                !config.protected_ids.contains(id)
                    || host
                        .config
                        .plugins
                        .native
                        .superusers
                        .contains(&message.sender_id),
                "该用户已设置转发保护，只有机器人管理员可使用"
            );
        }
        let names: BTreeMap<i64, String> = match message.key.scope.scene {
            Scene::Group => host
                .milky
                .get_group_members(&message.key.scope, false)
                .await?
                .into_iter()
                .map(|member| {
                    (
                        member.user_id,
                        if member.card.is_empty() {
                            member.nickname
                        } else {
                            member.card
                        },
                    )
                })
                .collect(),
            Scene::Friend => {
                let data = host
                    .milky
                    .proxy_read(
                        Some(&message.key.scope),
                        "get_friend_info",
                        serde_json::json!({"user_id":message.key.scope.peer_id}),
                    )
                    .await?;
                let own = host
                    .milky
                    .proxy_read(
                        Some(&message.key.scope),
                        "get_login_info",
                        serde_json::json!({}),
                    )
                    .await?;
                BTreeMap::from([
                    (
                        message.key.scope.peer_id,
                        crate::types::sanitize_display_name(data["friend"]["nickname"].as_str())
                            .unwrap_or_else(|| message.key.scope.peer_id.to_string()),
                    ),
                    (
                        message.key.scope.bot_id,
                        crate::types::sanitize_display_name(own["nickname"].as_str())
                            .unwrap_or_else(|| message.key.scope.bot_id.to_string()),
                    ),
                ])
            }
        };
        for (id, text) in lines {
            let name = names
                .get(&id)
                .context("只可使用当前群成员或当前私聊双方的身份")?;
            nodes.push(OutgoingForwardedMessage {
                user_id: id,
                sender_name: if name.is_empty() {
                    id.to_string()
                } else {
                    name.chars().take(64).collect()
                },
                time: None,
                segments: vec![OutgoingSegment::Text { text }],
            });
        }
    }
    Ok(forward_card(nodes, message.key.scope.scene))
}

fn forward_card(messages: Vec<OutgoingForwardedMessage>, scene: Scene) -> OutgoingMessage {
    let preview = messages
        .iter()
        .take(4)
        .map(|message| {
            let text: String = message
                .segments
                .iter()
                .filter_map(|segment| match segment {
                    OutgoingSegment::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            format!("{}: {text}", message.sender_name)
                .chars()
                .filter(|ch| !ch.is_control())
                .take(256)
                .collect()
        })
        .collect();
    let summary = format!("查看{}条转发消息", messages.len());
    OutgoingMessage {
        segments: vec![OutgoingSegment::Forward {
            messages,
            title: Some(match scene {
                Scene::Group => "群聊的聊天记录".into(),
                Scene::Friend => "聊天记录".into(),
            }),
            preview: Some(preview),
            summary: Some(summary),
            prompt: Some("[聊天记录]".into()),
        }],
    }
}

pub struct NudgePlugin;
const NUDGE_MIGRATION: &str = r#"
CREATE TABLE nudge_events(source_key TEXT PRIMARY KEY,scope TEXT NOT NULL,created_at INTEGER NOT NULL,back_state TEXT NOT NULL);
CREATE INDEX nudge_cooldown ON nudge_events(scope,created_at);
CREATE TABLE nudge_images(scope TEXT NOT NULL,id TEXT NOT NULL,added_by INTEGER NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(scope,id));
"#;
#[async_trait]
impl Plugin for NudgePlugin {
    fn spec(&self) -> PluginSpec {
        PluginSpec {
            id: "nudge",
            commands: &["/zq", "/抓图"],
            tools: &[],
            dependencies: &["host"],
        }
    }
    fn claims(&self, message: &Incoming) -> bool {
        message.notification.as_ref().is_some_and(|event| {
            event.target == message.key.scope.bot_id && event.initiator != message.key.scope.bot_id
        }) || matches!(
            message.text.split_whitespace().next(),
            Some("/zq" | "/抓图")
        )
    }
    async fn handles_message(&self, host: &Host, message: &Incoming) -> Result<bool> {
        Ok(self.claims(message)
            && poke_scope_allowed(&host.config.plugins.native.poke, &message.key.scope))
    }
    async fn init(&self, host: &Host) -> Result<()> {
        host.store.migrate("nudge", 1, NUDGE_MIGRATION).await
    }
    async fn command(&self, host: &Host, message: &Incoming) -> Result<String> {
        Ok(nudge_reply(host, message).await?.text_content())
    }
    async fn reply(&self, host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
        nudge_reply(host, message).await
    }
}

fn poke_scope_allowed(config: &crate::config::PokeConfig, scope: &crate::types::Scope) -> bool {
    if config.blacklist_mode {
        scope.scene != Scene::Group || !config.blocked_groups.contains(&scope.peer_id)
    } else {
        config.allowed_groups.is_empty()
            || scope.scene == Scene::Group && config.allowed_groups.contains(&scope.peer_id)
    }
}

async fn nudge_reply(host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
    if message.notification.is_none() {
        return add_nudge_images(host, message).await;
    }
    let config = &host.config.plugins.native.poke;
    if !poke_scope_allowed(config, &message.key.scope) {
        return Ok(OutgoingMessage { segments: vec![] });
    }
    let event = message.notification.as_ref().context("missing nudge")?;
    ensure!(
        event.target == message.key.scope.bot_id && event.initiator != message.key.scope.bot_id,
        "invalid nudge recipient"
    );
    let scope = message.key.scope.key();
    let key = message.key.key();
    let cooldown = config.cooldown_secs as i64;
    let epoch = *host.epochs.get("nudge").context("plugin unavailable")?;
    let admitted = host
        .store
        .call(move |db| {
            let tx = db.transaction()?;
            crate::host::check_plugin(&tx, "nudge", epoch)?;
            crate::catalog::check_message(&tx, &key, "nudge")?;
            let last: Option<i64> = tx.query_row(
                "SELECT MAX(created_at) FROM nudge_events WHERE scope=?1",
                [&scope],
                |r| r.get(0),
            )?;
            if last.is_some_and(|last| now().saturating_sub(last) < cooldown) {
                return Ok(false);
            }
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO nudge_events VALUES(?1,?2,?3,'not_requested')",
                params![key, scope, now()],
            )?;
            tx.execute(
                "DELETE FROM nudge_events WHERE created_at<?1",
                [now() - 86400],
            )?;
            tx.commit()?;
            Ok(inserted == 1)
        })
        .await?;
    if !admitted {
        return Ok(OutgoingMessage { segments: vec![] });
    }
    if config.send_back {
        let result = send_back(host, message, event.initiator).await;
        if !matches!(
            result,
            Ok(crate::milky::ActionOutcome::Confirmed | crate::milky::ActionOutcome::Deferred)
        ) {
            tracing::warn!("nudge callback unconfirmed; not retried");
        }
    }
    let use_record =
        config.send_record && (!(config.send_image || config.send_text) || random(2) == 0);
    if use_record {
        match optional_resource(host, &config.resource_dir, "acc", false).await {
            Some(uri) => {
                return Ok(OutgoingMessage {
                    segments: vec![OutgoingSegment::Record { uri }],
                });
            }
            None => {
                tracing::debug!("nudge audio unavailable; trying configured image/text fallback")
            }
        }
    }
    let mut segments = vec![];
    if config.send_image {
        let scope = message.key.scope.key();
        let ids: Vec<String> = host
            .store
            .call(move |db| {
                Ok(db
                    .prepare("SELECT id FROM nudge_images WHERE scope=?1 ORDER BY id LIMIT 128")?
                    .query_map([scope], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?)
            })
            .await?;
        let saved = if ids.is_empty() {
            None
        } else {
            host.media
                .url(host, message, "nudge", &ids[random(ids.len())])
                .await
                .ok()
        };
        let uri = if saved.is_some() {
            saved
        } else {
            optional_resource(host, &config.resource_dir, "pic", true).await
        };
        if let Some(uri) = uri {
            segments.push(OutgoingSegment::Image {
                uri,
                sub_type: ImageType::Normal,
                summary: None,
            });
        }
    }
    if config.send_text {
        let custom = local_texts(&config.resource_dir).await.unwrap_or_default();
        let texts: Vec<&str> = if custom.is_empty() {
            vec![
                "请不要戳我 >_<",
                "戳坏了，赔钱！",
                "喂，戳我干嘛！",
                "再戳一下试试？",
                "(。´・ω・) ん?",
            ]
        } else {
            custom.iter().map(String::as_str).collect()
        };
        segments.push(OutgoingSegment::Text {
            text: texts[random(texts.len())].replace('我', &config.nickname),
        });
    }
    Ok(OutgoingMessage { segments })
}
async fn optional_resource(host: &Host, root: &str, folder: &str, image: bool) -> Option<String> {
    let (bytes, mime) = local_resource(root, folder, image).await.ok()??;
    host.media.bytes(host, bytes, mime).await.ok()
}

async fn send_back(
    host: &Host,
    message: &Incoming,
    target: i64,
) -> Result<crate::milky::ActionOutcome> {
    let _gate = host.access.delivery_guard(&message.key.scope).await?;
    let source = message.key.key();
    host.store.call(move|db|{
        crate::catalog::check_message(db,&source,"nudge")?;
        ensure!(db.execute("UPDATE nudge_events SET back_state='submitted' WHERE source_key=?1 AND back_state='not_requested'",[source])?==1,"回戳已处理");
        Ok(())
    }).await?;
    let result = host
        .milky
        .send_nudge(&message.key.scope, target, &host.cancel)
        .await;
    let state = match &result {
        Ok(crate::milky::ActionOutcome::Confirmed) => "confirmed",
        Ok(crate::milky::ActionOutcome::Deferred) => "not_submitted",
        Ok(crate::milky::ActionOutcome::Failed { .. }) | Err(_) => "failed",
        Ok(crate::milky::ActionOutcome::Unknown { .. }) => "unknown",
    };
    let source = message.key.key();
    if host
        .store
        .call(move |db| {
            db.execute(
                "UPDATE nudge_events SET back_state=?2 WHERE source_key=?1",
                params![source, state],
            )?;
            Ok(())
        })
        .await
        .is_err()
    {
        tracing::warn!("nudge callback result could not be recorded; no retry");
    }
    result
}

async fn add_nudge_images(host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
    let mut sources = vec![(message.key.clone(), message.raw_message.clone())];
    if let Some(sequence) = message.quote_seq {
        let key = MessageKey {
            scope: message.key.scope.clone(),
            message_seq: sequence,
        };
        sources.push((key.clone(), host.milky.get_message_raw(&key).await?));
    }
    let mut added = 0;
    let mut failed = 0;
    for (key, raw) in sources {
        for resource in image_resources(&raw).into_iter().take(8 - added - failed) {
            let result = async {
                let url = host.milky.get_resource_temp_url(&key, &resource).await?;
                let id = host.media.persist_url(host, message, "nudge", &url).await?;
                let scope = message.key.scope.key();
                let source = message.key.key();
                let actor = message.sender_id;
                host.store
                    .call(move |db| {
                        let tx = db.transaction()?;
                        crate::catalog::check_message(&tx, &source, "nudge")?;
                        let count: i64 = tx.query_row(
                            "SELECT COUNT(*) FROM nudge_images WHERE scope=?1",
                            [&scope],
                            |r| r.get(0),
                        )?;
                        ensure!(count < 128, "当前会话戳戳图库最多128张");
                        tx.execute(
                            "INSERT OR IGNORE INTO nudge_images VALUES(?1,?2,?3,?4)",
                            params![scope, id, actor, now()],
                        )?;
                        tx.commit()?;
                        Ok(())
                    })
                    .await
            }
            .await;
            if result.is_ok() {
                added += 1;
            } else {
                failed += 1;
            }
        }
    }
    Ok(OutgoingMessage::text(if added + failed == 0 {
        "请附带或回复图片后发送 /zq。".into()
    } else {
        format!("添加完成，成功{added}张，失败{failed}张；用于当前会话戳戳随机图。")
    }))
}

async fn local_resource(
    root: &str,
    subdir: &str,
    image: bool,
) -> Result<Option<(Vec<u8>, &'static str)>> {
    let root = PathBuf::from(root);
    if !tokio::fs::try_exists(&root).await? {
        return Ok(None);
    }
    let root = tokio::fs::canonicalize(root).await?;
    let directory = root.join(subdir);
    if !tokio::fs::try_exists(&directory).await? {
        return Ok(None);
    }
    let directory = tokio::fs::canonicalize(directory).await?;
    ensure!(directory.starts_with(&root), "资源目录越界");
    let mut scan = tokio::fs::read_dir(&directory).await?;
    let mut files = vec![];
    let mut visited = 0;
    while let Some(entry) = scan.next_entry().await? {
        visited += 1;
        if visited > 512 {
            break;
        }
        if !entry.file_type().await?.is_file() {
            continue;
        }
        let path = entry.path();
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if (image && ["png", "jpg", "jpeg", "gif", "webp"].contains(&extension.as_str()))
            || (!image && ["wav", "mp3", "aac", "acc", "ogg", "silk"].contains(&extension.as_str()))
        {
            files.push(path);
        }
    }
    if files.is_empty() {
        return Ok(None);
    }
    let path = tokio::fs::canonicalize(&files[random(files.len())]).await?;
    ensure!(path.starts_with(&directory), "资源路径越界");
    let bytes = bounded_file(&path, 20 * 1024 * 1024).await?;
    let mime = media_mime(&bytes, image).context("资源类型不支持，SVG不能作为图片发送")?;
    Ok(Some((bytes, mime)))
}

async fn local_texts(root: &str) -> Result<Vec<String>> {
    if !tokio::fs::try_exists(root).await? {
        return Ok(vec![]);
    }
    let root = tokio::fs::canonicalize(root).await?;
    let path = root.join("poke.txt");
    if !tokio::fs::try_exists(&path).await? {
        return Ok(vec![]);
    }
    let path = tokio::fs::canonicalize(path).await?;
    ensure!(path.starts_with(&root), "文本资源路径越界");
    let bytes = bounded_file(&path, 64 * 1024).await?;
    Ok(std::str::from_utf8(&bytes)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(256)
        .map(|line| line.chars().take(500).collect())
        .collect())
}
async fn bounded_file(path: &Path, limit: usize) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path).await?;
    ensure!(file.metadata().await?.len() <= limit as u64, "本地资源过大");
    let mut bytes = vec![];
    file.take(limit as u64 + 1).read_to_end(&mut bytes).await?;
    ensure!(bytes.len() <= limit, "本地资源过大");
    Ok(bytes)
}
fn media_mime(bytes: &[u8], image: bool) -> Option<&'static str> {
    if image {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some("image/png")
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            Some("image/jpeg")
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some("image/gif")
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            Some("image/webp")
        } else {
            None
        }
    } else {
        if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
            Some("audio/wav")
        } else if bytes.first() == Some(&255) && bytes.get(1).is_some_and(|byte| byte & 246 == 240)
        {
            Some("audio/aac")
        } else if bytes.starts_with(b"ID3")
            || bytes.first() == Some(&255) && bytes.get(1).is_some_and(|byte| byte & 224 == 224)
        {
            Some("audio/mpeg")
        } else if bytes.starts_with(b"OggS") {
            Some("audio/ogg")
        } else if bytes.starts_with(b"#!SILK_V3") || bytes.starts_with(b"\x02#!SILK_V3") {
            Some("audio/silk")
        } else {
            None
        }
    }
}
fn random(len: usize) -> usize {
    let id = uuid::Uuid::new_v4().into_bytes();
    (u64::from_le_bytes(id[..8].try_into().expect("eight UUID bytes")) % len as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forward_message(text: &str, scene: Scene) -> Incoming {
        Incoming {
            key: MessageKey {
                scope: crate::types::Scope {
                    bot_id: 112233,
                    scene,
                    peer_id: 123456,
                },
                message_seq: 1,
            },
            sender_id: 654321,
            sender_nickname: None,
            sender_card: None,
            text: text.into(),
            mentions: vec![],
            quote_seq: None,
            received_at: 1,
            raw_message: Value::Null,
            notification: None,
        }
    }

    fn forward_config() -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.app.database = ":memory:".into();
        config.plugins.ai.enabled = false;
        config.plugins.native.enabled.clear();
        config
    }

    #[tokio::test]
    async fn explicit_forward_contains_only_requested_nodes_and_natural_metadata() {
        let host = Host::create(forward_config()).await.unwrap();
        for (scene, title) in [
            (Scene::Group, "群聊的聊天记录"),
            (Scene::Friend, "聊天记录"),
        ] {
            let incoming = forward_message("/mock-forward 小明:你好 | 小红:第二行\n继续", scene);
            let outgoing = fake_reply(&host, &incoming).await.unwrap();
            outgoing.validate(8192).unwrap();
            assert!(outgoing.text_content().is_empty());
            assert_eq!(outgoing.segments.len(), 1);
            let value = serde_json::to_value(outgoing).unwrap();
            let data = &value["segments"][0]["data"];
            let nodes = data["messages"].as_array().unwrap();
            assert_eq!(nodes.len(), 2);
            assert_eq!(nodes[0]["user_id"], incoming.key.scope.bot_id);
            assert_eq!(nodes[0]["sender_name"], "小明");
            assert_eq!(nodes[0]["segments"][0]["data"]["text"], "你好");
            assert_eq!(nodes[1]["sender_name"], "小红");
            assert_eq!(nodes[1]["segments"][0]["data"]["text"], "第二行\n继续");
            assert_eq!(data["title"], title);
            assert_eq!(
                data["preview"],
                serde_json::json!(["小明: 你好", "小红: 第二行 继续"])
            );
            assert_eq!(data["summary"], "查看2条转发消息");
            assert_eq!(data["prompt"], "[聊天记录]");
        }
        let mut lines = vec![format!("{}:{}", "名".repeat(48), "甲".repeat(1000))];
        lines.extend((1..16).map(|index| format!("用户{index}:原文\n\t保留\0内容")));
        let incoming = forward_message(&format!("/mock-forward {}", lines.join("|")), Scene::Group);
        let outgoing = fake_reply(&host, &incoming).await.unwrap();
        outgoing.validate(8192).unwrap();
        let value = serde_json::to_value(outgoing).unwrap();
        let data = &value["segments"][0]["data"];
        assert_eq!(data["messages"].as_array().unwrap().len(), 16);
        assert_eq!(data["summary"], "查看16条转发消息");
        let preview = data["preview"].as_array().unwrap();
        assert_eq!(preview.len(), 4);
        assert_eq!(preview[0].as_str().unwrap().chars().count(), 256);
        assert!(
            preview
                .iter()
                .all(|line| !line.as_str().unwrap().chars().any(char::is_control))
        );
        assert_eq!(
            data["messages"][1]["segments"][0]["data"]["text"],
            "原文\n\t保留\0内容"
        );
        lines.push("多余:第17条".into());
        assert!(parse_mock_lines(&lines.join("|")).is_err());
        host.store.stop().await.unwrap();
    }

    #[tokio::test]
    async fn original_forward_preserves_member_identity_and_protection() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let app = axum::Router::new()
            .route("/api/get_login_info", axum::routing::post(|| async {
                axum::Json(serde_json::json!({"status":"ok","retcode":0,"data":{"uin":112233,"nickname":"机器人"}}))
            }))
            .route("/api/get_group_member_list", axum::routing::post(move || {
                let calls = observed.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({"status":"ok","retcode":0,"data":{"members":[
                        {"user_id":123456,"group_id":123456,"nickname":"原昵称","card":"群名片","role":"member"},
                        {"user_id":654321,"group_id":123456,"nickname":"张".repeat(64),"card":"","role":"member"},
                        {"user_id":777777,"group_id":123456,"nickname":"","card":"","role":"member"}
                    ]}}))
                }
            }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut config = forward_config();
        config.milky.url = url;
        config.plugins.native.fakemsg.protected_ids = vec![123456];
        config.plugins.native.superusers = vec![999999];
        let host = Host::create(config).await.unwrap();
        host.milky.login().await.unwrap();
        let mut incoming = forward_message("/mock-forward 123456说你好|654321说再见", Scene::Group);
        assert!(
            fake_reply(&host, &incoming)
                .await
                .unwrap_err()
                .to_string()
                .contains("转发保护")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        incoming.sender_id = 999999;
        // The real-mention syntax follows the same ID and name lookup path.
        incoming.raw_message = serde_json::json!({"segments":[
            {"type":"mention","data":{"user_id":123456}},
            {"type":"text","data":{"text":"说你好|654321说再见|777777说收到"}}
        ]});
        let outgoing = fake_reply(&host, &incoming).await.unwrap();
        outgoing.validate(8192).unwrap();
        let value = serde_json::to_value(outgoing).unwrap();
        let data = &value["segments"][0]["data"];
        let nodes = data["messages"].as_array().unwrap();
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0]["user_id"], 123456);
        assert_eq!(nodes[0]["sender_name"], "群名片");
        assert_eq!(nodes[1]["user_id"], 654321);
        assert_eq!(nodes[1]["sender_name"], "张".repeat(64));
        assert_eq!(nodes[2]["user_id"], 777777);
        assert_eq!(nodes[2]["sender_name"], "777777");
        assert_eq!(data["preview"][0], "群名片: 你好");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        incoming.raw_message = Value::Null;
        incoming.text = "/mock-forward 111111说不在当前群".into();
        assert!(
            fake_reply(&host, &incoming)
                .await
                .unwrap_err()
                .to_string()
                .contains("只可使用当前群成员")
        );
        server.abort();
        host.store.stop().await.unwrap();
    }

    #[test]
    fn original_forward_parser_preserves_speaker_and_per_speaker_message_split() {
        let lines = parse_original_fake(
            "123456说hello world|654321说再见",
            &FakeMsgConfig::default(),
        )
        .unwrap();
        assert_eq!(
            lines,
            vec![
                (123456, "hello".into()),
                (123456, "world".into()),
                (654321, "再见".into())
            ]
        );
        assert!(parse_original_fake("name说text", &FakeMsgConfig::default()).is_err());
        assert!(parse_original_fake("123456说[CQ:at,qq=1]", &FakeMsgConfig::default()).is_err());
        assert!(parse_mock_lines("x:1|y:2").is_ok());
        assert!(parse_mock_lines("not a pair").is_err());
    }
    #[test]
    fn image_sources_do_not_accept_arbitrary_embedded_urls_or_quoted_segments() {
        let value = serde_json::json!({"segments":[
            {"type":"image","data":{"resource_id":"real","url":"file:///secret"}},
            {"type":"reply","data":{"segments":[{"type":"image","data":{"resource_id":"fake"}}]}},
            {"type":"text","data":{"text":"https://example.org"}}
        ]});
        assert_eq!(image_resources(&value), vec!["real"]);
    }
    #[test]
    fn poke_scope_policy_matches_upstream_black_and_white_modes() {
        let group = crate::types::Scope {
            bot_id: 1,
            scene: Scene::Group,
            peer_id: 20,
        };
        let mut config = crate::config::PokeConfig::default();
        assert!(poke_scope_allowed(&config, &group));
        config.blocked_groups.push(20);
        assert!(!poke_scope_allowed(&config, &group));
        config.blacklist_mode = false;
        assert!(poke_scope_allowed(&config, &group));
        config.allowed_groups.push(21);
        assert!(!poke_scope_allowed(&config, &group));
        config.allowed_groups.push(20);
        assert!(poke_scope_allowed(&config, &group));
    }
    #[test]
    fn media_signature_detection_rejects_svg_and_mislabeled_files() {
        assert_eq!(media_mime(b"<svg xmlns='x'/>", true), None);
        assert_eq!(media_mime(b"ID3abcdef", true), None);
        assert_eq!(media_mime(b"ID3abcdef", false), Some("audio/mpeg"));
        assert_eq!(media_mime(b"GIF89a123", true), Some("image/gif"));
    }
}
