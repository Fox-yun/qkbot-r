//! Final text uses host-owned source URLs; model output cannot invent citations.
use super::{SearchResponse, SearchResult};
use anyhow::{Result, ensure};
use regex::Regex;
use std::{collections::BTreeMap, sync::LazyLock};

#[derive(Default, Clone)]
pub struct Citations {
    sources: BTreeMap<String, SearchResult>,
    searched: bool,
}
impl Citations {
    pub fn add(&mut self, value: &serde_json::Value) {
        self.searched = true;
        if let Ok(response) = serde_json::from_value::<SearchResponse>(value.clone()) {
            for source in response.results {
                self.sources.insert(source.source_id.clone(), source);
            }
        }
    }
    pub fn render(&self, text: &str, max_bytes: usize) -> Result<String> {
        if !self.searched {
            return Ok(text.into());
        }
        static URL: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"(?i)https?://[^\s<>\[\]\"，。；！？（）【】]+"#).unwrap()
        });
        static SOURCE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\[(s[a-zA-Z0-9_-]+)\]").unwrap());
        // Milky sends plain text: keep links clickable without exposing Markdown
        // syntax. Match only complete URLs supplied by this turn's search.
        let mut text = text.to_owned();
        for source in self.sources.values() {
            let markdown = Regex::new(&format!(
                r"\[([^\]\r\n]+)\]\({}\)",
                regex::escape(&source.url)
            ))?;
            text = markdown
                .replace_all(&text, |caps: &regex::Captures<'_>| {
                    format!("{}（{}）", &caps[1], source.url)
                })
                .into_owned();
        }
        let text = URL.replace_all(&text, |caps: &regex::Captures<'_>| {
            let candidate = &caps[0];
            if self.sources.values().any(|source| source.url == candidate) {
                return candidate.to_owned();
            }
            // An ASCII sentence/parenthesis delimiter can follow a bare URL.
            // Try exact matches at those boundaries, never a URL prefix match.
            let mut end = candidate.len();
            while candidate[..end].ends_with(['.', ',', ';', ':', '!', '?', ')', '}']) {
                end -= 1;
                if self
                    .sources
                    .values()
                    .any(|source| source.url == candidate[..end])
                {
                    return candidate.to_owned();
                }
            }
            "[未核实链接]".into()
        });
        let mut text = SOURCE
            .replace_all(&text, |caps: &regex::Captures<'_>| {
                if let Some(source) = self.sources.get(&caps[1]) {
                    format!("（{}）", source.url)
                } else {
                    "[无效来源]".into()
                }
            })
            .into_owned();
        if self.sources.is_empty() {
            text.push_str("\n\n本次联网未获得可用来源，以上内容未通过本次搜索核实。");
        }
        ensure!(
            text.len() <= max_bytes,
            "回答加来源后超过消息上限，请缩短正文并保留引用"
        );
        Ok(text)
    }
}
