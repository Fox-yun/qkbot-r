//! Bounded search material, never instructions or evidence for long-term memory.
pub mod citations;
pub mod provider;
pub mod tavily;
#[cfg(test)]
pub(crate) mod tests;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc, time::Duration};
use tokio::sync::{OnceCell, Semaphore};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    pub enabled: bool,
    pub provider: String,
    pub api_key: String,
    pub max_results: usize,
    pub request_timeout_secs: u64,
    pub max_response_bytes: usize,
    pub max_output_bytes: usize,
    pub max_concurrent_requests: usize,
    pub max_calls_per_turn: usize,
    pub user_per_hour: usize,
    pub conversation_per_hour: usize,
    pub global_per_hour: usize,
}
impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "tavily".into(),
            api_key: String::new(),
            max_results: 5,
            request_timeout_secs: 8,
            max_response_bytes: 262144,
            max_output_bytes: 6144,
            max_concurrent_requests: 2,
            max_calls_per_turn: 1,
            user_per_hour: 10,
            conversation_per_hour: 30,
            global_per_hour: 60,
        }
    }
}
impl fmt::Debug for SearchConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SearchConfig")
            .field("enabled", &self.enabled)
            .field("provider", &self.provider)
            .field("api_key", &"[redacted]")
            .finish_non_exhaustive()
    }
}
impl SearchConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.provider == "tavily", "搜索供应商目前只支持 tavily");
        ensure!(
            self.api_key.len() <= 8192
                && reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.api_key))
                    .is_ok(),
            "搜索密钥格式无效"
        );
        ensure!(
            (1..=5).contains(&self.max_results) && (1..=12).contains(&self.request_timeout_secs),
            "搜索结果数须为 1–5，超时须为 1–12 秒"
        );
        ensure!(
            (4096..=1048576).contains(&self.max_response_bytes)
                && (2048..=8192).contains(&self.max_output_bytes),
            "搜索响应上限须为 4 KiB–1 MiB，输出上限须为 2–8 KiB"
        );
        ensure!(
            (1..=16).contains(&self.max_concurrent_requests)
                && (1..=2).contains(&self.max_calls_per_turn),
            "搜索并发须为 1–16，每轮调用须为 1–2"
        );
        ensure!(
            [
                self.user_per_hour,
                self.conversation_per_hour,
                self.global_per_hour
            ]
            .iter()
            .all(|v| (1..=100000).contains(v)),
            "搜索每小时额度须为 1–100000"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TimeRange {
    #[default]
    Any,
    Day,
    Week,
    Month,
    Year,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub time_range: TimeRange,
    #[serde(default = "default_results")]
    pub max_results: usize,
}
fn default_results() -> usize {
    5
}
impl SearchRequest {
    pub fn normalize(mut self, config: &SearchConfig) -> Result<Self> {
        self.query = self.query.trim().to_owned();
        ensure!(
            (1..=300).contains(&self.query.chars().count())
                && !self.query.chars().any(char::is_control),
            "搜索词须为 1–300 个字符，不能含控制字符"
        );
        ensure!((1..=5).contains(&self.max_results), "搜索结果数须为 1–5");
        self.max_results = self.max_results.min(config.max_results);
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub source_id: String,
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub published_at: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub provider: String,
    pub retrieved_at: String,
    pub time_range: TimeRange,
    pub results: Vec<SearchResult>,
    pub truncated: bool,
}

/// A small, safe error vocabulary. Never forward provider bodies, keys or URLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchError {
    Timeout,
    Busy,
    RateLimited,
    Unauthorized,
    TooLarge,
    InvalidResponse,
    Unavailable,
    Cancelled,
}
impl SearchError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Timeout => "search_timeout",
            Self::Busy => "search_busy",
            Self::RateLimited => "search_rate_limited",
            Self::Unauthorized => "search_unauthorized",
            Self::TooLarge => "search_response_too_large",
            Self::InvalidResponse => "search_invalid_response",
            Self::Unavailable => "search_unavailable",
            Self::Cancelled => "search_cancelled",
        }
    }
}
impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Timeout => "本次联网搜索超时，未重试",
            Self::Busy => "联网搜索繁忙，请稍后再试",
            Self::RateLimited => "搜索供应商限流或额度不足",
            Self::Unauthorized => "搜索密钥缺失或无效",
            Self::TooLarge => "搜索响应超过大小限制",
            Self::InvalidResponse => "搜索服务返回无效数据",
            Self::Unavailable => "本次联网搜索未成功",
            Self::Cancelled => "搜索已取消",
        })
    }
}
impl std::error::Error for SearchError {}

pub struct SearchService {
    config: SearchConfig,
    slots: Semaphore,
    provider: OnceCell<Arc<dyn provider::SearchProvider>>,
}
impl SearchService {
    #[cfg(test)]
    pub(crate) fn with_provider(
        config: SearchConfig,
        provider: Arc<dyn provider::SearchProvider>,
    ) -> Self {
        let service = Self::new(config);
        assert!(service.provider.set(provider).is_ok());
        service
    }
    /// Catalog construction requires neither credentials nor a network client.
    pub fn new(config: SearchConfig) -> Self {
        Self {
            slots: Semaphore::new(config.max_concurrent_requests.min(16)),
            config,
            provider: OnceCell::new(),
        }
    }
    pub async fn search(
        &self,
        request: SearchRequest,
        operation: &str,
        cancel: &CancellationToken,
    ) -> Result<SearchResponse> {
        let request = request.normalize(&self.config)?;
        ensure!(self.config.enabled, "联网搜索未启用");
        if self.config.api_key.trim().is_empty() {
            return Err(SearchError::Unauthorized.into());
        }
        let _permit = self.slots.try_acquire().map_err(|_| SearchError::Busy)?;
        let work = async {
            let provider = self
                .provider
                .get_or_try_init(|| async {
                    Ok::<Arc<dyn provider::SearchProvider>, anyhow::Error>(Arc::new(
                        tavily::TavilyProvider::new(self.config.clone())?,
                    ))
                })
                .await?;
            let mut response = provider.search(&request, cancel).await?;
            for (index, source) in response.results.iter_mut().enumerate() {
                source.source_id = format!(
                    "s{}_{}",
                    operation.chars().take(12).collect::<String>(),
                    index + 1
                );
            }
            response.bound(self.config.max_output_bytes)?;
            Ok(response)
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(SearchError::Cancelled.into()),
            result = tokio::time::timeout(Duration::from_secs(self.config.request_timeout_secs), work) => result.map_err(|_| SearchError::Timeout)?,
        }
    }
}

pub(crate) fn shorten(text: &str, chars: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(chars)
        .collect()
}
impl SearchResponse {
    pub fn bound(&mut self, limit: usize) -> Result<()> {
        // Trim fields, then remove last results. JSON remains valid and UTF-8 intact.
        for result in &mut self.results {
            let title = shorten(&result.title, 160);
            let snippet = shorten(&result.snippet, 650);
            self.truncated |= title != result.title || snippet != result.snippet;
            result.title = title;
            result.snippet = snippet;
        }
        while serde_json::to_vec(self)?.len() > limit {
            self.truncated = true;
            if let Some(result) = self
                .results
                .iter_mut()
                .rev()
                .find(|r| r.snippet.chars().count() > 80)
            {
                result.snippet = shorten(&result.snippet, result.snippet.chars().count() / 2);
            } else if self.results.pop().is_none() {
                return Err(SearchError::TooLarge.into());
            }
        }
        Ok(())
    }
    pub fn receipt(&self) -> String {
        if self.results.is_empty() {
            return "本次联网搜索未找到可用结果，无法据此核实。".into();
        }
        let mut text = format!("搜索：{}\n检索时间：{}\n", self.query, self.retrieved_at);
        for source in &self.results {
            text.push_str(&format!(
                "\n[{}] {}\n{}\n{}\n",
                source.source_id, source.title, source.snippet, source.url
            ));
        }
        if self.truncated {
            text.push_str("\n结果已按长度限制裁剪。");
        }
        text
    }
}
