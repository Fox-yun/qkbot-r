//! Ephemeral candidates: only thumbnails actually shown in this turn grant a send reference.
use super::{
    stickers::{self, Candidate},
    vision::{LoadedImage, Origin},
};
use crate::{host::Host, types::Scope};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use image::{ImageReader, codecs::jpeg::JpegEncoder};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Cursor};

#[derive(Default)]
pub(super) struct StickerTurn {
    candidates: BTreeMap<String, Candidate>,
    sheet: Option<String>,
    visible: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    query: String,
}

pub(super) fn query(args: Value) -> Result<String> {
    let args: Query = serde_json::from_value(args).context("表情搜索参数无效")?;
    let query = args.query.trim();
    ensure!(
        !query.is_empty() && query.chars().count() <= 256 && query.len() <= 1024,
        "表情搜索内容须为1至256字"
    );
    Ok(query.to_owned())
}

impl StickerTurn {
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
    pub(super) async fn replace(&mut self, candidates: Vec<Candidate>) -> Result<Value> {
        self.candidates.clear();
        self.sheet = None;
        self.visible = false;
        let candidates: Vec<_> = candidates.into_iter().take(3).collect();
        if candidates.is_empty() {
            return Ok(
                json!({"candidates":[],"note":"本群暂无可用候选或仍在冷却，请用文字回复。"}),
            );
        }
        let thumbnails = candidates
            .iter()
            .map(|c| c.thumbnail_jpeg.clone())
            .collect();
        let jpeg = tokio::task::spawn_blocking(move || contact_sheet(thumbnails)).await??;
        self.sheet = Some(format!(
            "data:image/jpeg;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(jpeg)
        ));
        let mut result = Vec::new();
        for (index, candidate) in candidates.into_iter().enumerate() {
            let reference = format!("s{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
            result.push(json!({"sticker_ref":reference,"position":index+1}));
            self.candidates.insert(reference, candidate);
        }
        Ok(
            json!({"candidates":result,"note":"候选从左到右对应position；看图后可选一张，或只回文字。"}),
        )
    }

    pub(super) fn image(&self) -> Option<LoadedImage> {
        self.sheet.as_ref().map(|url| LoadedImage {
            url: url.clone(),
            origin: Origin::StickerSheet,
        })
    }

    pub(super) fn observe(&mut self, messages: &[Value]) {
        if let Some(sheet) = &self.sheet {
            self.visible |= messages
                .iter()
                .filter_map(|m| m["content"].as_array())
                .flatten()
                .any(|p| p["type"] == "image_url" && p["image_url"]["url"].as_str() == Some(sheet));
        }
    }

    pub(super) async fn check(&self, host: &Host, scope: &Scope) -> Result<()> {
        let physical = scope.clone();
        let candidates: Vec<_> = self
            .candidates
            .values()
            .map(|c| (c.id, c.revision, c.hash.clone()))
            .collect();
        host.store
            .call(move |db| {
                for (id, revision, hash) in candidates {
                    stickers::check_stored(db, &physical, id, revision, &hash)?;
                }
                Ok(())
            })
            .await
    }

    fn take_reference(&self, args: &mut Value) -> Result<Option<Candidate>> {
        let args = args.as_object_mut().context("reply_message 参数格式无效")?;
        let Some(reference) = args.remove("sticker_ref").filter(|v| !v.is_null()) else {
            return Ok(None);
        };
        let reference = reference.as_str().context("sticker_ref必须是单个引用")?;
        ensure!(self.visible, "候选预览未提供给模型，请改用文字回复");
        Ok(Some(
            self.candidates
                .get(reference)
                .context("未知或已过期的表情引用")?
                .clone(),
        ))
    }

    pub(super) async fn prepare(
        &self,
        host: &Host,
        scope: &Scope,
        args: &mut Value,
    ) -> Result<Option<(Candidate, String)>> {
        let Some(candidate) = self.take_reference(args)? else {
            return Ok(None);
        };
        let Ok(image) = host.stickers.read(host, scope, &candidate).await else {
            tracing::warn!(
                stage = "sticker_prepare",
                status = "text_fallback",
                reason = "candidate_unavailable",
                "selected sticker could not be read"
            );
            return Ok(None);
        };
        let Ok(uri) = host.media.bytes(host, image.bytes, &image.mime).await else {
            tracing::warn!(
                stage = "sticker_prepare",
                status = "text_fallback",
                reason = "media_unavailable",
                "selected sticker could not be published locally"
            );
            return Ok(None);
        };
        Ok(Some((candidate, uri)))
    }
}

#[derive(Debug)]
pub(super) struct StalePreview;
impl std::fmt::Display for StalePreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("表情候选已失效")
    }
}
impl std::error::Error for StalePreview {}

fn contact_sheet(thumbnails: Vec<Vec<u8>>) -> Result<Vec<u8>> {
    ensure!(
        !thumbnails.is_empty() && thumbnails.len() <= 3,
        "invalid sticker preview count"
    );
    let mut canvas = image::RgbImage::from_pixel(
        320 * thumbnails.len() as u32,
        320,
        image::Rgb([255, 255, 255]),
    );
    for (index, bytes) in thumbnails.into_iter().enumerate() {
        ensure!(
            !bytes.is_empty() && bytes.len() <= 1024 * 1024,
            "invalid sticker preview size"
        );
        let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(1024);
        limits.max_image_height = Some(1024);
        limits.max_alloc = Some(16 * 1024 * 1024);
        reader.limits(limits);
        let image = reader.decode()?.thumbnail(312, 312).to_rgb8();
        image::imageops::replace(
            &mut canvas,
            &image,
            (index as u32 * 320 + (320 - image.width()) / 2) as i64,
            ((320 - image.height()) / 2) as i64,
        );
    }
    let mut result = Vec::new();
    JpegEncoder::new_with_quality(&mut result, 80).encode_image(&canvas)?;
    ensure!(result.len() <= 1024 * 1024, "sticker preview too large");
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_and_references_cannot_forge_authority() {
        assert!(query(json!({"query":" ","id":1})).is_err());
        assert!(query(json!({"query":"x".repeat(257)})).is_err());
        assert_eq!(query(json!({"query":"开心"})).unwrap(), "开心");
        let turn = StickerTurn::default();
        assert!(
            turn.take_reference(&mut json!({"text":"hi","sticker_ref":"s1"}))
                .is_err()
        );
        assert!(
            turn.take_reference(&mut json!({"text":"hi","sticker_ref":["s1"]}))
                .is_err()
        );
        assert!(
            turn.take_reference(&mut json!({"text":"hi"}))
                .unwrap()
                .is_none()
        );
    }
    #[tokio::test]
    async fn refs_require_exact_visible_sheet_and_are_replaced_each_search() {
        let mut bytes = vec![];
        JpegEncoder::new(&mut bytes)
            .encode_image(&image::RgbImage::new(2, 2))
            .unwrap();
        let candidate = Candidate {
            id: 1,
            revision: 1,
            hash: "a".repeat(64),
            thumbnail_jpeg: bytes,
            score: 1.0,
        };
        let mut turn = StickerTurn::default();
        let result = turn.replace(vec![candidate]).await.unwrap();
        let reference = result["candidates"][0]["sticker_ref"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            turn.take_reference(&mut json!({"sticker_ref":reference}))
                .is_err()
        );
        turn.observe(&[json!({"content":reference})]);
        assert!(!turn.visible);
        turn.observe(&[json!({"content":[{"type":"image_url","image_url":{"url":turn.sheet}}]})]);
        assert!(
            turn.take_reference(&mut json!({"sticker_ref":reference}))
                .unwrap()
                .is_some()
        );
        turn.replace(vec![]).await.unwrap();
        assert!(
            turn.take_reference(&mut json!({"sticker_ref":reference}))
                .is_err()
        );
    }
}
