//! Administrator-initiated synthetic search. Draft secrets never persist.
use super::settings::{ConfigStore, SecretChange};
use crate::search::{SearchRequest, SearchService, TimeRange};
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchTestRequest {
    pub revision: String,
    #[serde(default)]
    pub api_key: SecretChange,
}
pub struct SearchTestService {
    store: Arc<ConfigStore>,
    last: Mutex<Option<Instant>>,
}
impl SearchTestService {
    pub fn new(store: Arc<ConfigStore>) -> Self {
        Self {
            store,
            last: Mutex::new(None),
        }
    }
    pub async fn test(&self, request: SearchTestRequest) -> Result<Value> {
        let mut last = self
            .last
            .try_lock()
            .map_err(|_| anyhow::anyhow!("搜索连接测试正在进行"))?;
        ensure!(
            last.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)),
            "请至少间隔5秒再测试搜索连接"
        );
        let snapshot = self.store.reload().await?;
        let (config, revision) = self.store.effective_with_revision().await?;
        ensure!(
            revision == request.revision && snapshot["revision"] == revision,
            "配置已变化，请重新载入后测试"
        );
        let environment = snapshot["secrets"]["search_api_key"]["env_set"] == true;
        let mut config = config.plugins.web_search;
        let source = match request.api_key {
            SecretChange::Keep => {
                if environment {
                    "environment"
                } else {
                    "file"
                }
            }
            SecretChange::Set { value } => {
                config.api_key = value;
                "temporary"
            }
            SecretChange::Clear => {
                config.api_key.clear();
                "none"
            }
        };
        ensure!(!config.api_key.trim().is_empty(), "请先填写搜索密钥");
        config.enabled = true;
        config.request_timeout_secs = config.request_timeout_secs.min(8);
        config.validate()?;
        *last = Some(Instant::now());
        let service = SearchService::new(config);
        let result = service
            .search(
                SearchRequest {
                    query: "Rust programming language".into(),
                    time_range: TimeRange::Any,
                    max_results: 1,
                },
                &uuid::Uuid::new_v4().simple().to_string(),
                &CancellationToken::new(),
            )
            .await?;
        ensure!(
            self.store.reload().await?["revision"] == revision,
            "配置已变化，请重新测试"
        );
        Ok(
            json!({"provider":result.provider,"result_count":result.results.len(),"key_source":source,
            "environment_overrides_saved_key":environment && source == "temporary"}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn search_test_rejects_stale_missing_credentials_and_duplicate_requests_without_saving() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = crate::config::Config::default();
        config.plugins.ai.enabled = false;
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        let original = std::fs::read(&path).unwrap();
        let store = Arc::new(ConfigStore::open(&path).await.unwrap());
        let (_, revision) = store.effective_with_revision().await.unwrap();
        let service = SearchTestService::new(store);
        assert!(
            service
                .test(SearchTestRequest {
                    revision: "stale".into(),
                    api_key: SecretChange::Set {
                        value: "synthetic".into()
                    }
                })
                .await
                .unwrap_err()
                .to_string()
                .contains("配置已变化")
        );
        assert!(
            service
                .test(SearchTestRequest {
                    revision: revision.clone(),
                    api_key: SecretChange::Clear
                })
                .await
                .unwrap_err()
                .to_string()
                .contains("密钥")
        );
        let guard = service.last.lock().await;
        assert!(
            service
                .test(SearchTestRequest {
                    revision,
                    api_key: SecretChange::Clear
                })
                .await
                .unwrap_err()
                .to_string()
                .contains("正在进行")
        );
        drop(guard);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
}
