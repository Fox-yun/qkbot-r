//! Behavior reference: lm175/nonebot-plugin-fakepic @ 0ac846a9.
use super::render::{ChatBubble, ChatContent};
use crate::{
    host::Host,
    plugin::{Plugin, PluginSpec},
    types::{Incoming, OutgoingMessage, Scene},
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use futures_util::StreamExt;
use regex::Regex;
use std::time::Duration;

pub struct FakePicPlugin;
fn pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"^(\d{5,10})(?:【([^】]{0,64})】)?说").unwrap())
}
fn content_parts(text: &str, images: &[(String, Vec<u8>)]) -> Vec<ChatContent> {
    let mut result = Vec::new();
    let mut remaining = text;
    while !remaining.is_empty() {
        let next = images
            .iter()
            .filter_map(|(marker, bytes)| remaining.find(marker).map(|at| (at, marker, bytes)))
            .min_by_key(|(at, _, _)| *at);
        let Some((at, marker, bytes)) = next else {
            result.push(ChatContent::Text(remaining.to_owned()));
            break;
        };
        if at > 0 {
            result.push(ChatContent::Text(remaining[..at].to_owned()));
        }
        result.push(ChatContent::Image(bytes.clone()));
        remaining = &remaining[at + marker.len()..];
    }
    result
}
fn display_name(override_name: Option<&str>, resolved: &str, id: i64) -> String {
    let name = override_name.unwrap_or(resolved);
    if name.trim().is_empty() {
        format!("QQ用户 {id}")
    } else {
        name.to_owned()
    }
}
#[async_trait]
impl Plugin for FakePicPlugin {
    fn spec(&self) -> PluginSpec {
        PluginSpec {
            id: "mock_chat_image",
            commands: &["/mock-image"],
            tools: &[],
            dependencies: &["host"],
        }
    }
    fn claims(&self, message: &Incoming) -> bool {
        message.notification.is_none()
            && (message.text.starts_with("/mock-image ")
                || message.text == "/mock-image"
                || (!message.text.contains('|') && pattern().is_match(&message.text)))
    }
    async fn command(&self, _: &Host, _: &Incoming) -> Result<String> {
        Ok("QQ号【昵称】说内容+QQ号说内容；空格分隔同一人的多条消息，支持消息中的图片。".into())
    }
    async fn reply(&self, host: &Host, message: &Incoming) -> Result<OutgoingMessage> {
        let mut text = String::new();
        let mut images = Vec::new();
        // Only placeholders inserted from actual image segments are interpreted.
        // User-written strings such as [[qkimage0]] stay ordinary text.
        let image_nonce = uuid::Uuid::new_v4();
        if let Some(segments) = message.raw_message["segments"].as_array() {
            for segment in segments {
                match segment["type"].as_str() {
                    Some("text") => text.push_str(segment["data"]["text"].as_str().unwrap_or("")),
                    Some("mention") => {
                        let id = segment["data"]["user_id"].as_i64().unwrap_or_default();
                        text.push_str(&format!("@{id}"));
                    }
                    Some("image") => {
                        ensure!(images.len() < 8, "一次最多8张图片");
                        let resource = segment["data"]["resource_id"]
                            .as_str()
                            .context("图片资源缺失")?;
                        let url = host
                            .milky
                            .get_resource_temp_url(&message.key, resource)
                            .await?;
                        let bytes = fetch(&url).await?;
                        let marker = format!("[[qkimage-{image_nonce}-{}]]", images.len());
                        text.push_str(&marker);
                        images.push((marker, bytes));
                    }
                    _ => {}
                }
            }
        } else {
            text = message.text.clone();
        }
        let text = text.strip_prefix("/mock-image").unwrap_or(&text).trim();
        if text.is_empty() {
            return Ok(OutgoingMessage::text(self.command(host, message).await?));
        }
        let mut bubbles = Vec::new();
        let mut cached: std::collections::HashMap<i64, (String, Option<Vec<u8>>)> =
            std::collections::HashMap::new();
        for part in text.split('+') {
            let part = part.trim();
            let captures = pattern()
                .captures(part)
                .context("格式：QQ号【昵称】说内容+QQ号说内容")?;
            let id: i64 = captures[1].parse()?;
            ensure!(id > 0, "QQ号无效");
            let override_name = captures
                .get(2)
                .map(|m| m.as_str())
                .filter(|s| !s.is_empty());
            let (name, avatar) = if let Some(value) = cached.get(&id) {
                value.clone()
            } else {
                let name = if let Some(name) = override_name {
                    name.to_owned()
                } else if message.key.scope.scene == Scene::Group {
                    host.milky
                        .get_member(&message.key.scope, id, false)
                        .await
                        .map(|m| {
                            if m.card.is_empty() {
                                m.nickname
                            } else {
                                m.card
                            }
                        })
                        .unwrap_or_else(|_| format!("QQ用户 {id}"))
                } else {
                    format!("QQ用户 {id}")
                };
                let avatar = fetch(&format!("https://q1.qlogo.cn/g?b=qq&nk={id}&s=100"))
                    .await
                    .ok();
                cached.insert(id, (name.clone(), avatar.clone()));
                (name, avatar)
            };
            let name = display_name(override_name, &name, id);
            let body = &part[captures.get(0).unwrap().end()..];
            for line in body.split(' ').filter(|s| !s.is_empty()) {
                ensure!(bubbles.len() < 24, "一次最多24条聊天消息");
                bubbles.push(ChatBubble {
                    name: name.clone(),
                    avatar: avatar.clone(),
                    content: content_parts(line, &images),
                });
            }
        }
        super::render::chat(host, bubbles).await
    }
}
async fn fetch(url: &str) -> Result<Vec<u8>> {
    crate::types::validate_media_uri(url)?;
    let url = reqwest::Url::parse(url)?;
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    ensure!(
        url.scheme() == "https"
            && ["qpic.cn", "qq.com", "qlogo.cn"]
                .iter()
                .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}"))),
        "图片来源不是QQ资源"
    );
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?;
    let response = client.get(url).send().await?.error_for_status()?;
    ensure!(
        response.content_length().unwrap_or(0) <= 4 * 1024 * 1024,
        "图片太大"
    );
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        ensure!(bytes.len() + chunk.len() <= 4 * 1024 * 1024, "图片太大");
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upstream_numeric_nickname_syntax_is_anchored() {
        assert!(pattern().is_match("123456【小鱼】说你好"));
        assert!(pattern().is_match("123456说你好"));
        assert!(!pattern().is_match("他说123456说你好"));
        assert!(!pattern().is_match("123说你好"));
    }
    #[test]
    fn mixed_content_keeps_order_repeats_and_literal_user_markers() {
        let images = vec![
            ("[[trusted-a]]".into(), vec![1]),
            ("[[trusted-b]]".into(), vec![2]),
        ];
        let parts = content_parts(
            "前[[trusted-b]]中[[trusted-a]]后[[qkimage0]][[trusted-a]]",
            &images,
        );
        let actual: Vec<String> = parts
            .into_iter()
            .map(|part| match part {
                ChatContent::Text(text) => format!("text:{text}"),
                ChatContent::Image(bytes) => format!("image:{}", bytes[0]),
            })
            .collect();
        assert_eq!(
            actual,
            [
                "text:前",
                "image:2",
                "text:中",
                "image:1",
                "text:后[[qkimage0]]",
                "image:1"
            ]
        );
        let adjacent = content_parts("[[trusted-a]][[trusted-b]]", &images);
        assert_eq!(adjacent.len(), 2);
        assert!(
            adjacent
                .iter()
                .all(|part| matches!(part, ChatContent::Image(_)))
        );
    }
    #[test]
    fn each_explicit_nickname_overrides_cached_name_and_empty_names_use_qq_id() {
        assert_eq!(display_name(Some("新昵称"), "旧昵称", 123456), "新昵称");
        assert_eq!(display_name(None, "群名片", 123456), "群名片");
        assert_eq!(display_name(None, "", 123456), "QQ用户 123456");
        assert_eq!(display_name(Some("  "), "", 123456), "QQ用户 123456");
    }
}
