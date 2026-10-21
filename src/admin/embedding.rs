//! Explicit synthetic connectivity tests. Preview credentials never persist.
use super::settings::{ConfigStore, SecretChange};
use crate::embedding::{Embedding, EmbeddingConfig, EmbeddingError};
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingTestError {
    Invalid,
    KeyRequired,
    Stale,
    Busy,
    Unavailable,
}

impl EmbeddingTestError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Invalid => "embedding_test_invalid",
            Self::KeyRequired => "embedding_test_key_required",
            Self::Stale => "embedding_test_stale",
            Self::Busy => "embedding_test_busy",
            Self::Unavailable => "embedding_test_unavailable",
        }
    }

    pub fn status(self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            Self::Invalid | Self::KeyRequired => StatusCode::BAD_REQUEST,
            Self::Stale => StatusCode::CONFLICT,
            Self::Busy => StatusCode::TOO_MANY_REQUESTS,
            Self::Unavailable => StatusCode::BAD_GATEWAY,
        }
    }
}

impl fmt::Display for EmbeddingTestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Invalid => "向量连接测试参数无效，请检查地址、密钥、模型、维度和超时",
            Self::KeyRequired => {
                "向量服务地址已改变，请填写该地址的新密钥；原密钥不会发送到其他地址"
            }
            Self::Stale => "配置已变化，请重新载入后再测试向量连接",
            Self::Busy => "已有向量连接测试正在进行，请稍后重试",
            Self::Unavailable => "向量连接测试未通过，请检查服务地址、密钥、模型能力、维度和网络",
        })
    }
}

impl std::error::Error for EmbeddingTestError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingTestRequest {
    pub revision: String,
    pub url: String,
    pub model: String,
    pub dimensions: usize,
    pub request_timeout_secs: u64,
    #[serde(default)]
    pub api_key: SecretChange,
}

#[derive(Debug, Serialize)]
pub struct EmbeddingTestView {
    pub dimensions: usize,
    pub text_ok: bool,
    pub image_ok: bool,
    pub key_source: &'static str,
    pub environment_overrides_saved_key: bool,
}

fn credential(
    saved: &EmbeddingConfig,
    target: &str,
    change: SecretChange,
    environment_set: bool,
) -> Result<(String, &'static str), EmbeddingTestError> {
    let (key, source) = match change {
        SecretChange::Keep => {
            if !saved.api_key.is_empty()
                && reqwest::Url::parse(&saved.embeddings_url).ok()
                    != reqwest::Url::parse(target).ok()
            {
                return Err(EmbeddingTestError::KeyRequired);
            }
            (
                saved.api_key.clone(),
                if saved.api_key.is_empty() {
                    "none"
                } else if environment_set {
                    "environment"
                } else {
                    "file"
                },
            )
        }
        SecretChange::Set { value } => (value, "temporary"),
        SecretChange::Clear => (String::new(), "none"),
    };
    if key.trim().is_empty()
        || key.len() > 8192
        || reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")).is_err()
    {
        return Err(EmbeddingTestError::Invalid);
    }
    Ok((key, source))
}

pub struct EmbeddingTestService {
    store: Arc<ConfigStore>,
    requests: Semaphore,
}

impl EmbeddingTestService {
    pub fn new(store: Arc<ConfigStore>) -> Self {
        Self {
            store,
            requests: Semaphore::new(1),
        }
    }

    async fn current(&self, revision: &str) -> anyhow::Result<()> {
        let current = self.store.reload().await?;
        anyhow::ensure!(
            current["revision"].as_str() == Some(revision),
            "embedding preview changed"
        );
        Ok(())
    }

    pub async fn test(
        &self,
        request: EmbeddingTestRequest,
    ) -> Result<EmbeddingTestView, EmbeddingTestError> {
        let _permit = self
            .requests
            .try_acquire()
            .map_err(|_| EmbeddingTestError::Busy)?;
        if request.revision.len() != 64 {
            return Err(EmbeddingTestError::Stale);
        }
        let operation = async {
            let snapshot = self
                .store
                .reload()
                .await
                .map_err(|_| EmbeddingTestError::Stale)?;
            let (saved, revision) = self
                .store
                .effective_with_revision()
                .await
                .map_err(|_| EmbeddingTestError::Unavailable)?;
            if request.revision != revision
                || snapshot["revision"].as_str() != Some(revision.as_str())
            {
                return Err(EmbeddingTestError::Stale);
            }
            let environment_set = snapshot["secrets"]["embedding_api_key"]["env_set"] == true;
            let mut config = EmbeddingConfig {
                enabled: true,
                embeddings_url: request.url,
                model: request.model,
                dimensions: request.dimensions,
                request_timeout_secs: request.request_timeout_secs,
                // The preview response is deliberately bounded independently
                // of any draft/body-limit setting and never returns vectors.
                max_response_bytes: 256 * 1024,
                api_key: String::new(),
            };
            config.validate().map_err(|_| EmbeddingTestError::Invalid)?;
            let (key, key_source) = credential(
                &saved.embedding,
                &config.embeddings_url,
                request.api_key,
                environment_set,
            )?;
            config.api_key = key;
            // Two tests must fit inside the administration request's 30s limit.
            config.request_timeout_secs = config.request_timeout_secs.min(12);
            let client = Embedding::new(config).map_err(|_| EmbeddingTestError::Invalid)?;
            let cancel = CancellationToken::new();
            let map_error = |error: anyhow::Error| {
                if error.downcast_ref::<EmbeddingError>() == Some(&EmbeddingError::SourceChanged) {
                    EmbeddingTestError::Stale
                } else {
                    EmbeddingTestError::Unavailable
                }
            };
            client
                .embed_text(
                    "A synthetic connection test.",
                    &cancel,
                    self.current(&revision),
                )
                .await
                .map_err(map_error)?;
            let mut jpeg = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(2, 2)
                .write_to(&mut jpeg, image::ImageFormat::Jpeg)
                .map_err(|_| EmbeddingTestError::Unavailable)?;
            client
                .embed_jpeg(jpeg.get_ref(), &cancel, self.current(&revision))
                .await
                .map_err(map_error)?;
            self.current(&revision)
                .await
                .map_err(|_| EmbeddingTestError::Stale)?;
            Ok(EmbeddingTestView {
                dimensions: client.dimensions(),
                text_ok: true,
                image_ok: true,
                key_source,
                environment_overrides_saved_key: environment_set,
            })
        };
        tokio::time::timeout(Duration::from_secs(25), operation)
            .await
            .map_err(|_| EmbeddingTestError::Unavailable)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
    use serde_json::{Value, json};
    use tokio::sync::Mutex;

    type Calls = Arc<Mutex<Vec<(String, Value)>>>;

    async fn fixture() -> (
        tempfile::TempDir,
        Arc<ConfigStore>,
        EmbeddingTestRequest,
        Calls,
        tokio::task::JoinHandle<()>,
    ) {
        let calls: Calls = Arc::default();
        let router =
            Router::new()
                .route(
                    "/embeddings",
                    post(
                        |State(calls): State<Calls>,
                         headers: HeaderMap,
                         Json(body): Json<Value>| async move {
                            calls
                                .lock()
                                .await
                                .push((headers["authorization"].to_str().unwrap().into(), body));
                            Json(
                                json!({"object":"list","model":EmbeddingConfig::default().model,
                    "data":[{"object":"embedding","index":0,"embedding":vec![1.0;64]}],
                    "usage":{"prompt_tokens":4,"total_tokens":4}}),
                            )
                        },
                    ),
                )
                .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/embeddings", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = crate::config::Config::default();
        config.plugins.ai.enabled = false;
        config.nonebot.enabled = false;
        config.embedding.embeddings_url = endpoint.clone();
        config.embedding.api_key = "synthetic-saved-key".into();
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        let store = Arc::new(ConfigStore::open(path).await.unwrap());
        let (_, revision) = store.effective_with_revision().await.unwrap();
        let request = EmbeddingTestRequest {
            revision,
            url: endpoint,
            model: EmbeddingConfig::default().model,
            dimensions: 64,
            request_timeout_secs: 15,
            // Never use inherited environment credentials in network tests.
            api_key: SecretChange::Set {
                value: "synthetic-preview-key".into(),
            },
        };
        (directory, store, request, calls, server)
    }

    #[test]
    fn saved_key_is_bound_to_exact_endpoint_and_preview_keys_remain_explicit() {
        let saved = EmbeddingConfig {
            embeddings_url: "https://first.invalid/v1/embeddings".into(),
            api_key: "synthetic-saved-key".into(),
            ..Default::default()
        };
        assert_eq!(
            credential(&saved, &saved.embeddings_url, SecretChange::Keep, true)
                .unwrap()
                .1,
            "environment"
        );
        for target in [
            "https://other.invalid/v1/embeddings",
            "https://first.invalid/other",
            "https://first.invalid:8443/v1/embeddings",
        ] {
            assert_eq!(
                credential(&saved, target, SecretChange::Keep, false).unwrap_err(),
                EmbeddingTestError::KeyRequired
            );
            assert_eq!(
                credential(
                    &saved,
                    target,
                    SecretChange::Set {
                        value: "synthetic-new-key".into()
                    },
                    false
                )
                .unwrap()
                .1,
                "temporary"
            );
        }
        for value in ["", " ", "bad\nheader"] {
            assert_eq!(
                credential(
                    &saved,
                    &saved.embeddings_url,
                    SecretChange::Set {
                        value: value.into()
                    },
                    false
                )
                .unwrap_err(),
                EmbeddingTestError::Invalid
            );
        }
    }

    #[tokio::test]
    async fn preview_uses_two_synthetic_inputs_and_never_saves_credentials_or_returns_vectors() {
        let (_directory, store, request, calls, server) = fixture().await;
        let before = std::fs::read(store.path()).unwrap();
        let view = EmbeddingTestService::new(store.clone())
            .test(request)
            .await
            .unwrap();
        assert_eq!(view.dimensions, 64);
        assert!(view.text_ok && view.image_ok);
        assert_eq!(view.key_source, "temporary");
        let serialized = serde_json::to_string(&view).unwrap();
        assert!(!serialized.contains("synthetic-preview-key"));
        assert!(!serialized.contains("synthetic-saved-key"));
        assert!(!serialized.contains("vector"));
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        let requests = calls.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|(key, _)| key == "Bearer synthetic-preview-key")
        );
        assert_eq!(
            requests[0].1["input"]["text"],
            "A synthetic connection test."
        );
        assert!(
            requests[1].1["input"]["image"]
                .as_str()
                .unwrap()
                .starts_with("data:image/jpeg;base64,")
        );
        server.abort();
    }

    #[tokio::test]
    async fn stale_revision_or_another_preview_prevents_outbound_requests() {
        let (_directory, store, mut request, calls, server) = fixture().await;
        let service = EmbeddingTestService::new(store.clone());
        let permit = service.requests.acquire().await.unwrap();
        assert_eq!(
            service
                .test(EmbeddingTestRequest {
                    revision: request.revision.clone(),
                    url: request.url.clone(),
                    model: request.model.clone(),
                    dimensions: 64,
                    request_timeout_secs: 15,
                    api_key: SecretChange::Set {
                        value: "synthetic-preview-key".into()
                    },
                })
                .await
                .unwrap_err(),
            EmbeddingTestError::Busy
        );
        drop(permit);
        request.revision = "0".repeat(64);
        assert_eq!(
            service.test(request).await.unwrap_err(),
            EmbeddingTestError::Stale
        );
        assert!(calls.lock().await.is_empty());
        server.abort();
    }
}
