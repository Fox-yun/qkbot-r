//! Chat Completions compatible transport. Tool execution is exclusively the host's job.
use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::milky::read_bounded;

pub(crate) const MAX_INPUT_IMAGES: usize = 4;
pub(crate) const MAX_IMAGE_URL_BYTES: usize = 1_400_000;

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
        )
        .await
    }

    /// Visual inputs are tied to the QQ login that supplied their pixels,
    /// including while waiting for an available model request slot.
    pub(crate) async fn complete_visual_admitted(
        &self,
        messages: Vec<Value>,
        tools: Vec<Value>,
        max_output: usize,
        cancel: &CancellationToken,
        admission: crate::budget::Admission,
        account: (&crate::milky::Milky, u64),
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
        self.request(messages, tools, max_output, cancel, permit, None, None)
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
        let mut body = json!({"model": self.config.model, "messages": messages,
            "max_tokens": max_output, "stream": false});
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
            body["parallel_tool_calls"] = json!(false);
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
            let response = request
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("model HTTP transport failed"))?;
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
            parse_response(value, &allowed_tools)
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
            let id = call["id"].as_str().context("tool call missing id")?;
            ensure!(
                !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control),
                "invalid tool call id"
            );
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
    let calls = tool_calls(message)?;
    for (_, name) in &calls {
        ensure!(
            allowed_tools.contains(name),
            "model called a tool not offered in this request"
        );
    }
    ensure!(
        !calls.is_empty()
            || message["content"]
                .as_str()
                .is_some_and(|text| !text.trim().is_empty()),
        "model response contains neither text nor a tool call"
    );
    ensure!(
        calls.is_empty() || choices[0]["finish_reason"] != "length",
        "model tool call was truncated"
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
        let mut messages = vec![first.clone(), first];
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
        assert!(parse_response(response(message.clone()), &HashSet::new()).is_err());
        let allowed = HashSet::from(["clock_now".to_owned()]);
        assert!(parse_response(response(message.clone()), &allowed).is_ok());
        message["tool_calls"][0]["function"]["arguments"] = json!("{bad}");
        assert!(parse_response(response(message), &allowed).is_err());
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
}
