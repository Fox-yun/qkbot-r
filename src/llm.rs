//! Chat Completions compatible transport. Tool execution is exclusively the host's job.
use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::milky::read_bounded;

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    pub chat_completions_url: String,
    pub api_key: String,
    pub model: String,
    pub tool_calling: bool,
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
        self.request(messages, tools, max_output, cancel, permit, None)
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
        self.request(messages, Vec::new(), max_output, cancel, permit, admission)
            .await
            .map(Some)
    }

    async fn request(
        &self,
        messages: Vec<Value>,
        tools: Vec<Value>,
        max_output: usize,
        cancel: &CancellationToken,
        _permit: OwnedSemaphorePermit,
        admission: Option<crate::budget::RequestAdmission>,
    ) -> Result<LlmResponse> {
        ensure!(
            (1..=32768).contains(&max_output),
            "output token budget must be 1..32768"
        );
        validate_messages(&messages)?;
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
            if let Some(budget) = &self.budget {
                ensure!(
                    admission.is_some(),
                    "model request requires a runtime admission"
                );
                budget.reserve_request(admission).await?;
            }
            ensure!(!cancel.is_cancelled(), "model request cancelled");
            let mut request = self
                .http
                .post(self.endpoint.clone())
                .timeout(Duration::from_secs(self.config.request_timeout_secs))
                .json(&body);
            if !self.config.api_key.is_empty() {
                request = request.bearer_auth(&self.config.api_key);
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

fn validate_messages(messages: &[Value]) -> Result<()> {
    ensure!(
        !messages.is_empty() && messages.len() <= 8192,
        "model messages must contain 1..8192 entries"
    );
    let mut pending = HashSet::new();
    let mut used_ids = HashSet::new();
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
        ensure!(
            message["content"].is_string() || (role == "assistant" && message["content"].is_null()),
            "message content must be text"
        );
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
    Ok(())
}

fn parse_response(value: Value, allowed_tools: &HashSet<String>) -> Result<LlmResponse> {
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
