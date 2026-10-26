//! On-demand model discovery. Preview credentials are never saved or redirected.
use std::{collections::BTreeSet, fmt, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Semaphore;

use super::settings::{ConfigStore, SecretChange};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_MODELS: usize = 4096;
const MAX_ID_BYTES: usize = 256;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelsError {
    Invalid,
    KeyRequired,
    Stale,
    Busy,
    Unavailable,
}
impl ModelsError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Invalid => "models_invalid",
            Self::KeyRequired => "models_key_required",
            Self::Stale => "models_stale",
            Self::Busy => "models_busy",
            Self::Unavailable => "models_unavailable",
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
impl fmt::Display for ModelsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Invalid => "模型列表请求无效，请检查服务地址和密钥格式",
            Self::KeyRequired => {
                "服务地址已改变，请输入新服务的密钥；已保存的密钥不会发送到其他地址"
            }
            Self::Stale => "配置已变化，请重新载入后再获取模型列表",
            Self::Busy => "已有模型列表请求正在处理，请稍后重试",
            Self::Unavailable => {
                "无法获取模型列表，请检查服务地址、密钥、网络及服务是否支持模型列表接口"
            }
        })
    }
}
impl std::error::Error for ModelsError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsRequest {
    pub revision: String,
    pub url: String,
    #[serde(default)]
    pub api_key: SecretChange,
}
#[derive(Debug, Serialize)]
pub struct ModelsView {
    pub models: Vec<String>,
    pub chat_completions_url: String,
    pub responses_url: String,
    pub key_source: &'static str,
    pub environment_overrides_saved_key: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ModelEndpoints {
    pub models: reqwest::Url,
    pub chat_completions: reqwest::Url,
    pub responses: reqwest::Url,
}

/// Accept a service base or either complete model endpoint, preserving
/// custom path prefixes. Query parameters and URL credentials are not accepted.
pub fn endpoints(input: &str) -> Result<ModelEndpoints, ModelsError> {
    if input.len() > 4096 || input.chars().any(char::is_control) {
        return Err(ModelsError::Invalid);
    }
    let mut base = reqwest::Url::parse(input.trim()).map_err(|_| ModelsError::Invalid)?;
    if !matches!(base.scheme(), "http" | "https")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(ModelsError::Invalid);
    }
    let path = base.path().trim_end_matches('/');
    let prefix = path
        .strip_suffix("/chat/completions")
        .or_else(|| path.strip_suffix("/responses"))
        .unwrap_or(path);
    let model_path = format!("{prefix}/models");
    let chat_path = format!("{prefix}/chat/completions");
    let responses_path = format!("{prefix}/responses");
    base.set_path(&model_path);
    let models = base.clone();
    base.set_path(&chat_path);
    let chat_completions = base.clone();
    base.set_path(&responses_path);
    Ok(ModelEndpoints {
        models,
        chat_completions,
        responses: base,
    })
}

fn credential(
    saved: &crate::llm::LlmConfig,
    target: &ModelEndpoints,
    change: SecretChange,
    environment_set: bool,
) -> Result<(String, &'static str), ModelsError> {
    let (key, source) = match change {
        SecretChange::Keep => {
            if !saved.api_key.is_empty()
                && endpoints(saved.endpoint_url())
                    .map(|saved| saved.models != target.models)
                    .unwrap_or(true)
            {
                return Err(ModelsError::KeyRequired);
            }
            let source = if saved.api_key.is_empty() {
                "none"
            } else if environment_set {
                "environment"
            } else {
                "file"
            };
            (saved.api_key.clone(), source)
        }
        SecretChange::Set { value } => {
            if value.is_empty() {
                return Err(ModelsError::Invalid);
            }
            (value, "temporary")
        }
        SecretChange::Clear => (String::new(), "none"),
    };
    if key.len() > 8192 || reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")).is_err()
    {
        return Err(ModelsError::Invalid);
    }
    Ok((key, source))
}

fn parse_models(value: Value) -> Result<Vec<String>, ModelsError> {
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err(ModelsError::Unavailable);
    }
    let rows = value["data"].as_array().ok_or(ModelsError::Unavailable)?;
    if rows.len() > MAX_MODELS {
        return Err(ModelsError::Unavailable);
    }
    let mut models = BTreeSet::new();
    for row in rows {
        let id = row["id"].as_str().ok_or(ModelsError::Unavailable)?;
        if id.is_empty()
            || id.len() > MAX_ID_BYTES
            || id.trim() != id
            || id.chars().any(char::is_control)
        {
            return Err(ModelsError::Unavailable);
        }
        models.insert(id.to_owned());
    }
    Ok(models.into_iter().collect())
}

pub struct ModelsService {
    store: Arc<ConfigStore>,
    requests: Semaphore,
}
impl ModelsService {
    pub fn new(store: Arc<ConfigStore>) -> Self {
        Self {
            store,
            requests: Semaphore::new(2),
        }
    }

    pub async fn list(&self, request: ModelsRequest) -> Result<ModelsView, ModelsError> {
        let _permit = self.requests.try_acquire().map_err(|_| ModelsError::Busy)?;
        if request.revision.len() != 64 {
            return Err(ModelsError::Stale);
        }
        let target = endpoints(&request.url)?;
        // This is an explicit administrator action, not a polling path. Reload
        // before checking revision so an external file edit cannot be missed.
        self.store.reload().await.map_err(|_| ModelsError::Stale)?;
        let (saved, revision, environment_set) = self
            .store
            .llm_config_with_revision()
            .await
            .map_err(|_| ModelsError::Unavailable)?;
        if request.revision != revision {
            return Err(ModelsError::Stale);
        }
        let (key, source) = credential(&saved, &target, request.api_key, environment_set)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| ModelsError::Unavailable)?;
        let mut get = http.get(target.models);
        if !key.is_empty() {
            get = get.bearer_auth(&key);
        }
        let operation = async {
            let response = get.send().await.map_err(|_| ModelsError::Unavailable)?;
            if !response.status().is_success() {
                return Err(ModelsError::Unavailable);
            }
            let bytes = crate::milky::read_bounded(response, MAX_RESPONSE_BYTES)
                .await
                .map_err(|_| ModelsError::Unavailable)?;
            let value = serde_json::from_slice(&bytes).map_err(|_| ModelsError::Unavailable)?;
            parse_models(value)
        };
        let models = tokio::time::timeout(REQUEST_TIMEOUT, operation)
            .await
            .map_err(|_| ModelsError::Unavailable)??;
        let current = self.store.reload().await.map_err(|_| ModelsError::Stale)?;
        if current["revision"].as_str() != Some(revision.as_str()) {
            return Err(ModelsError::Stale);
        }
        Ok(ModelsView {
            models,
            chat_completions_url: target.chat_completions.to_string(),
            responses_url: target.responses.to_string(),
            key_source: source,
            environment_overrides_saved_key: environment_set,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{HeaderMap, StatusCode, Uri},
        response::Response,
        routing::get,
    };
    use serde_json::json;
    use tokio::{sync::Mutex, task::JoinHandle};

    #[test]
    fn endpoint_paths_preserve_prefix_and_canonicalize_complete_chat_urls() {
        for (url, expected) in [
            ("https://example.com", "https://example.com"),
            ("https://example.com/v1/", "https://example.com/v1"),
            (
                "https://example.com/proxy/custom/chat/completions/",
                "https://example.com/proxy/custom",
            ),
            ("http://localhost:9000/api", "http://localhost:9000/api"),
            (
                "https://example.com/proxy/v1/responses/",
                "https://example.com/proxy/v1",
            ),
        ] {
            let paths = endpoints(url).unwrap();
            assert_eq!(paths.models.as_str(), format!("{expected}/models"));
            assert_eq!(
                paths.chat_completions.as_str(),
                format!("{expected}/chat/completions")
            );
            assert_eq!(endpoints(paths.chat_completions.as_str()).unwrap(), paths);
            assert_eq!(paths.responses.as_str(), format!("{expected}/responses"));
            assert_eq!(endpoints(paths.responses.as_str()).unwrap(), paths);
        }
        for url in [
            "file:///tmp/provider",
            "https://name:password@example.com/v1",
            "https://example.com/v1?key=synthetic",
            "https://example.com/v1#fragment",
            "https://exam\nple.com/v1",
            "not a URL",
        ] {
            assert_eq!(endpoints(url).unwrap_err(), ModelsError::Invalid);
        }
    }

    #[test]
    fn responses_saved_key_uses_the_selected_endpoint_only() {
        let saved = crate::llm::LlmConfig {
            protocol: crate::llm::ApiProtocol::Responses,
            responses_url: "https://new.example/proxy/v1/responses".into(),
            chat_completions_url: "https://old.example/v1/chat/completions".into(),
            api_key: "synthetic".into(),
            ..Default::default()
        };
        assert_eq!(
            credential(
                &saved,
                &endpoints("https://new.example/proxy/v1").unwrap(),
                SecretChange::Keep,
                false
            )
            .unwrap()
            .0,
            "synthetic"
        );
        assert_eq!(
            credential(
                &saved,
                &endpoints(&saved.chat_completions_url).unwrap(),
                SecretChange::Keep,
                false
            )
            .unwrap_err(),
            ModelsError::KeyRequired
        );
    }

    #[test]
    fn saved_credentials_are_bound_to_exact_model_list_endpoint() {
        let saved = crate::llm::LlmConfig {
            chat_completions_url: "https://provider.example/v1/chat/completions".into(),
            api_key: "synthetic-effective-key".into(),
            ..Default::default()
        };
        let same = endpoints("https://provider.example/v1/").unwrap();
        let (key, source) = credential(&saved, &same, SecretChange::Keep, true).unwrap();
        assert_eq!(key, "synthetic-effective-key");
        assert_eq!(source, "environment");
        assert_eq!(
            credential(&saved, &same, SecretChange::Keep, false)
                .unwrap()
                .1,
            "file"
        );
        for changed in [
            "https://another.example/v1",
            "http://provider.example/v1",
            "https://provider.example:8443/v1",
            "https://provider.example/other-proxy/v1",
        ] {
            let changed = endpoints(changed).unwrap();
            assert_eq!(
                credential(&saved, &changed, SecretChange::Keep, true).unwrap_err(),
                ModelsError::KeyRequired
            );
            assert_eq!(
                credential(
                    &saved,
                    &changed,
                    SecretChange::Set {
                        value: "synthetic-temporary-key".into()
                    },
                    true,
                )
                .unwrap(),
                ("synthetic-temporary-key".into(), "temporary")
            );
            assert_eq!(
                credential(&saved, &changed, SecretChange::Clear, true).unwrap(),
                (String::new(), "none")
            );
        }
        // An empty environment override is an empty effective credential;
        // neither Keep nor Clear can fall back to an old file credential.
        let empty = crate::llm::LlmConfig {
            api_key: String::new(),
            ..saved
        };
        assert_eq!(
            credential(
                &empty,
                &endpoints("https://another.example/v1").unwrap(),
                SecretChange::Keep,
                true,
            )
            .unwrap(),
            (String::new(), "none")
        );
    }

    #[test]
    fn malformed_keys_and_response_ids_are_rejected_without_reflection() {
        let target = endpoints("https://provider.example/v1").unwrap();
        for value in [
            String::new(),
            "key\r\nInjected: yes".into(),
            "x".repeat(8193),
        ] {
            let error = credential(
                &crate::llm::LlmConfig::default(),
                &target,
                SecretChange::Set { value },
                false,
            )
            .unwrap_err();
            assert_eq!(error, ModelsError::Invalid);
            assert!(!error.to_string().contains("Injected"));
        }
        assert_eq!(
            parse_models(json!({"data":[{"id":"z"},{"id":"a"},{"id":"z"}]})).unwrap(),
            ["a", "z"]
        );
        assert!(parse_models(json!({"data":[]})).unwrap().is_empty());
        for value in [
            json!({"data":{},"error":null}),
            json!({"data":[],"error":{"message":"sensitive upstream text"}}),
            json!({"data":[{"id":1}]}),
            json!({"data":[{"id":""}]}),
            json!({"data":[{"id":" leading"}]}),
            json!({"data":[{"id":"line\nbreak"}]}),
            json!({"data":[{"id":"x".repeat(MAX_ID_BYTES+1)}]}),
            json!({"data":vec![json!({"id":"a"});MAX_MODELS+1]}),
        ] {
            assert_eq!(parse_models(value).unwrap_err(), ModelsError::Unavailable);
        }
        let request = ModelsRequest {
            revision: "0".repeat(64),
            url: "https://provider.example/v1".into(),
            api_key: SecretChange::Set {
                value: "synthetic-preview-secret".into(),
            },
        };
        assert!(!format!("{request:?}").contains("synthetic-preview-secret"));
    }

    struct MockState {
        calls: Mutex<Vec<(String, Option<String>)>>,
        status: StatusCode,
        body: String,
        location: Option<String>,
        gate: Option<Semaphore>,
    }
    struct Mock {
        url: String,
        state: Arc<MockState>,
        task: JoinHandle<()>,
    }
    impl Drop for Mock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl Mock {
        async fn new(
            status: StatusCode,
            body: String,
            location: Option<String>,
            block: bool,
        ) -> Self {
            let state = Arc::new(MockState {
                calls: Mutex::new(Vec::new()),
                status,
                body,
                location,
                gate: block.then(|| Semaphore::new(0)),
            });
            let router = Router::new()
                .route("/{*path}", get(mock_handler))
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            Self { url, state, task }
        }
        async fn wait_calls(&self, count: usize) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while self.state.calls.lock().await.len() < count {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("mock request not received");
        }
    }
    async fn mock_handler(
        State(state): State<Arc<MockState>>,
        uri: Uri,
        headers: HeaderMap,
    ) -> Response {
        state.calls.lock().await.push((
            uri.path().into(),
            headers
                .get("authorization")
                .map(|value| value.to_str().unwrap().to_owned()),
        ));
        if let Some(gate) = &state.gate {
            let Ok(Ok(permit)) = tokio::time::timeout(Duration::from_secs(5), gate.acquire()).await
            else {
                return Response::builder()
                    .status(StatusCode::GATEWAY_TIMEOUT)
                    .body(Body::empty())
                    .unwrap();
            };
            permit.forget();
        }
        let mut response = Response::builder().status(state.status);
        if let Some(location) = &state.location {
            response = response.header("location", location);
        }
        response.body(Body::from(state.body.clone())).unwrap()
    }

    async fn fixture(url: &str) -> (tempfile::TempDir, Arc<ConfigStore>, ModelsRequest) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = crate::config::Config::default();
        config.llm.chat_completions_url = format!("{url}/custom/v1/chat/completions");
        config.llm.api_key = "synthetic-file-key".into();
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        let store = Arc::new(ConfigStore::open(&path).await.unwrap());
        let (_, revision) = store.effective_with_revision().await.unwrap();
        let request = ModelsRequest {
            revision,
            url: format!("{url}/custom/v1"),
            // Network tests always use a synthetic explicit credential, never
            // any environment credential present on the developer's computer.
            api_key: SecretChange::Set {
                value: "synthetic-preview-key".into(),
            },
        };
        (directory, store, request)
    }

    #[tokio::test]
    async fn preview_fetches_only_models_and_never_persists_the_temporary_key() {
        let mock = Mock::new(
            StatusCode::OK,
            json!({"data":[{"id":"z"},{"id":"a"},{"id":"a"}]}).to_string(),
            None,
            false,
        )
        .await;
        let (_directory, store, request) = fixture(&mock.url).await;
        let before = std::fs::read(store.path()).unwrap();
        let response = ModelsService::new(store.clone())
            .list(request)
            .await
            .unwrap();
        assert_eq!(response.models, ["a", "z"]);
        assert_eq!(response.key_source, "temporary");
        assert_eq!(
            response.chat_completions_url,
            format!("{}/custom/v1/chat/completions", mock.url)
        );
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        assert_eq!(
            *mock.state.calls.lock().await,
            vec![(
                "/custom/v1/models".into(),
                Some("Bearer synthetic-preview-key".into())
            )]
        );
        let serialized = serde_json::to_string(&response).unwrap();
        assert!(!serialized.contains("synthetic-preview-key"));
        assert!(!serialized.contains("synthetic-file-key"));
    }

    #[tokio::test]
    async fn clear_is_anonymous_and_empty_model_list_is_successful() {
        let mock = Mock::new(StatusCode::OK, "{\"data\":[]}".into(), None, false).await;
        let (_directory, store, mut request) = fixture(&mock.url).await;
        request.api_key = SecretChange::Clear;
        let view = ModelsService::new(store).list(request).await.unwrap();
        assert!(view.models.is_empty());
        assert_eq!(view.key_source, "none");
        assert!(mock.state.calls.lock().await[0].1.is_none());
    }

    #[tokio::test]
    async fn stale_revision_never_contacts_the_provider() {
        let mock = Mock::new(StatusCode::OK, "{\"data\":[]}".into(), None, false).await;
        let (_directory, store, request) = fixture(&mock.url).await;
        let mut changed = crate::config::Config::load_raw(store.path()).unwrap();
        changed.app.timezone = "UTC".into();
        std::fs::write(store.path(), toml::to_string(&changed).unwrap()).unwrap();
        assert_eq!(
            ModelsService::new(store).list(request).await.unwrap_err(),
            ModelsError::Stale
        );
        assert!(mock.state.calls.lock().await.is_empty());
    }

    #[tokio::test]
    async fn redirects_are_not_followed_and_upstream_errors_are_not_reflected() {
        let target = Mock::new(StatusCode::OK, "{\"data\":[]}".into(), None, false).await;
        let redirect = Mock::new(
            StatusCode::TEMPORARY_REDIRECT,
            "private-upstream-detail".into(),
            Some(format!("{}/capture", target.url)),
            false,
        )
        .await;
        let (_directory, store, request) = fixture(&redirect.url).await;
        let error = ModelsService::new(store).list(request).await.unwrap_err();
        assert_eq!(error, ModelsError::Unavailable);
        assert!(!error.to_string().contains("private-upstream-detail"));
        assert!(!error.to_string().contains(&target.url));
        assert!(target.state.calls.lock().await.is_empty());
    }

    #[tokio::test]
    async fn response_size_and_provider_error_envelopes_are_bounded() {
        for body in [
            "x".repeat(MAX_RESPONSE_BYTES + 1),
            json!({"error":{"message":"synthetic-private-error"},"data":[]}).to_string(),
        ] {
            let mock = Mock::new(StatusCode::OK, body, None, false).await;
            let (_directory, store, request) = fixture(&mock.url).await;
            assert_eq!(
                ModelsService::new(store).list(request).await.unwrap_err(),
                ModelsError::Unavailable
            );
        }
    }

    #[tokio::test]
    async fn concurrent_requests_are_bounded_and_late_configuration_changes_are_rejected() {
        let mock = Mock::new(StatusCode::OK, "{\"data\":[]}".into(), None, true).await;
        let (_directory, store, request) = fixture(&mock.url).await;
        let service = Arc::new(ModelsService::new(store.clone()));
        let revision = request.revision.clone();
        let make_request = || ModelsRequest {
            revision: revision.clone(),
            url: format!("{}/custom/v1", mock.url),
            api_key: SecretChange::Clear,
        };
        let first_service = service.clone();
        let first = tokio::spawn(async move { first_service.list(request).await });
        let second_service = service.clone();
        let second_request = make_request();
        let second = tokio::spawn(async move { second_service.list(second_request).await });
        mock.wait_calls(2).await;
        assert_eq!(
            service.list(make_request()).await.unwrap_err(),
            ModelsError::Busy
        );
        let mut changed = crate::config::Config::load_raw(store.path()).unwrap();
        changed.app.timezone = "UTC".into();
        std::fs::write(store.path(), toml::to_string(&changed).unwrap()).unwrap();
        mock.state.gate.as_ref().unwrap().add_permits(2);
        for task in [first, second] {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err(),
                ModelsError::Stale
            );
        }
        assert_eq!(mock.state.calls.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn cancelled_preview_releases_capacity_for_a_following_request() {
        let mock = Mock::new(StatusCode::OK, "{\"data\":[]}".into(), None, true).await;
        let (_directory, store, request) = fixture(&mock.url).await;
        let next = ModelsRequest {
            revision: request.revision.clone(),
            url: request.url.clone(),
            api_key: SecretChange::Clear,
        };
        let service = Arc::new(ModelsService::new(store));
        let first_service = service.clone();
        let first = tokio::spawn(async move { first_service.list(request).await });
        mock.wait_calls(1).await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(service.requests.available_permits(), 2);
        mock.state.gate.as_ref().unwrap().add_permits(2);
        let result = tokio::time::timeout(Duration::from_secs(5), service.list(next))
            .await
            .unwrap()
            .unwrap();
        assert!(result.models.is_empty());
        assert_eq!(mock.state.calls.lock().await.len(), 2);
        assert_eq!(service.requests.available_permits(), 2);
    }
}
