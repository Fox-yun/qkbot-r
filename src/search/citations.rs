//! Final text uses host-owned source URLs; model output cannot invent citations.
use super::{SearchResponse, SearchResult};
use anyhow::{Result, ensure};
use regex::Regex;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

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
        static URL: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#"(?i)https?://[^\s<>\[\]()\"，。；！？]+"#).unwrap());
        static SOURCE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\[(s[a-zA-Z0-9_-]+)\]").unwrap());
        let mut used = BTreeSet::new();
        let text = URL.replace_all(text, |caps: &regex::Captures<'_>| {
            if let Some((id, _)) = self
                .sources
                .iter()
                .find(|(_, source)| source.url == caps[0])
            {
                format!("[{id}]")
            } else {
                "[未核实链接]".into()
            }
        });
        let mut text = SOURCE
            .replace_all(&text, |caps: &regex::Captures<'_>| {
                if self.sources.contains_key(&caps[1]) {
                    used.insert(caps[1].to_owned());
                    caps[0].to_owned()
                } else {
                    "[无效来源]".into()
                }
            })
            .into_owned();
        if self.sources.is_empty() {
            text.push_str("\n\n本次联网未获得可用来源，以上内容未通过本次搜索核实。");
        } else {
            if used.is_empty() {
                used.extend(self.sources.keys().cloned());
            }
            text.push_str("\n\n检索来源：");
            for id in used {
                let source = &self.sources[&id];
                // Provider titles are untrusted too: they cannot add hyperlinks
                // beside the validated source URL selected by the host.
                let title = URL.replace_all(&source.title, "[链接]");
                text.push_str(&format!(
                    "\n[{id}] {}\n{}",
                    super::shorten(&title, 100).replace('\n', " "),
                    source.url
                ));
            }
        }
        ensure!(
            text.len() <= max_bytes,
            "回答加来源后超过消息上限，请缩短正文并保留引用"
        );
        Ok(text)
    }
}
