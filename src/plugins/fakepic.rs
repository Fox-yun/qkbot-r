//! Behavior reference: lm175/nonebot-plugin-fakepic @ 0ac846a9.
use super::render::ChatBubble;
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
                        text.push_str(&format!("[[qkimage{}]]", images.len()));
                        images.push(bytes);
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
            let body = &part[captures.get(0).unwrap().end()..];
            for line in body.split(' ').filter(|s| !s.is_empty()) {
                ensure!(bubbles.len() < 24, "一次最多24条模拟消息");
                let mut rendered = line.to_owned();
                let mut pictures = Vec::new();
                for (i, image) in images.iter().enumerate() {
                    let marker = format!("[[qkimage{i}]]");
                    if rendered.contains(&marker) {
                        rendered = rendered.replace(&marker, "");
                        pictures.push(image.clone());
                    }
                }
                bubbles.push(ChatBubble {
                    name: name.clone(),
                    text: rendered,
                    avatar: avatar.clone(),
                    images: pictures,
                    is_bot: (3_889_000_000..3_890_000_000).contains(&id),
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
}
