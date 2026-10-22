use super::{SearchRequest, SearchResponse};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait SearchProvider: Send + Sync {
    async fn search(
        &self,
        request: &SearchRequest,
        cancel: &CancellationToken,
    ) -> anyhow::Result<SearchResponse>;
}
