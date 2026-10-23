//! Chat Completions compatible transport. Tool execution is exclusively the host's job.
use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::milky::read_bounded;

pub(crate) const MAX_INPUT_IMAGES: usize = 5;
pub(crate) const MAX_IMAGE_URL_BYTES: usize = 1_400_000;

type SourceValidation<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    pub chat_completions_url: String,
    pub api_key: String,
    pub model: String,
    pub tool_calling: bool,
    pub vision: bool,
    pub max_concurrent_requests: usize,
    pub request_timeout_secs: u64,
    pub max_response_bytes: usize,
}

impl std::fmt::Debug for LlmConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LlmConfig")
            .field("chat_completions_url", &self.chat_completions_url)
            .field("api_key", &"[redacted]")
            .field("model", &self.model)
            .field("tool_calling", &self.tool_calling)
            .field("vision", &self.vision)
            .field("max_concurrent_requests", &self.max_concurrent_requests)
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            chat_completions_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            tool_calling: true,
            vision: true,
            max_concurrent_requests: 2,
            request_timeout_secs: 60,
            max_response_bytes: 256 * 1024,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub message: Value,
    pub usage: Usage,
    pub model: String,
}

/// A provider refusal is a service result, never an assistant answer or tool call.
/// Keep this error independent of the provider body so chat/logs cannot expose it.
#[derive(Debug)]
pub(crate) struct ProviderBlocked;

impl std::fmt::Display for ProviderBlocked {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .write_str("上游模型拦截了本次请求，可能涉及当前消息或会话上下文，未返回可用回复。")
    }
}

impl std::error::Error for ProviderBlocked {}

#[derive(Debug)]
pub(crate) struct TruncatedToolResponse;

impl std::fmt::Display for TruncatedToolResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("模型输出达到长度上限，工具调用未完整生成，本次调用未执行。")
    }
}

impl std::error::Error for TruncatedToolResponse {}

/// Only validated protocol names are retained; provider text, arguments and
/// call IDs must never enter the diagnostic error or its source chain.
#[derive(Debug)]
pub(crate) struct UnofferedTool {
    requested: String,
    allowed_names: Vec<String>,
    call_count: usize,
}

impl UnofferedTool {
    /// A narrow recovery predicate; never expose the rejected call or its arguments.
    pub(crate) fn is_only_call_to(&self, name: &str) -> bool {
        self.call_count == 1 && self.requested == name
    }
}

impl std::fmt::Display for UnofferedTool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "模型请求了本轮未开放的工具（{}），未执行。",
            self.requested
        )
    }
}

impl std::error::Error for UnofferedTool {}

const MAX_TOOL_CALL_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvalidCallIdReason {
    Empty,
    TooLong,
    Control,
    Missing,
    NotString,
}

impl InvalidCallIdReason {
    fn label(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too_long",
            Self::Control => "control",
            Self::Missing => "missing",
            Self::NotString => "not_string",
        }
    }
}

#[derive(Debug)]
struct InvalidToolCallId {
    reason: InvalidCallIdReason,
    id_bytes: Option<usize>,
}

impl std::fmt::Display for InvalidToolCallId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("模型返回的工具调用标识无效，本批工具调用未执行。")
    }
}

impl std::error::Error for InvalidToolCallId {}

fn validate_tool_call_id(id: &str) -> Result<()> {
    let reason = if id.is_empty() {
        InvalidCallIdReason::Empty
    } else if id.len() > MAX_TOOL_CALL_ID_BYTES {
        InvalidCallIdReason::TooLong
    } else if id.chars().any(char::is_control) {
        InvalidCallIdReason::Control
    } else {
        return Ok(());
    };
    Err(InvalidToolCallId {
        reason,
        id_bytes: Some(id.len()),
    }
    .into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportKind {
    Timeout,
    Connect,
    Request,
    Body,
    Builder,
    Decode,
    Redirect,
    Other,
}

impl TransportKind {
    fn classify(error: &reqwest::Error) -> Self {
        // A connect timeout is both kinds: its actionable classification is timeout.
        if error.is_timeout() {
            Self::Timeout
        } else if error.is_connect() {
            Self::Connect
        } else if error.is_request() {
            Self::Request
        } else if error.is_body() {
            Self::Body
        } else if error.is_builder() {
            Self::Builder
        } else if error.is_decode() {
            Self::Decode
        } else if error.is_redirect() {
            Self::Redirect
        } else {
            Self::Other
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::Request => "request",
            Self::Body => "body",
            Self::Builder => "builder",
            Self::Decode => "decode",
            Self::Redirect => "redirect",
            Self::Other => "other",
        }
    }
}

#[derive(Debug)]
pub(crate) struct ModelHttpFailure(TransportKind);

impl std::fmt::Display for ModelHttpFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self.0 {
            TransportKind::Timeout => "模型请求超时，当前请求尚未取得模型响应。",
            TransportKind::Connect => "无法连接模型服务，当前请求尚未取得模型响应。",
            _ => "模型请求传输失败，当前请求尚未取得模型响应。",
        })
    }
}

impl std::error::Error for ModelHttpFailure {}

fn transport_io_kind(error: &reqwest::Error) -> Option<&'static str> {
    let mut source = std::error::Error::source(error);
    for _ in 0..8 {
        let current = source?;
        if let Some(error) = current.downcast_ref::<std::io::Error>() {
            return Some(match error.kind() {
                std::io::ErrorKind::ConnectionReset => "connection_reset",
                std::io::ErrorKind::ConnectionAborted => "connection_aborted",
                std::io::ErrorKind::ConnectionRefused => "connection_refused",
                std::io::ErrorKind::BrokenPipe => "broken_pipe",
                std::io::ErrorKind::UnexpectedEof => "unexpected_eof",
                std::io::ErrorKind::TimedOut => "timed_out",
                _ => "other",
            });
        }
        source = current.source();
    }
    None
}

// Read only integer counters. No provider text, tool names or arguments are
// retained here, including when those fields are malformed or adversarial.
#[derive(Debug, PartialEq, Eq)]
struct TruncationMetadata {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    tool_call_count: usize,
}

impl TruncationMetadata {
    fn from_response(value: &Value) -> Self {
        Self {
            prompt_tokens: value["usage"]["prompt_tokens"].as_u64(),
            completion_tokens: value["usage"]["completion_tokens"].as_u64(),
            total_tokens: value["usage"]["total_tokens"].as_u64(),
            reasoning_tokens: value["usage"]["completion_tokens_details"]["reasoning_tokens"]
                .as_u64(),
            tool_call_count: value["choices"][0]["message"]["tool_calls"]
                .as_array()
                .map_or(0, Vec::len),
        }
    }
}

#[derive(Clone)]
pub struct Llm {
    config: LlmConfig,
    http: reqwest::Client,
    endpoint: reqwest::Url,
    requests: Arc<Semaphore>,
    background: Arc<Semaphore>,
    budget: Option<crate::budget::Budget>,
}

impl Llm {
    pub fn new(config: LlmConfig, http: reqwest::Client) -> Result<Self> {
        let endpoint =
            reqwest::Url::parse(&config.chat_completions_url).context("invalid model endpoint")?;
        ensure!(
            matches!(endpoint.scheme(), "http" | "https"),
            "model endpoint must use http or https"
        );
        ensure!(
            endpoint.username().is_empty()
                && endpoint.password().is_none()
                && endpoint.query().is_none()
                && endpoint.fragment().is_none(),
            "model endpoint must not contain credentials, query or fragment"
        );
        ensure!(!config.model.trim().is_empty(), "model must be configured");
        ensure!(
            (1..=16).contains(&config.max_concurrent_requests),
            "model concurrency must be 1..16"
        );
        ensure!(
            (1..=120).contains(&config.request_timeout_secs),
            "model timeout must be 1..120 seconds"
        );
        ensure!(
            (1024..=4 * 1024 * 1024).contains(&config.max_response_bytes),
            "model body limit must be 1 KiB..4 MiB"
        );
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", config.api_key))
            .context("invalid model API key")?;
        let requests = Arc::new(Semaphore::new(config.max_concurrent_requests));
        Ok(Self {
            config,
            http,
            endpoint,
            requests,
            background: Arc::new(Semaphore::new(1)),
            budget: None,
        })
    }

    pub fn has_capacity(&self) -> bool {
        self.requests.available_permits() > 0
    }

    pub fn with_budget(mut self, budget: crate::budget::Budget) -> Self {
        self.budget = Some(budget);
        self
    }

    pub async fn complete_admitted(
        &self,
        messages: Vec<Value>,
        tools: Vec<Value>,
        max_output: usize,
        cancel: &CancellationToken,
        admission: crate::budget::Admission,
    ) -> Result<LlmResponse> {
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("model request cancelled"),
            p = self.requests.clone().acquire_owned() => p.context("model request scheduler closed")?,
        };
        self.request(
            messages,
            tools,
            max_output,
            cancel,
            permit,
            Some(crate::budget::RequestAdmission::Chat(admission)),
            None,
            None,
        )
        .await
    }

    /// Revalidate versioned text sources after waiting for model capacity and
    /// immediately before HTTP, just as visual sources are revalidated.
    #[allow(clippy::too_many_arguments)]
    pub async fn complete_checked_admitted(
        &self,
        messages: Vec<Value>,
        tools: Vec<Value>,
        max_output: usize,
        cancel: &CancellationToken,
        admission: crate::budget::Admission,
        revalidate_sources: impl std::future::Future<Output = Result<()>> + Send,
    ) -> Result<LlmResponse> {
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("model request cancelled"),
            p = self.requests.clone().acquire_owned() => p.context("model request scheduler closed")?,
        };
        self.request(
            messages,
            tools,
            max_output,
            cancel,
            permit,
            Some(crate::budget::RequestAdmission::Chat(admission)),
            None,
            Some(Box::pin(revalidate_sources)),
        )
        .await
    }

    /// Visual inputs are tied to the QQ login that supplied their pixels,
    /// including while waiting for an available model request slot. Revalidate
    /// the source messages after that wait, before submitting any pixels.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn complete_visual_admitted(
        &self,
        messages: Vec<Value>,
        tools: Vec<Value>,
        max_output: usize,
        cancel: &CancellationToken,
        admission: crate::budget::Admission,
        account: (&crate::milky::Milky, u64),
        revalidate_sources: impl std::future::Future<Output = Result<()>> + Send,
    ) -> Result<LlmResponse> {
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("model request cancelled"),
            p = self.requests.clone().acquire_owned() => p.context("model request scheduler closed")?,
        };
        self.request(
            messages,
            tools,
            max_output,
            cancel,
            permit,
            Some(crate::budget::RequestAdmission::Chat(admission)),
            Some(account),
            Some(Box::pin(revalidate_sources)),
        )
        .await
    }

    pub async fn complete(
        &self,
        messages: Vec<Value>,
        tools: Vec<Value>,
        max_output: usize,
        cancel: &CancellationToken,
    ) -> Result<LlmResponse> {
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("model request cancelled"),
            permit = self.requests.clone().acquire_owned() => permit.context("model request scheduler closed")?,
        };
        self.request(
            messages, tools, max_output, cancel, permit, None, None, None,
        )
        .await
    }

    /// Background jobs never queue ahead of foreground requests and cannot use tools.
    pub async fn complete_background(
        &self,
        messages: Vec<Value>,
        max_output: usize,
        cancel: &CancellationToken,
    ) -> Result<Option<LlmResponse>> {
        self.background_request(messages, max_output, cancel, None)
            .await
    }

    pub async fn complete_background_admitted(
        &self,
        messages: Vec<Value>,
        max_output: usize,
        cancel: &CancellationToken,
        guard: crate::types::ContextGuard,
        plugin_epoch: i64,
    ) -> Result<Option<LlmResponse>> {
        self.background_request(
            messages,
            max_output,
            cancel,
            Some(crate::budget::RequestAdmission::Memory {
                guard,
                plugin_epoch,
            }),
        )
        .await
    }

    /// Revalidate frozen knowledge after admission and immediately before the
    /// background model request, using the existing memory quota and capacity.
    pub async fn complete_background_checked_admitted(
        &self,
        messages: Vec<Value>,
        max_output: usize,
        cancel: &CancellationToken,
        guard: crate::types::ContextGuard,
        plugin_epoch: i64,
        revalidate_sources: impl std::future::Future<Output = Result<()>> + Send,
    ) -> Result<Option<LlmResponse>> {
        let Ok(_background) = self.background.clone().try_acquire_owned() else {
            return Ok(None);
        };
        let Ok(permit) = self.requests.clone().try_acquire_owned() else {
            return Ok(None);
        };
        self.request(
            messages,
            Vec::new(),
            max_output,
            cancel,
            permit,
            Some(crate::budget::RequestAdmission::Memory {
                guard,
                plugin_epoch,
            }),
            None,
            Some(Box::pin(revalidate_sources)),
        )
        .await
        .map(Some)
    }

    /// Learning has its own persisted quota but shares background capacity so
    /// it never queues ahead of normal chats or invokes tools.
    pub async fn complete_learning_admitted(
        &self,
        messages: Vec<Value>,
        max_output: usize,
        cancel: &CancellationToken,
        guard: crate::types::ContextGuard,
        plugin_epoch: i64,
    ) -> Result<Option<LlmResponse>> {
        self.background_request(
            messages,
            max_output,
            cancel,
            Some(crate::budget::RequestAdmission::Learning {
                guard,
                plugin_epoch,
            }),
        )
        .await
    }

    /// Learning sources may change while their persisted quota is reserved.
    /// Validate them and the optional QQ session again before sending the body.
    #[allow(clippy::too_many_arguments)]
    pub async fn complete_learning_checked_admitted(
        &self,
        messages: Vec<Value>,
        max_output: usize,
        cancel: &CancellationToken,
        guard: crate::types::ContextGuard,
        plugin_epoch: i64,
        account: Option<(&crate::milky::Milky, u64)>,
        revalidate_sources: impl std::future::Future<Output = Result<()>> + Send,
    ) -> Result<Option<LlmResponse>> {
        let Ok(_background) = self.background.clone().try_acquire_owned() else {
            return Ok(None);
        };
        let Ok(permit) = self.requests.clone().try_acquire_owned() else {
            return Ok(None);
        };
        self.request(
            messages,
            Vec::new(),
            max_output,
            cancel,
            permit,
            Some(crate::budget::RequestAdmission::Learning {
                guard,
                plugin_epoch,
            }),
            account,
            Some(Box::pin(revalidate_sources)),
        )
        .await
        .map(Some)
    }

    async fn background_request(
        &self,
        messages: Vec<Value>,
        max_output: usize,
        cancel: &CancellationToken,
        admission: Option<crate::budget::RequestAdmission>,
    ) -> Result<Option<LlmResponse>> {
        let Ok(_background) = self.background.clone().try_acquire_owned() else {
            return Ok(None);
        };
        let Ok(permit) = self.requests.clone().try_acquire_owned() else {
            return Ok(None);
        };
        self.request(
            messages,
            Vec::new(),
            max_output,
            cancel,
            permit,
            admission,
            None,
            None,
        )
        .await
        .map(Some)
    }

    #[allow(clippy::too_many_arguments)]
    async fn request(
        &self,
        messages: Vec<Value>,
        tools: Vec<Value>,
        max_output: usize,
        cancel: &CancellationToken,
        _permit: OwnedSemaphorePermit,
        admission: Option<crate::budget::RequestAdmission>,
        account: Option<(&crate::milky::Milky, u64)>,
        revalidate_sources: Option<SourceValidation<'_>>,
    ) -> Result<LlmResponse> {
        ensure!(
            (1..=32768).contains(&max_output),
            "output token budget must be 1..32768"
        );
        let image_count = validate_messages(&messages)?;
        ensure!(
            self.config.vision || image_count == 0,
            "vision input disabled for this model"
        );
        ensure!(tools.len() <= 16, "too many tool definitions");
        ensure!(
            self.config.tool_calling || tools.is_empty(),
            "tool calling disabled for this model"
        );
        let allowed_tools = tools
            .iter()
            .map(|tool| {
                ensure!(
                    tool["type"] == "function",
                    "unsupported tool definition type"
                );
                let name = tool["function"]["name"]
                    .as_str()
                    .context("tool definition missing name")?;
                ensure!(valid_name(name), "invalid tool name");
                Ok(name.to_owned())
            })
            .collect::<Result<HashSet<_>>>()?;
        ensure!(
            allowed_tools.len() == tools.len(),
            "duplicate model tool names"
        );
        let tool_count = tools.len();
        let has_tool_history = messages.iter().any(|message| message["role"] == "tool");
        let mut body = json!({"model": self.config.model, "messages": messages,
            "max_tokens": max_output, "stream": false});
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
            body["parallel_tool_calls"] = json!(false);
        } else if self.config.tool_calling && has_tool_history {
            // Earlier tool calls remain in protocol history during the final
            // answer. They do not authorize another call after tools are closed.
            body["tool_choice"] = json!("none");
        }
        let operation = async {
            if let Some((milky, generation)) = account {
                milky.check_generation(generation)?;
            }
            if let Some(budget) = &self.budget {
                ensure!(
                    admission.is_some(),
                    "model request requires a runtime admission"
                );
                budget.reserve_request(admission).await?;
            }
            if let Some(revalidate) = revalidate_sources {
                revalidate.await?;
            }
            ensure!(!cancel.is_cancelled(), "model request cancelled");
            if let Some((milky, generation)) = account {
                milky.check_generation(generation)?;
            }
            let mut request = self
                .http
                .post(self.endpoint.clone())
                .timeout(Duration::from_secs(self.config.request_timeout_secs))
                .json(&body);
            if !self.config.api_key.is_empty() {
                request = request.bearer_auth(&self.config.api_key);
            }
            if let Some((milky, generation)) = account {
                milky.check_generation(generation)?;
            }
            let started = std::time::Instant::now();
            let mut request_bytes = None;
            let response = match request.build() {
                Ok(request) => {
                    // .json() already serialized this body; count the existing
                    // bytes without logging, cloning or serializing it again.
                    request_bytes = request
                        .body()
                        .and_then(reqwest::Body::as_bytes)
                        .map(<[u8]>::len);
                    self.http.execute(request).await
                }
                Err(error) => Err(error),
            }
            .map_err(|error| {
                let kind = TransportKind::classify(&error);
                tracing::warn!(
                    phase = "headers",
                    kind = kind.label(),
                    io_kind = transport_io_kind(&error),
                    elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    timeout_secs = self.config.request_timeout_secs,
                    image_count,
                    max_output,
                    tool_count,
                    request_bytes,
                    "model HTTP request failed"
                );
                // Do not retain reqwest's source chain: it can include the URL.
                ModelHttpFailure(kind)
            })?;
            ensure!(
                response.status().is_success(),
                "model HTTP status {}",
                response.status().as_u16()
            );
            let bytes = read_bounded(response, self.config.max_response_bytes).await?;
            if let Some((milky, generation)) = account {
                milky.check_generation(generation)?;
            }
            let value: Value =
                serde_json::from_slice(&bytes).context("model response is not valid JSON")?;
            let metadata = TruncationMetadata::from_response(&value);
            let result = parse_response(value, &allowed_tools);
            if result
                .as_ref()
                .is_err_and(|error| error.is::<TruncatedToolResponse>())
            {
                tracing::warn!(
                    max_output,
                    finish_reason = "length",
                    prompt_tokens = metadata.prompt_tokens,
                    completion_tokens = metadata.completion_tokens,
                    total_tokens = metadata.total_tokens,
                    reasoning_tokens = metadata.reasoning_tokens,
                    tool_call_count = metadata.tool_call_count,
                    "model tool response reached output limit"
                );
            }
            if let Some(error) = result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<InvalidToolCallId>())
            {
                tracing::warn!(
                    reason = error.reason.label(),
                    id_bytes = error.id_bytes,
                    max_id_bytes = MAX_TOOL_CALL_ID_BYTES,
                    "model tool response had invalid call id"
                );
            }
            if let Some(error) = result
                .as_ref()
                .err()
                .and_then(|error| error.downcast_ref::<UnofferedTool>())
            {
                tracing::warn!(
                    requested = error.requested.as_str(),
                    allowed_names = ?error.allowed_names,
                    "model requested a tool not offered in this request"
                );
            }
            result
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("model request cancelled"),
            response = operation => response,
        }
    }
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn tool_calls(message: &Value) -> Result<Vec<(String, String)>> {
    let Some(calls) = message.get("tool_calls").filter(|value| !value.is_null()) else {
        return Ok(Vec::new());
    };
    let calls = calls.as_array().context("tool_calls must be an array")?;
    // The dispatcher rejects multi-call batches without executing them, then
    // reports a result for every ID. Preserve bounded batches for that protocol.
    ensure!(
        calls.len() <= 32,
        "at most 32 tool calls may be parsed per response"
    );
    let mut ids = HashSet::new();
    calls
        .iter()
        .map(|call| {
            ensure!(call["type"] == "function", "unsupported tool call type");
            let value = call.get("id").ok_or(InvalidToolCallId {
                reason: InvalidCallIdReason::Missing,
                id_bytes: None,
            })?;
            let id = value.as_str().ok_or(InvalidToolCallId {
                reason: InvalidCallIdReason::NotString,
                id_bytes: None,
            })?;
            validate_tool_call_id(id)?;
            ensure!(ids.insert(id), "duplicate tool call id in response");
            let name = call["function"]["name"]
                .as_str()
                .context("tool call missing function name")?;
            ensure!(valid_name(name), "invalid tool call name");
            let arguments = call["function"]["arguments"]
                .as_str()
                .context("tool arguments must be a JSON string")?;
            ensure!(arguments.len() <= 8192, "tool arguments exceed limit");
            let parsed: Value =
                serde_json::from_str(arguments).context("tool arguments are not valid JSON")?;
            ensure!(parsed.is_object(), "tool arguments must be an object");
            Ok((id.to_owned(), name.to_owned()))
        })
        .collect()
}

/// Only host-normalized JPEG data is accepted. The transport never asks the
/// model provider to fetch a remote URL or interpret another media format.
fn validate_user_parts(parts: &[Value]) -> Result<usize> {
    ensure!(!parts.is_empty(), "user content parts must not be empty");
    let mut images = 0;
    for part in parts {
        let part = part
            .as_object()
            .context("user content part must be an object")?;
        ensure!(
            part.len() == 2,
            "user content part has missing or unknown fields"
        );
        match part.get("type").and_then(Value::as_str) {
            Some("text") => ensure!(
                part.get("text").is_some_and(Value::is_string),
                "text content part must contain a string"
            ),
            Some("image_url") => {
                images += 1;
                ensure!(
                    images <= MAX_INPUT_IMAGES,
                    "too many input images in one message"
                );
                let image = part
                    .get("image_url")
                    .and_then(Value::as_object)
                    .context("image_url content part must contain an object")?;
                ensure!(
                    image.len() == 2 && image.get("detail").and_then(Value::as_str) == Some("auto"),
                    "image_url must contain only url and detail=auto"
                );
                let url = image
                    .get("url")
                    .and_then(Value::as_str)
                    .context("image_url url must be a string")?;
                ensure!(
                    url.len() <= MAX_IMAGE_URL_BYTES,
                    "image data URL exceeds limit"
                );
                let encoded = url
                    .strip_prefix("data:image/jpeg;base64,")
                    .context("image URL must be inline JPEG base64 data")?;
                ensure!(!encoded.is_empty(), "image base64 data must not be empty");
                let bytes = STANDARD
                    .decode(encoded)
                    .context("image base64 data is invalid")?;
                ensure!(
                    bytes.len() >= 5
                        && bytes.starts_with(&[0xff, 0xd8, 0xff])
                        && bytes.ends_with(&[0xff, 0xd9]),
                    "image data is not a complete JPEG"
                );
            }
            _ => anyhow::bail!("unsupported user content part type"),
        }
    }
    Ok(images)
}

fn validate_messages(messages: &[Value]) -> Result<usize> {
    ensure!(
        !messages.is_empty() && messages.len() <= 8192,
        "model messages must contain 1..8192 entries"
    );
    let mut pending = HashSet::new();
    let mut used_ids = HashSet::new();
    let mut images = 0;
    for message in messages {
        let role = message["role"].as_str().context("message missing role")?;
        if role == "tool" {
            let id = message["tool_call_id"]
                .as_str()
                .context("tool result missing tool_call_id")?;
            ensure!(pending.remove(id), "orphan or duplicate tool result");
            ensure!(
                message["content"].is_string(),
                "tool result content must be text"
            );
            continue;
        }
        ensure!(
            pending.is_empty(),
            "tool call/result pairs must remain adjacent and complete"
        );
        ensure!(
            matches!(role, "system" | "user" | "assistant"),
            "unsupported model message role"
        );
        if role == "user"
            && let Some(parts) = message["content"].as_array()
        {
            images += validate_user_parts(parts)?;
            ensure!(
                images <= MAX_INPUT_IMAGES,
                "too many input images in one request"
            );
        } else {
            ensure!(
                message["content"].is_string()
                    || (role == "assistant" && message["content"].is_null()),
                "message content must be text"
            );
        }
        if role == "assistant" {
            for (id, _) in tool_calls(message)? {
                ensure!(
                    used_ids.insert(id.clone()),
                    "duplicate tool call id in history"
                );
                pending.insert(id);
            }
        } else {
            ensure!(
                message.get("tool_calls").is_none(),
                "only assistant messages may call tools"
            );
        }
    }
    ensure!(pending.is_empty(), "missing tool result");
    Ok(images)
}

// This exact service notice has been observed inside a successful compatibility
// response. Match the complete notice, including both official links and sentence
// boundaries; quoting or explaining it is ordinary model text, not this fallback.
const GOOGLE_PROMPT_BLOCKED_NOTICE: &str = concat!(
    "The prompt could not be submitted. The prompt contains sensitive words that violate Google's ",
    "[Generative AI Prohibited Use policy](https://policies.google.com/terms/generative-ai/use-policy). ",
    "Try rephrasing the prompt. If you think this was an error, ",
    "[send feedback](https://ai.google.dev/gemini-api/docs/troubleshooting)."
);

fn blocked_finish_reason(reason: &Value) -> bool {
    reason.as_str().is_some_and(|reason| {
        [
            "content_filter",
            "content_blocked",
            "safety",
            "blocklist",
            "prohibited_content",
            "spii",
            "recitation",
            "image_safety",
            "image_prohibited_content",
            "image_recitation",
            "escalation",
            "pup_limited_disabled",
        ]
        .iter()
        .any(|blocked| reason.eq_ignore_ascii_case(blocked))
    })
}

fn safety_blocked(value: &Value) -> bool {
    ["safetyRatings", "safety_ratings"].iter().any(|field| {
        value[*field]
            .as_array()
            .is_some_and(|ratings| ratings.iter().any(|rating| rating["blocked"] == true))
    })
}

fn prompt_blocked(value: &Value) -> bool {
    ["promptFeedback", "prompt_feedback"].iter().any(|field| {
        let feedback = &value[*field];
        ["blockReason", "block_reason"].iter().any(|reason| {
            feedback[*reason].as_str().is_some_and(|reason| {
                !reason.trim().is_empty()
                    && !reason.eq_ignore_ascii_case("BLOCK_REASON_UNSPECIFIED")
            })
        }) || safety_blocked(feedback)
    })
}

fn provider_blocked(value: &Value) -> bool {
    if prompt_blocked(value)
        || safety_blocked(value)
        || blocked_finish_reason(&value["error"]["code"])
    {
        return true;
    }
    let choices = value["choices"].as_array().into_iter().flatten();
    let candidates = value["candidates"].as_array().into_iter().flatten();
    choices.chain(candidates).any(|choice| {
        let message = &choice["message"];
        blocked_finish_reason(&choice["finish_reason"])
            || blocked_finish_reason(&choice["finishReason"])
            || prompt_blocked(choice)
            || safety_blocked(choice)
            || message["refusal"]
                .as_str()
                .is_some_and(|refusal| !refusal.trim().is_empty())
            || message["content"]
                .as_str()
                .is_some_and(|text| text.trim() == GOOGLE_PROMPT_BLOCKED_NOTICE)
    })
}

fn parse_response(value: Value, allowed_tools: &HashSet<String>) -> Result<LlmResponse> {
    // Refuse the entire response before exposing text or even validating tool
    // calls. A provider block must not reach the dispatcher or assistant history.
    if provider_blocked(&value) {
        return Err(ProviderBlocked.into());
    }
    ensure!(
        value.get("error").is_none_or(Value::is_null),
        "model provider returned an error"
    );
    let choices = value["choices"]
        .as_array()
        .context("model response missing choices")?;
    ensure!(choices.len() == 1, "expected exactly one model choice");
    let message = &choices[0]["message"];
    ensure!(
        message["role"] == "assistant",
        "model response role must be assistant"
    );
    ensure!(
        message["content"].is_string() || message["content"].is_null(),
        "model content must be text or null"
    );
    ensure!(
        message.get("function_call").is_none_or(Value::is_null),
        "legacy function_call is not supported"
    );
    // Even apparently complete JSON cannot authorize an operation when the
    // provider says generation stopped at its output limit. Classify this before
    // parsing arguments so a cut-off JSON string receives the same safe error.
    if choices[0]["finish_reason"] == "length"
        && message["tool_calls"]
            .as_array()
            .is_some_and(|calls| !calls.is_empty())
    {
        return Err(TruncatedToolResponse.into());
    }
    let calls = tool_calls(message)?;
    for (_, name) in &calls {
        if !allowed_tools.contains(name) {
            let mut allowed_names: Vec<_> = allowed_tools.iter().cloned().collect();
            allowed_names.sort_unstable();
            return Err(UnofferedTool {
                requested: name.clone(),
                allowed_names,
                call_count: calls.len(),
            }
            .into());
        }
    }
    ensure!(
        !calls.is_empty()
            || message["content"]
                .as_str()
                .is_some_and(|text| !text.trim().is_empty()),
        "model response contains neither text nor a tool call"
    );
    // Keep only native protocol fields; unknown provider metadata cannot become instructions.
    let mut normalized = json!({"role":"assistant", "content":message["content"]});
    if !calls.is_empty() {
        normalized["tool_calls"] = message["tool_calls"].clone();
    }
    Ok(LlmResponse {
        message: normalized,
        usage: Usage {
            input_tokens: value["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: value["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        },
        model: value["model"].as_str().unwrap_or("unknown").to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::milky::tests::server;

    #[derive(Clone, Default)]
    struct DiagnosticLog(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for DiagnosticLog {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn diagnostic_log() -> (DiagnosticLog, impl tracing::Subscriber + Send + Sync) {
        let log = DiagnosticLog::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        (log, subscriber)
    }

    impl DiagnosticLog {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    async fn read_synthetic_request(stream: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            assert!(bytes.len() <= 128 * 1024);
            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse().ok())
                    })
                    .unwrap();
                if bytes.len() >= end + 4 + length {
                    return;
                }
            }
        }
    }

    fn response(message: Value) -> Value {
        json!({"model":"test", "choices":[{"message":message,"finish_reason":"stop"}], "usage":{"prompt_tokens":10,"completion_tokens":2}})
    }
    fn call(id: &str) -> Value {
        json!({"id":id,"type":"function","function":{"name":"clock_now","arguments":"{}"}})
    }
    fn config(url: String) -> LlmConfig {
        LlmConfig {
            chat_completions_url: url,
            model: "test".into(),
            api_key: "llm-only".into(),
            ..Default::default()
        }
    }

    fn image_part() -> Value {
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(1, 1, image::Rgb([255, 0, 0])))
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        json!({"type":"image_url","image_url":{
            "url":format!("data:image/jpeg;base64,{}",STANDARD.encode(jpeg.into_inner())),
            "detail":"auto"
        }})
    }

    #[tokio::test]
    async fn visual_sources_are_revalidated_after_waiting_for_a_model_slot() {
        use crate::types::{ContextGuard, Scene, Scope};
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut cfg = config(format!("http://{}", listener.local_addr().unwrap()));
        cfg.max_concurrent_requests = 1;
        let client = Llm::new(cfg, reqwest::Client::new()).unwrap();
        let (login_url, login_server) = crate::milky::tests::server(
            "200 OK",
            r#"{"status":"ok","retcode":0,"data":{"uin":123}}"#,
        )
        .await;
        let milky = crate::milky::Milky::new(
            crate::milky::MilkyConfig {
                url: login_url,
                bot_id: Some(123),
                ..Default::default()
            },
            reqwest::Client::new(),
        )
        .unwrap();
        milky.login().await.unwrap();
        login_server.await.unwrap();
        let held = client.requests.clone().acquire_owned().await.unwrap();
        let revoked = AtomicBool::new(false);
        let checked = AtomicBool::new(false);
        let cancel = CancellationToken::new();
        let request = client.complete_visual_admitted(
            vec![json!({"role":"user","content":[{"type":"text","text":"describe quote"},image_part()]})],
            vec![], 128, &cancel,
            crate::budget::Admission {
                scope: Scope { bot_id: 123, scene: Scene::Group, peer_id: 456 },
                actor: 789,
                guard: ContextGuard { conversation_id: "visual-source-test".into(), epoch: 1 },
                plugin_epoch: 1,
            },
            (&milky, milky.scope_generation(&Scope { bot_id: 123, scene: Scene::Group, peer_id: 456 }).unwrap()),
            async {
                checked.store(true, Ordering::SeqCst);
                ensure!(!revoked.load(Ordering::SeqCst), "quoted source revoked");
                Ok(())
            },
        );
        tokio::pin!(request);
        tokio::select! {
            biased;
            result = &mut request => panic!("request bypassed held permit: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        assert!(!checked.load(Ordering::SeqCst));
        revoked.store(true, Ordering::SeqCst);
        drop(held);
        let error = request.await.unwrap_err();
        assert_eq!(error.to_string(), "quoted source revoked");
        assert!(checked.load(Ordering::SeqCst));
        assert_eq!(client.requests.available_permits(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn checked_text_sources_are_revalidated_after_waiting_for_a_model_slot() {
        use crate::types::{ContextGuard, Scene, Scope};
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut cfg = config(format!("http://{}", listener.local_addr().unwrap()));
        cfg.max_concurrent_requests = 1;
        let client = Llm::new(cfg, reqwest::Client::new()).unwrap();
        let held = client.requests.clone().acquire_owned().await.unwrap();
        let revoked = AtomicBool::new(false);
        let checked = AtomicBool::new(false);
        let cancel = CancellationToken::new();
        let request = client.complete_checked_admitted(
            vec![
                json!({"role":"system","content":"previous learning note"}),
                json!({"role":"user","content":"hello"}),
            ],
            vec![],
            128,
            &cancel,
            crate::budget::Admission {
                scope: Scope {
                    bot_id: 123,
                    scene: Scene::Group,
                    peer_id: 456,
                },
                actor: 789,
                guard: ContextGuard {
                    conversation_id: "learning-source-test".into(),
                    epoch: 1,
                },
                plugin_epoch: 1,
            },
            async {
                checked.store(true, Ordering::SeqCst);
                ensure!(!revoked.load(Ordering::SeqCst), "learning source revoked");
                Ok(())
            },
        );
        tokio::pin!(request);
        tokio::select! {
            biased;
            result = &mut request => panic!("request bypassed held permit: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        assert!(!checked.load(Ordering::SeqCst));
        revoked.store(true, Ordering::SeqCst);
        drop(held);
        let error = request.await.unwrap_err();
        assert_eq!(error.to_string(), "learning source revoked");
        assert!(checked.load(Ordering::SeqCst));
        assert_eq!(client.requests.available_permits(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
    }

    #[test]
    fn mixed_user_input_accepts_inline_jpeg_without_changing_tool_protocol() {
        let messages = vec![
            json!({"role":"system","content":"Describe the image"}),
            json!({"role":"user","content":[{"type":"text","text":"first"},image_part(),{"type":"text","text":"second"},image_part()]}),
            json!({"role":"assistant","content":null,"tool_calls":[call("one")]}),
            json!({"role":"tool","tool_call_id":"one","content":"time result"}),
            json!({"role":"assistant","content":"answer"}),
        ];
        assert_eq!(validate_messages(&messages).unwrap(), 2);
        assert_eq!(
            validate_messages(&[
                json!({"role":"user","content":[{"type":"text","text":"text only"}]})
            ])
            .unwrap(),
            0
        );
        assert!(
            validate_messages(&[
                messages[2].clone(),
                messages[1].clone(),
                messages[3].clone()
            ])
            .is_err()
        );
    }

    #[test]
    fn input_images_are_bounded_per_message_and_request() {
        let four = vec![image_part(); MAX_INPUT_IMAGES];
        assert_eq!(
            validate_messages(&[json!({"role":"user","content":four})]).unwrap(),
            MAX_INPUT_IMAGES
        );
        let five = vec![image_part(); MAX_INPUT_IMAGES + 1];
        assert!(validate_messages(&[json!({"role":"user","content":five})]).is_err());
        let two = vec![image_part(); 2];
        let first = json!({"role":"user","content":two});
        let mut messages = vec![
            first.clone(),
            first,
            json!({"role":"user","content":[image_part()]}),
        ];
        assert_eq!(validate_messages(&messages).unwrap(), MAX_INPUT_IMAGES);
        messages.push(json!({"role":"user","content":[image_part()]}));
        assert!(validate_messages(&messages).is_err());
        let mut oversized = image_part();
        oversized["image_url"]["url"] = json!("x".repeat(MAX_IMAGE_URL_BYTES + 1));
        assert!(
            validate_messages(&[json!({"role":"user","content":[oversized]})])
                .unwrap_err()
                .to_string()
                .contains("exceeds limit")
        );
    }

    #[test]
    fn image_input_rejects_external_urls_invalid_data_and_non_user_roles() {
        let mut invalid_parts = vec![
            Value::Null,
            json!("plain part"),
            json!({"type":"text"}),
            json!({"type":"text","text":17}),
            json!({"type":"text","text":"hello","extra":true}),
            json!({"type":"audio","audio":"data"}),
            json!({"type":"image_url","image_url":"not an object"}),
            json!({"type":"image_url","image_url":{"url":"data:image/jpeg;base64,"}}),
        ];
        for url in [
            "https://example.invalid/image.jpg",
            "http://127.0.0.1/image.jpg",
            "file:///tmp/image.jpg",
            "data:image/png;base64,iVBORw==",
            "data:image/jpeg;base64,",
            "data:image/jpeg;base64,not base64!",
            "data:image/jpeg;base64,/9j/==",
            "data:image/jpeg;base64,/9j/\n",
            "data:image/jpeg;base64,/9j/",
            "data:image/jpeg;base64,aGVsbG8=",
        ] {
            let mut part = image_part();
            part["image_url"]["url"] = json!(url);
            invalid_parts.push(part);
        }
        for detail in [json!("high"), json!("low"), Value::Null, json!(false)] {
            let mut part = image_part();
            part["image_url"]["detail"] = detail;
            invalid_parts.push(part);
        }
        let mut extra = image_part();
        extra["image_url"]["headers"] = json!({"Authorization":"fake"});
        invalid_parts.push(extra);
        let mut extra = image_part();
        extra["extra"] = json!(true);
        invalid_parts.push(extra);
        for part in invalid_parts {
            assert!(validate_messages(&[json!({"role":"user","content":[part]})]).is_err());
        }
        for content in [json!([]), Value::Null, json!({"text":"hello"}), json!(42)] {
            assert!(validate_messages(&[json!({"role":"user","content":content})]).is_err());
        }
        for role in ["system", "assistant"] {
            for parts in [
                json!([image_part()]),
                json!([{"type":"text","text":"hello"}]),
            ] {
                assert!(validate_messages(&[json!({"role":role,"content":parts})]).is_err());
            }
        }
        let call = json!({"role":"assistant","content":null,"tool_calls":[call("one")]});
        assert!(
            validate_messages(&[
                call,
                json!({"role":"tool","tool_call_id":"one","content":[image_part()]})
            ])
            .is_err()
        );
    }

    #[tokio::test]
    async fn disabled_vision_rejects_images_before_transport() {
        let mut cfg = config("http://127.0.0.1:1".into());
        cfg.vision = false;
        let client = Llm::new(cfg, reqwest::Client::new()).unwrap();
        let error = client
            .complete(
                vec![json!({"role":"user","content":[image_part()]})],
                vec![],
                128,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "vision input disabled for this model");
        assert_eq!(client.requests.available_permits(), 2);
    }

    #[tokio::test]
    async fn disabled_vision_preserves_text_and_native_tools() {
        let body = response(json!({"role":"assistant","content":null,"tool_calls":[call("one")]}))
            .to_string();
        let (url, server) = server("200 OK", &body).await;
        let mut cfg = config(url);
        cfg.vision = false;
        let client = Llm::new(cfg, reqwest::Client::new()).unwrap();
        let result = client.complete(
            vec![json!({"role":"user","content":[{"type":"text","text":"what time?"}]})],
            vec![json!({"type":"function","function":{"name":"clock_now","parameters":{"type":"object"}}})],
            128, &CancellationToken::new(),
        ).await.unwrap();
        assert_eq!(
            result.message["tool_calls"][0]["function"]["name"],
            "clock_now"
        );
        let request = server.await.unwrap();
        assert!(request.contains("what time?"));
        assert!(request.contains("\"parallel_tool_calls\":false"));
        assert!(!request.contains("image_url"));
    }

    #[tokio::test]
    async fn visual_request_waiting_for_a_permit_rejects_reverified_account_before_http() {
        use crate::{
            milky::{Milky, MilkyConfig},
            types::{ContextGuard, Scene, Scope},
        };
        let login_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let login_url = format!("http://{}", login_listener.local_addr().unwrap());
        let login_server = tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/api/get_login_info",
                axum::routing::post(|| async {
                    axum::Json(json!({"status":"ok","retcode":0,"data":{"uin":123}}))
                }),
            );
            axum::serve(login_listener, app).await.unwrap();
        });
        let milky = Milky::new(
            MilkyConfig {
                url: login_url,
                ..Default::default()
            },
            reqwest::Client::new(),
        )
        .unwrap();
        assert_eq!(milky.login().await.unwrap(), 123);
        let scope = Scope {
            bot_id: 123,
            scene: Scene::Group,
            peer_id: 456,
        };
        let generation = milky.scope_generation(&scope).unwrap();
        let model_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut cfg = config(format!("http://{}", model_listener.local_addr().unwrap()));
        cfg.max_concurrent_requests = 1;
        cfg.request_timeout_secs = 1;
        let client = Llm::new(cfg, reqwest::Client::new()).unwrap();
        let held = client.requests.clone().acquire_owned().await.unwrap();
        let cancel = CancellationToken::new();
        let request = client.complete_visual_admitted(
            vec![json!({"role":"user","content":[{"type":"text","text":"describe"},image_part()]})],
            vec![],
            128,
            &cancel,
            crate::budget::Admission {
                scope: scope.clone(),
                actor: 789,
                guard: ContextGuard {
                    conversation_id: "visual-test".into(),
                    epoch: 1,
                },
                plugin_epoch: 1,
            },
            (&milky, generation),
            async { Ok(()) },
        );
        tokio::pin!(request);
        // Poll the request into the semaphore queue before replacing the login.
        tokio::select! {
            biased;
            result = &mut request => panic!("request bypassed held permit: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        assert_eq!(milky.login().await.unwrap(), 123);
        assert_ne!(milky.scope_generation(&scope).unwrap(), generation);
        drop(held);
        let error = tokio::time::timeout(Duration::from_secs(2), &mut request)
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("account verification expired"));
        assert!(
            tokio::time::timeout(Duration::from_millis(30), model_listener.accept())
                .await
                .is_err()
        );
        assert_eq!(client.requests.available_permits(), 1);
        login_server.abort();
    }

    #[test]
    fn structured_provider_blocks_return_a_fixed_error_without_provider_details() {
        let plain = response(json!({"role":"assistant","content":"provider body secret"}));
        let mut cases = Vec::new();
        for reason in [
            "content_filter",
            "SAFETY",
            "BLOCKLIST",
            "PROHIBITED_CONTENT",
            "SPII",
            "RECITATION",
            "IMAGE_SAFETY",
            "PUP_LIMITED_DISABLED",
        ] {
            let mut blocked = plain.clone();
            blocked["choices"][0]["finish_reason"] = json!(reason);
            cases.push(blocked);
        }
        let mut refusal = plain.clone();
        refusal["choices"][0]["message"]["refusal"] = json!("provider refusal secret");
        cases.push(refusal);
        let mut rating = plain.clone();
        rating["choices"][0]["safetyRatings"] = json!([{"blocked":true}]);
        cases.push(rating);
        cases.extend([
            json!({"promptFeedback":{"blockReason":"SAFETY"},"candidates":[]}),
            json!({"promptFeedback":{"blockReason":"OTHER"},"candidates":[]}),
            json!({"prompt_feedback":{"block_reason":"BLOCKLIST"},"choices":[]}),
            json!({"promptFeedback":{"safetyRatings":[{"blocked":true}]}}),
            json!({"candidates":[{"finishReason":"PROHIBITED_CONTENT"}]}),
            json!({"error":{"code":"content_blocked","message":"provider error secret"}}),
        ]);
        for value in cases {
            let error = parse_response(value, &HashSet::new()).unwrap_err();
            assert!(error.is::<ProviderBlocked>());
            assert_eq!(
                error.to_string(),
                "上游模型拦截了本次请求，可能涉及当前消息或会话上下文，未返回可用回复。"
            );
            assert!(!format!("{error:?}").contains("secret"));
        }
    }

    #[test]
    fn known_google_gateway_notice_is_rejected_but_discussion_and_quotes_are_preserved() {
        for text in [
            GOOGLE_PROMPT_BLOCKED_NOTICE.to_owned(),
            format!("\n{GOOGLE_PROMPT_BLOCKED_NOTICE}\n"),
        ] {
            let error = parse_response(
                response(json!({"role":"assistant","content":text})),
                &HashSet::new(),
            )
            .unwrap_err();
            assert!(error.is::<ProviderBlocked>());
        }
        for text in [
            "我不能帮助实施伤害，但可以介绍安全的替代做法。".to_owned(),
            "Google 的政策可能拒绝敏感内容；这不意味着所有请求都不可用。".to_owned(),
            format!("这条错误的意思是：{GOOGLE_PROMPT_BLOCKED_NOTICE}"),
            format!("\"{GOOGLE_PROMPT_BLOCKED_NOTICE}\""),
            format!("> {GOOGLE_PROMPT_BLOCKED_NOTICE}"),
            format!("```text\n{GOOGLE_PROMPT_BLOCKED_NOTICE}\n```"),
            format!("{GOOGLE_PROMPT_BLOCKED_NOTICE}\n以上是服务错误示例。"),
            GOOGLE_PROMPT_BLOCKED_NOTICE.replace(
                "https://ai.google.dev/gemini-api/docs/troubleshooting",
                "https://example.invalid/troubleshooting",
            ),
            GOOGLE_PROMPT_BLOCKED_NOTICE.replace(". Try rephrasing", ": Try rephrasing"),
        ] {
            let parsed = parse_response(
                response(json!({"role":"assistant","content":text})),
                &HashSet::new(),
            )
            .unwrap();
            assert_eq!(parsed.message["content"], text);
        }
    }

    #[test]
    fn provider_block_rejects_the_entire_response_before_tool_dispatch() {
        let allowed = HashSet::from(["clock_now".to_owned()]);
        let clean = response(json!({
            "role":"assistant", "content":null, "tool_calls":[call("clock")]
        }));
        assert!(parse_response(clean.clone(), &allowed).is_ok());
        let mut filtered = clean.clone();
        filtered["choices"][0]["finish_reason"] = json!("content_filter");
        let mut refused = clean.clone();
        refused["choices"][0]["message"]["refusal"] = json!("provider refusal secret");
        let mut notice = clean.clone();
        notice["choices"][0]["message"]["content"] = json!(GOOGLE_PROMPT_BLOCKED_NOTICE);
        let mut prompt = clean;
        prompt["promptFeedback"] = json!({"blockReason":"SAFETY"});
        for value in [filtered, refused, notice, prompt] {
            // No LlmResponse (and therefore no executable tool call) escapes.
            let error = parse_response(value.clone(), &allowed).unwrap_err();
            assert!(error.is::<ProviderBlocked>());
            // Provider blocking is recognized even before offered-tool validation.
            let error = parse_response(value, &HashSet::new()).unwrap_err();
            assert!(error.is::<ProviderBlocked>());
        }
    }

    #[test]
    fn unblocked_provider_metadata_does_not_change_normal_answers_or_tools() {
        for content in [json!("正常回答"), Value::Null] {
            let mut value = response(json!({
                "role":"assistant", "content":content, "refusal":null,
                "tool_calls":[call("clock")]
            }));
            value["promptFeedback"] = json!({
                "blockReason":"BLOCK_REASON_UNSPECIFIED",
                "safetyRatings":[{"blocked":false,"probability":"LOW"}]
            });
            value["choices"][0]["finish_reason"] = json!("tool_calls");
            value["choices"][0]["safety_ratings"] = json!([{"blocked":false}]);
            let parsed = parse_response(value, &HashSet::from(["clock_now".to_owned()])).unwrap();
            assert_eq!(parsed.message["content"], content);
            assert_eq!(parsed.message["tool_calls"][0], call("clock"));
        }
    }

    #[test]
    fn native_tools_require_valid_arguments_and_offered_name() {
        let mut message = json!({"role":"assistant","content":null,"tool_calls":[call("call1")]});
        let error = parse_response(response(message.clone()), &HashSet::new()).unwrap_err();
        let unavailable = error.downcast_ref::<UnofferedTool>().unwrap();
        assert_eq!(unavailable.requested, "clock_now");
        assert!(unavailable.is_only_call_to("clock_now"));
        assert!(!unavailable.is_only_call_to("web_search"));
        assert!(unavailable.allowed_names.is_empty());
        let allowed = HashSet::from(["clock_now".to_owned()]);
        assert!(parse_response(response(message.clone()), &allowed).is_ok());
        message["tool_calls"][0]["function"]["arguments"] = json!("{bad}");
        assert!(parse_response(response(message), &allowed).is_err());
    }

    #[test]
    fn unoffered_tool_error_only_retains_validated_names_and_rejects_the_whole_batch() {
        let allowed = HashSet::from(["reply_message".to_owned(), "clock_now".to_owned()]);
        let unoffered = json!({"id":"private-call-id","type":"function","function":{
            "name":"web_search","arguments":"{\"query\":\"private search secret\"}"
        }});
        for calls in [
            vec![call("allowed"), unoffered.clone()],
            vec![unoffered, call("allowed")],
        ] {
            let value = response(json!({
                "role":"assistant","content":"private provider content","tool_calls":calls
            }));
            let error = parse_response(value.clone(), &allowed).unwrap_err();
            let typed = error.downcast_ref::<UnofferedTool>().unwrap();
            assert_eq!(typed.requested, "web_search");
            assert!(!typed.is_only_call_to("web_search"));
            assert_eq!(typed.allowed_names, ["clock_now", "reply_message"]);
            assert_eq!(
                error.to_string(),
                "模型请求了本轮未开放的工具（web_search），未执行。"
            );
            assert!(std::error::Error::source(typed).is_none());
            for private in ["private", "secret", "arguments", "query"] {
                assert!(!format!("{error:#?}").contains(private));
            }
            let mut truncated = value;
            truncated["choices"][0]["finish_reason"] = json!("length");
            assert!(
                parse_response(truncated.clone(), &allowed)
                    .unwrap_err()
                    .is::<TruncatedToolResponse>()
            );
            truncated["choices"][0]["message"]["refusal"] = json!("private refusal");
            assert!(
                parse_response(truncated, &allowed)
                    .unwrap_err()
                    .is::<ProviderBlocked>()
            );
        }
    }

    #[test]
    fn invalid_unoffered_names_are_never_retained_in_diagnostics() {
        for name in [
            "private name".to_owned(),
            "private\nname".to_owned(),
            "private-name".repeat(6),
            "private-工具".to_owned(),
        ] {
            let mut entry = call("private-call-id");
            entry["function"]["name"] = json!(name);
            let error = parse_response(
                response(json!({"role":"assistant","content":null,"tool_calls":[entry]})),
                &HashSet::new(),
            )
            .unwrap_err();
            assert!(!error.is::<UnofferedTool>());
            assert_eq!(error.to_string(), "invalid tool call name");
            assert!(!format!("{error:#?}").contains("private"));
        }
    }

    #[tokio::test]
    async fn unoffered_tool_http_diagnostics_only_log_requested_and_allowed_names() {
        use tracing::instrument::WithSubscriber;
        let message = json!({"role":"assistant","content":"private provider content",
        "tool_calls":[{"id":"private-call-id","type":"function","function":{
            "name":"web_search","arguments":"{\"query\":\"private search secret\"}"
        }}]});
        let body = response(message).to_string();
        let (url, server) = server("200 OK", &body).await;
        let client = Llm::new(
            config(url),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        )
        .unwrap();
        let (log, subscriber) = diagnostic_log();
        let error = client.complete(
            vec![json!({"role":"user","content":"private request"})],
            vec![
                json!({"type":"function","function":{"name":"reply_message","parameters":{"type":"object"}}}),
                json!({"type":"function","function":{"name":"clock_now","parameters":{"type":"object"}}}),
            ],
            128,
            &CancellationToken::new(),
        ).with_subscriber(subscriber).await.unwrap_err();
        server.await.unwrap();
        assert!(error.is::<UnofferedTool>());
        let logged = log.text();
        for field in [
            "model requested a tool not offered in this request",
            "requested=\"web_search\"",
            "allowed_names=[\"clock_now\", \"reply_message\"]",
        ] {
            assert!(logged.contains(field), "missing diagnostic field {field}");
        }
        for private in [
            "private",
            "secret",
            "arguments",
            "llm-only",
            "http://",
            "Bearer",
        ] {
            assert!(!logged.contains(private));
            assert!(!format!("{error:#?}").contains(private));
        }
    }

    #[test]
    fn invalid_tool_ids_report_safe_reasons_and_preserve_valid_opaque_ids() {
        let allowed = HashSet::from(["clock_now".to_owned()]);
        for (id, reason) in [
            (String::new(), InvalidCallIdReason::Empty),
            ("x".repeat(129), InvalidCallIdReason::TooLong),
            ("private-id\n".to_owned(), InvalidCallIdReason::Control),
            (
                "private-id\u{0085}".to_owned(),
                InvalidCallIdReason::Control,
            ),
        ] {
            let message = json!({"role":"assistant","content":null,"tool_calls":[call(&id)]});
            let error = parse_response(response(message), &allowed).unwrap_err();
            let typed = error.downcast_ref::<InvalidToolCallId>().unwrap();
            assert_eq!(typed.reason, reason);
            assert_eq!(typed.id_bytes, Some(id.len()));
            assert_eq!(
                error.to_string(),
                "模型返回的工具调用标识无效，本批工具调用未执行。"
            );
            assert!(std::error::Error::source(typed).is_none());
            assert!(!format!("{error:#?}").contains("private"));
        }
        for id in [
            "x".repeat(128),
            "界".repeat(42) + "ab",
            "opaque /+= id".to_owned(),
        ] {
            assert!(validate_tool_call_id(&id).is_ok());
            let message = json!({"role":"assistant","content":null,"tool_calls":[call(&id)]});
            let parsed = parse_response(response(message.clone()), &allowed).unwrap();
            assert_eq!(parsed.message, message);
            assert!(
                validate_messages(&[
                    parsed.message,
                    json!({"role":"tool","tool_call_id":id,"content":"result"})
                ])
                .is_ok()
            );
        }
    }

    #[test]
    fn missing_and_non_string_tool_ids_have_distinct_safe_diagnostics() {
        let allowed = HashSet::from(["clock_now".to_owned()]);
        for value in [
            None,
            Some(Value::Null),
            Some(json!(123)),
            Some(json!({"private-id":true})),
        ] {
            let reason = if value.is_none() {
                InvalidCallIdReason::Missing
            } else {
                InvalidCallIdReason::NotString
            };
            let mut entry = call("unused");
            match value {
                Some(value) => entry["id"] = value,
                None => {
                    entry.as_object_mut().unwrap().remove("id");
                }
            }
            let message = json!({"role":"assistant","content":null,"tool_calls":[entry]});
            let error = parse_response(response(message), &allowed).unwrap_err();
            let typed = error.downcast_ref::<InvalidToolCallId>().unwrap();
            assert_eq!(typed.reason, reason);
            assert_eq!(typed.id_bytes, None);
            assert!(!format!("{error:#?}").contains("private"));
        }
    }

    #[test]
    fn invalid_tool_id_rejects_whole_batch_after_provider_block_and_length_checks() {
        let allowed = HashSet::from(["clock_now".to_owned()]);
        for calls in [vec![call("valid"), call("")], vec![call(""), call("valid")]] {
            let value =
                response(json!({"role":"assistant","content":"private text","tool_calls":calls}));
            assert!(
                parse_response(value.clone(), &allowed)
                    .unwrap_err()
                    .is::<InvalidToolCallId>()
            );
            let mut truncated = value.clone();
            truncated["choices"][0]["finish_reason"] = json!("length");
            assert!(
                parse_response(truncated.clone(), &allowed)
                    .unwrap_err()
                    .is::<TruncatedToolResponse>()
            );
            truncated["choices"][0]["message"]["refusal"] = json!("private refusal");
            assert!(
                parse_response(truncated, &allowed)
                    .unwrap_err()
                    .is::<ProviderBlocked>()
            );
        }
    }

    #[tokio::test]
    async fn invalid_tool_id_http_diagnostics_never_log_id_or_arguments() {
        use tracing::instrument::WithSubscriber;
        let message = json!({"role":"assistant","content":"private provider content",
        "tool_calls":[{"id":"private-id\n","type":"function","function":{
            "name":"clock_now","arguments":"{\"private-argument\":\"secret\"}"
        }}]});
        let body = response(message).to_string();
        let (url, server) = server("200 OK", &body).await;
        let client = Llm::new(
            config(url),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        )
        .unwrap();
        let (log, subscriber) = diagnostic_log();
        let error = client.complete(
            vec![json!({"role":"user","content":"private request"})],
            vec![json!({"type":"function","function":{"name":"clock_now","parameters":{"type":"object"}}})],
            128,
            &CancellationToken::new(),
        ).with_subscriber(subscriber).await.unwrap_err();
        server.await.unwrap();
        assert!(error.is::<InvalidToolCallId>());
        let logged = log.text();
        for field in [
            "model tool response had invalid call id",
            "reason=\"control\"",
            "id_bytes=11",
            "max_id_bytes=128",
        ] {
            assert!(logged.contains(field), "missing diagnostic field {field}");
        }
        for private in [
            "private",
            "secret",
            "clock_now",
            "llm-only",
            "http://",
            "Bearer",
        ] {
            assert!(!logged.contains(private));
            assert!(!format!("{error:#?}").contains(private));
        }
    }

    #[test]
    fn truncated_tool_response_rejects_complete_and_partial_arguments() {
        let allowed = HashSet::from(["search_stickers".to_owned()]);
        let message = json!({"role":"assistant","content":"private provider content",
        "tool_calls":[{"id":"find","type":"function","function":{
            "name":"search_stickers","arguments":"{\"query\":\"合成庆祝\"}"
        }}]});
        for finish_reason in ["stop", "tool_calls"] {
            let mut complete = response(message.clone());
            complete["choices"][0]["finish_reason"] = json!(finish_reason);
            assert_eq!(parse_response(complete, &allowed).unwrap().message, message);
        }
        for arguments in [r#"{"query":"合成庆祝"}"#, r#"{"query":"private unfinished"#] {
            let mut truncated = response(message.clone());
            truncated["choices"][0]["finish_reason"] = json!("length");
            truncated["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] =
                json!(arguments);
            let error = parse_response(truncated, &allowed).unwrap_err();
            assert!(error.is::<TruncatedToolResponse>());
            assert_eq!(
                error.to_string(),
                "模型输出达到长度上限，工具调用未完整生成，本次调用未执行。"
            );
            assert!(!format!("{error:#}").contains("private"));
        }
    }

    #[test]
    fn truncated_tool_response_does_not_override_provider_blocking_or_plain_text() {
        let mut blocked = response(json!({"role":"assistant","content":null,
            "refusal":"private provider refusal", "tool_calls":[call("one")]}));
        blocked["choices"][0]["finish_reason"] = json!("length");
        assert!(
            parse_response(blocked, &HashSet::new())
                .unwrap_err()
                .is::<ProviderBlocked>()
        );
        let mut text = response(json!({"role":"assistant","content":"ordinary partial text"}));
        text["choices"][0]["finish_reason"] = json!("length");
        assert_eq!(
            parse_response(text, &HashSet::new()).unwrap().message["content"],
            "ordinary partial text"
        );
    }

    #[test]
    fn truncation_metadata_retains_only_unsigned_integer_counters() {
        let mut value = response(json!({"role":"assistant","content":"private text",
            "tool_calls":[{"function":{"name":"private-name","arguments":"private-args"}}]}));
        value["usage"] = json!({"prompt_tokens":123,"completion_tokens":800,"total_tokens":923,
            "completion_tokens_details":{"reasoning_tokens":768}});
        assert_eq!(
            TruncationMetadata::from_response(&value),
            TruncationMetadata {
                prompt_tokens: Some(123),
                completion_tokens: Some(800),
                total_tokens: Some(923),
                reasoning_tokens: Some(768),
                tool_call_count: 1,
            }
        );
        value["usage"] = json!({"prompt_tokens":-1,"completion_tokens":"private-counter",
            "total_tokens":1.5,"completion_tokens_details":{"reasoning_tokens":{"secret":"private"}}});
        let metadata = TruncationMetadata::from_response(&value);
        assert_eq!(
            metadata,
            TruncationMetadata {
                prompt_tokens: None,
                completion_tokens: None,
                total_tokens: None,
                reasoning_tokens: None,
                tool_call_count: 1,
            }
        );
        assert!(!format!("{metadata:?}").contains("private"));
    }

    #[test]
    fn bounded_batches_are_preserved_for_dispatcher_rejection() {
        let mut calls: Vec<_> = (0..32)
            .map(|index| call(&format!("call-{index}")))
            .collect();
        calls[31]["function"]["name"] = json!("reply_message");
        calls[31]["function"]["arguments"] = json!(r#"{"text":"final response"}"#);
        let allowed = HashSet::from(["clock_now".to_owned(), "reply_message".to_owned()]);
        let message = json!({"role":"assistant","content":null,"tool_calls":calls});
        let parsed = parse_response(response(message.clone()), &allowed).unwrap();
        assert_eq!(parsed.message, message);

        let mut oversized = message;
        oversized["tool_calls"]
            .as_array_mut()
            .unwrap()
            .push(call("overflow"));
        assert!(parse_response(response(oversized), &allowed).is_err());
    }

    #[test]
    fn any_invalid_member_rejects_the_entire_batch() {
        let allowed = HashSet::from(["clock_now".to_owned()]);
        let mut invalid = Vec::new();
        invalid.push(call("first")); // A duplicate of the first member.
        invalid.push(call("bad\nid"));
        invalid.push(call(&"x".repeat(129)));
        for (field, value) in [("type", json!("not-a-function")), ("id", Value::Null)] {
            let mut entry = call("second");
            entry[field] = value;
            invalid.push(entry);
        }
        for (field, value) in [
            ("name", json!("not_offered")),
            ("name", json!("bad name")),
            ("arguments", json!({})),
            ("arguments", json!("{invalid}")),
            ("arguments", json!("[]")),
            (
                "arguments",
                json!(format!(r#"{{"value":"{}"}}"#, "x".repeat(8192))),
            ),
        ] {
            let mut entry = call("second");
            entry["function"][field] = value;
            invalid.push(entry);
        }
        for entry in invalid {
            let message =
                json!({"role":"assistant","content":null,"tool_calls":[call("first"),entry]});
            assert!(parse_response(response(message), &allowed).is_err());
        }
    }

    #[test]
    fn rejected_batch_protocol_requires_each_result_once_before_continuing() {
        let assistant = json!({"role":"assistant","content":null,"tool_calls":[call("one"),call("two"),call("three")]});
        let result = |id: &str| json!({"role":"tool","tool_call_id":id,"content":"batch rejected; not executed"});
        let complete = vec![
            assistant.clone(),
            result("one"),
            result("two"),
            result("three"),
        ];
        assert!(validate_messages(&complete).is_ok());
        assert!(validate_messages(&complete[..3]).is_err());
        assert!(
            validate_messages(&[
                assistant.clone(),
                result("one"),
                result("one"),
                result("three")
            ])
            .is_err()
        );
        assert!(
            validate_messages(&[
                assistant.clone(),
                result("one"),
                result("two"),
                result("unknown")
            ])
            .is_err()
        );
        assert!(
            validate_messages(&[
                assistant.clone(),
                result("one"),
                json!({"role":"user","content":"interleaved"}),
                result("two"),
                result("three")
            ])
            .is_err()
        );
        let mut continued = complete;
        continued.push(json!({"role":"assistant","content":"one corrected final response"}));
        assert!(validate_messages(&continued).is_ok());
        continued.push(json!({"role":"assistant","content":null,"tool_calls":[call("one")]}));
        continued.push(result("one"));
        assert!(validate_messages(&continued).is_err());
    }

    #[test]
    fn textual_tool_requests_remain_plain_text() {
        let text = r#"{"tool":"reminder_create","arguments":{"text":"hello"}}"#;
        let parsed = parse_response(
            response(json!({"role":"assistant","content":text})),
            &HashSet::new(),
        )
        .unwrap();
        assert!(parsed.message.get("tool_calls").is_none());
        assert_eq!(parsed.message["content"], text);
    }

    #[test]
    fn tool_protocol_pairs_cannot_be_orphaned_or_split() {
        let assistant = json!({"role":"assistant","content":null,"tool_calls":[call("one")]});
        let result = json!({"role":"tool","tool_call_id":"one","content":"result"});
        assert!(validate_messages(&[assistant.clone(), result.clone()]).is_ok());
        assert!(validate_messages(std::slice::from_ref(&assistant)).is_err());
        assert!(validate_messages(&[result]).is_err());
        assert!(validate_messages(&[assistant, json!({"role":"user","content":"hello"})]).is_err());
    }

    #[test]
    fn expanded_conversation_history_is_supported_with_a_finite_protocol_cap() {
        let history: Vec<Value> = (0..500)
            .map(|index| {
                json!({
                    "role": if index % 2 == 0 { "user" } else { "assistant" },
                    "content": format!("conversation message {index}")
                })
            })
            .collect();
        assert!(validate_messages(&history).is_ok());

        let mut at_limit = vec![json!({"role":"user","content":"bounded history"}); 8192];
        assert!(validate_messages(&at_limit).is_ok());
        at_limit.push(json!({"role":"user","content":"one more message"}));
        assert!(validate_messages(&at_limit).is_err());
    }

    #[tokio::test]
    async fn http_success_preserves_usage_and_auth_is_request_local() {
        let body = response(json!({"role":"assistant","content":"hello"})).to_string();
        let (url, server) = server("200 OK", &body).await;
        let client = Llm::new(config(url), reqwest::Client::new()).unwrap();
        let result = client
            .complete(
                vec![json!({"role":"user","content":"hi"})],
                vec![],
                128,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.usage.input_tokens, 10);
        assert_eq!(result.usage.output_tokens, 2);
        let request = server.await.unwrap();
        assert!(request.contains("Bearer llm-only"));
        assert!(!request.contains("milky-only"));
        assert!(request.contains("\"max_tokens\":128"));
    }

    #[tokio::test]
    async fn tool_choice_closes_tools_only_for_tool_capable_history_followups() {
        for (tool_calling, has_history, offer_tools, expected_choice) in [
            (true, true, false, Some("none")),
            (true, false, false, None),
            (false, true, false, None),
            (true, true, true, Some("auto")),
        ] {
            let body = response(json!({"role":"assistant","content":"done"})).to_string();
            let (url, server) = server("200 OK", &body).await;
            let mut config = config(url);
            config.tool_calling = tool_calling;
            let client = Llm::new(
                config,
                reqwest::Client::builder().no_proxy().build().unwrap(),
            )
            .unwrap();
            let mut messages = vec![json!({"role":"user","content":"time please"})];
            if has_history {
                messages
                    .push(json!({"role":"assistant","content":null,"tool_calls":[call("clock")]}));
                messages
                    .push(json!({"role":"tool","tool_call_id":"clock","content":"time result"}));
            }
            let tools = if offer_tools {
                vec![
                    json!({"type":"function","function":{"name":"clock_now","parameters":{"type":"object"}}}),
                ]
            } else {
                vec![]
            };
            client
                .complete(messages, tools, 128, &CancellationToken::new())
                .await
                .unwrap();
            let request = server.await.unwrap();
            let (_, body) = request.split_once("\r\n\r\n").unwrap();
            let body: Value = serde_json::from_str(body).unwrap();
            assert_eq!(
                body.get("tool_choice").and_then(Value::as_str),
                expected_choice
            );
            assert_eq!(body.get("tools").is_some(), offer_tools);
            assert_eq!(body.get("parallel_tool_calls").is_some(), offer_tools);
        }
    }

    #[tokio::test]
    async fn http_diagnostics_classify_header_timeout_without_logging_request_content() {
        use tracing::instrument::WithSubscriber;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/private-endpoint", listener.local_addr().unwrap());
        let release = CancellationToken::new();
        let wait = release.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_synthetic_request(&mut stream).await;
            wait.cancelled().await;
        });
        let mut cfg = config(url);
        cfg.request_timeout_secs = 1;
        let client = Llm::new(cfg, reqwest::Client::builder().no_proxy().build().unwrap()).unwrap();
        let (log, subscriber) = diagnostic_log();
        let result = client.complete(
            vec![json!({"role":"user","content":[
                {"type":"text","text":"private-message"}, image_part()
            ]})],
            vec![json!({"type":"function","function":{"name":"private_tool","parameters":{"type":"object"}}})],
            128, &CancellationToken::new(),
        ).with_subscriber(subscriber).await;
        release.cancel();
        server.await.unwrap();
        let error = result.unwrap_err();
        assert_eq!(
            error.downcast_ref::<ModelHttpFailure>().unwrap().0,
            TransportKind::Timeout
        );
        assert!(error.to_string().contains("模型请求超时"));
        let logged = log.text();
        for field in [
            "phase=\"headers\"",
            "kind=\"timeout\"",
            "elapsed_ms=",
            "timeout_secs=1",
            "image_count=1",
            "max_output=128",
            "tool_count=1",
            "request_bytes=",
        ] {
            assert!(logged.contains(field), "missing diagnostic field {field}");
        }
        for secret in [
            "private-endpoint",
            "private-message",
            "private_tool",
            "llm-only",
            "data:image",
            "http://",
            "Bearer",
        ] {
            assert!(!logged.contains(secret));
            assert!(!format!("{error:#}").contains(secret));
        }
    }

    #[tokio::test]
    async fn http_diagnostics_distinguish_connection_refusal_from_timeout() {
        use tracing::instrument::WithSubscriber;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/private-endpoint", listener.local_addr().unwrap());
        drop(listener);
        let client = Llm::new(
            config(url),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        )
        .unwrap();
        let (log, subscriber) = diagnostic_log();
        let error = client
            .complete(
                vec![json!({"role":"user","content":"private-message"})],
                vec![],
                128,
                &CancellationToken::new(),
            )
            .with_subscriber(subscriber)
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ModelHttpFailure>().unwrap().0,
            TransportKind::Connect
        );
        assert!(error.to_string().contains("无法连接模型服务"));
        let logged = log.text();
        assert!(logged.contains("kind=\"connect\""));
        assert!(logged.contains("image_count=0"));
        assert!(logged.contains("tool_count=0"));
        assert!(
            !logged.contains("private")
                && !logged.contains("llm-only")
                && !logged.contains("http://")
        );
    }

    #[tokio::test]
    async fn http_diagnostics_do_not_misclassify_a_truncated_response_body_as_headers() {
        use tokio::io::AsyncWriteExt;
        use tracing::instrument::WithSubscriber;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_synthetic_request(&mut stream).await;
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{\"private-body\":").await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let client = Llm::new(
            config(url),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        )
        .unwrap();
        let (log, subscriber) = diagnostic_log();
        let error = client
            .complete(
                vec![json!({"role":"user","content":"private-message"})],
                vec![],
                128,
                &CancellationToken::new(),
            )
            .with_subscriber(subscriber)
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(!error.is::<ModelHttpFailure>());
        assert_eq!(error.to_string(), "HTTP response body could not be read");
        assert!(!log.text().contains("phase=\"headers\""));
        assert!(!format!("{error:#}").contains("private"));
    }

    #[tokio::test]
    async fn http_diagnostics_classify_closed_connection_before_headers_as_request_failure() {
        use tokio::io::AsyncWriteExt;
        use tracing::instrument::WithSubscriber;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/private-endpoint", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_synthetic_request(&mut stream).await;
            stream.shutdown().await.unwrap();
        });
        let client = Llm::new(
            config(url),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        )
        .unwrap();
        let (log, subscriber) = diagnostic_log();
        let error = client
            .complete(
                vec![json!({"role":"user","content":"private-message"})],
                vec![],
                128,
                &CancellationToken::new(),
            )
            .with_subscriber(subscriber)
            .await
            .unwrap_err();
        server.await.unwrap();
        assert_eq!(
            error.downcast_ref::<ModelHttpFailure>().unwrap().0,
            TransportKind::Request
        );
        assert_eq!(
            error.to_string(),
            "模型请求传输失败，当前请求尚未取得模型响应。"
        );
        let logged = log.text();
        assert!(logged.contains("phase=\"headers\"") && logged.contains("kind=\"request\""));
        if let Some((_, suffix)) = logged.split_once("io_kind=\"") {
            let kind = suffix.split('"').next().unwrap();
            assert!(
                [
                    "connection_reset",
                    "connection_aborted",
                    "connection_refused",
                    "broken_pipe",
                    "unexpected_eof",
                    "timed_out",
                    "other"
                ]
                .contains(&kind)
            );
        }
        for secret in ["private", "llm-only", "http://", "Bearer"] {
            assert!(!logged.contains(secret) && !format!("{error:#}").contains(secret));
        }
    }

    #[tokio::test]
    async fn http_200_provider_error_is_rejected() {
        let (url, server) = server("200 OK", r#"{"error":{"message":"failed"}}"#).await;
        let client = Llm::new(config(url), reqwest::Client::new()).unwrap();
        assert!(
            client
                .complete(
                    vec![json!({"role":"user","content":"hi"})],
                    vec![],
                    128,
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_does_not_consume_a_request_slot() {
        let client = Llm::new(config("http://127.0.0.1:1".into()), reqwest::Client::new()).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            client
                .complete(
                    vec![json!({"role":"user","content":"hi"})],
                    vec![],
                    128,
                    &cancel
                )
                .await
                .is_err()
        );
        assert_eq!(client.requests.available_permits(), 2);
    }

    #[tokio::test]
    async fn background_work_skips_when_foreground_owns_capacity() {
        let mut cfg = config("http://127.0.0.1:1".into());
        cfg.max_concurrent_requests = 1;
        let client = Llm::new(cfg, reqwest::Client::new()).unwrap();
        let _permit = client.requests.clone().acquire_owned().await.unwrap();
        assert!(
            client
                .complete_background(
                    vec![json!({"role":"user","content":"hi"})],
                    128,
                    &CancellationToken::new()
                )
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn learning_skips_busy_capacity_and_reserves_its_own_quota_before_http() {
        use axum::{Json, Router, http::StatusCode, routing::post};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let store = crate::store::Store::open(":memory:", 32).await.unwrap();
        store.activate_plugins(vec!["ai".into()]).await.unwrap();
        let plugin_epoch = store.epoch("ai").await.unwrap();
        store.call(|db| {
            db.execute("INSERT INTO ai_conversations(id,scope,mode) VALUES('learning','1:group:2','group')", [])?;
            Ok(())
        }).await.unwrap();
        let budget = crate::budget::Budget::new(
            store.clone(),
            crate::config::Limits {
                learning_requests_per_hour: 1,
                learning_requests_per_day: 2,
                ..Default::default()
            },
        );
        budget.init().await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/",
            post({
                let budget = budget.clone();
                let calls = calls.clone();
                move |Json(body): Json<Value>| {
                    let budget = budget.clone();
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        assert!(body.get("tools").is_none());
                        assert_eq!(body["max_tokens"], 256);
                        assert_eq!(budget.snapshot().await.unwrap()["used"]["learning"], 1);
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error":{"message":"temporary failure"}})),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Llm::new(
            config(format!("http://{}/", listener.local_addr().unwrap())),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        )
        .unwrap()
        .with_budget(budget.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let guard = crate::types::ContextGuard {
            conversation_id: "learning".into(),
            epoch: 1,
        };
        let input = vec![json!({"role":"user","content":"learning evidence"})];
        let cancel = CancellationToken::new();
        let held = client.requests.clone().acquire_many_owned(2).await.unwrap();
        assert!(
            client
                .complete_learning_admitted(
                    input.clone(),
                    256,
                    &cancel,
                    guard.clone(),
                    plugin_epoch
                )
                .await
                .unwrap()
                .is_none()
        );
        drop(held);
        let held = client.background.clone().acquire_owned().await.unwrap();
        assert!(
            client
                .complete_learning_admitted(
                    input.clone(),
                    256,
                    &cancel,
                    guard.clone(),
                    plugin_epoch
                )
                .await
                .unwrap()
                .is_none()
        );
        drop(held);
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            client
                .complete_learning_admitted(
                    input.clone(),
                    256,
                    &cancelled,
                    guard.clone(),
                    plugin_epoch
                )
                .await
                .is_err()
        );
        assert!(
            client
                .complete_learning_admitted(
                    input.clone(),
                    256,
                    &cancel,
                    crate::types::ContextGuard {
                        epoch: 2,
                        ..guard.clone()
                    },
                    plugin_epoch
                )
                .await
                .is_err()
        );
        assert_eq!(budget.snapshot().await.unwrap()["used"]["learning_day"], 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // An HTTP failure remains charged, and a second attempt is rejected
        // before HTTP without consuming either existing model quota.
        assert!(
            client
                .complete_learning_admitted(
                    input.clone(),
                    256,
                    &cancel,
                    guard.clone(),
                    plugin_epoch
                )
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let error = client
            .complete_learning_admitted(input, 256, &cancel, guard, plugin_epoch)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("每小时请求额度"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            budget.snapshot().await.unwrap()["used"],
            json!({"foreground":0,"background":0,"learning":1,"learning_day":1})
        );
        assert_eq!(client.requests.available_permits(), 2);
        assert_eq!(client.background.available_permits(), 1);
        server.abort();
        store.stop().await.unwrap();
    }
}
