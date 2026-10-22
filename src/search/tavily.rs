use super::{
    SearchConfig, SearchError, SearchRequest, SearchResponse, SearchResult, TimeRange,
    provider::SearchProvider, shorten,
};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

const ENDPOINT: &str = "https://api.tavily.com/search";
pub struct TavilyProvider {
    config: SearchConfig,
    client: reqwest::Client,
    endpoint: String,
}
impl TavilyProvider {
    pub fn new(config: SearchConfig) -> Result<Self> {
        config.validate()?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(std::time::Duration::from_secs(config.request_timeout_secs))
            .timeout(std::time::Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(|_| SearchError::Unavailable)?;
        Ok(Self {
            config,
            client,
            endpoint: ENDPOINT.into(),
        })
    }
    #[cfg(test)]
    pub(crate) fn test_endpoint(mut self, endpoint: String) -> Self {
        self.endpoint = endpoint;
        self
    }
}
#[derive(Deserialize)]
struct Response {
    results: Vec<Item>,
}
#[derive(Deserialize)]
struct Item {
    title: String,
    url: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    published_date: Option<String>,
}
#[async_trait]
impl SearchProvider for TavilyProvider {
    async fn search(
        &self,
        request: &SearchRequest,
        cancel: &CancellationToken,
    ) -> Result<SearchResponse> {
        let work = async {
            let mut body = json!({"query":request.query,"search_depth":"basic","topic":"general",
                "max_results":request.max_results,"include_answer":false,"include_raw_content":false,
                "include_images":false,"auto_parameters":false});
            if request.time_range != TimeRange::Any {
                body["time_range"] = serde_json::to_value(request.time_range)?;
            }
            let mut response = self
                .client
                .post(&self.endpoint)
                .bearer_auth(&self.config.api_key)
                .json(&body)
                .send()
                .await
                .map_err(transport_error)?;
            match response.status().as_u16() {
                200 => (),
                401 | 403 => return Err(SearchError::Unauthorized.into()),
                429 | 432 | 433 => return Err(SearchError::RateLimited.into()),
                _ => return Err(SearchError::Unavailable.into()),
            }
            if response
                .content_length()
                .is_some_and(|n| n > self.config.max_response_bytes as u64)
            {
                return Err(SearchError::TooLarge.into());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
                if bytes.len() + chunk.len() > self.config.max_response_bytes {
                    return Err(SearchError::TooLarge.into());
                }
                bytes.extend_from_slice(&chunk);
            }
            let parsed: Response =
                serde_json::from_slice(&bytes).map_err(|_| SearchError::InvalidResponse)?;
            let mut truncated = parsed.results.len() > request.max_results;
            let mut results = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            for item in parsed.results {
                let Ok(url) = reqwest::Url::parse(&item.url) else {
                    truncated = true;
                    continue;
                };
                if !["https", "http"].contains(&url.scheme())
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || item.url.len() > 1024
                    || item.url.chars().any(char::is_control)
                {
                    truncated = true;
                    continue;
                }
                if !seen.insert(url.to_string()) {
                    continue;
                }
                results.push(SearchResult {
                    source_id: String::new(),
                    title: item.title,
                    url: url.to_string(),
                    snippet: item.content,
                    published_at: item
                        .published_date
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| shorten(&s, 80)),
                });
                if results.len() == request.max_results {
                    break;
                }
            }
            Ok(SearchResponse {
                query: request.query.clone(),
                provider: "tavily".into(),
                retrieved_at: chrono::Utc::now().to_rfc3339(),
                time_range: request.time_range,
                results,
                truncated,
            })
        };
        tokio::select! { biased; _ = cancel.cancelled() => Err(SearchError::Cancelled.into()), result = work => result }
    }
}
fn transport_error(error: reqwest::Error) -> SearchError {
    if error.is_timeout() {
        SearchError::Timeout
    } else {
        SearchError::Unavailable
    }
}
