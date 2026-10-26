//! Stateless Responses adaptation; replay belongs only to the current tool loop.
use super::*;

pub(super) const REPLAY: &str = "_responses_output";

pub(super) fn request(chat: Value) -> Result<Value> {
    let messages = chat["messages"]
        .as_array()
        .context("missing model messages")?;
    let mut input = Vec::new();
    for message in messages {
        let role = message["role"].as_str().context("missing message role")?;
        if let Some(items) = message.get(REPLAY) {
            ensure!(
                role == "assistant",
                "Responses replay must belong to the assistant"
            );
            let items = items.as_array().context("invalid Responses replay")?;
            let projected = output_message(items)?;
            ensure!(
                projected["content"] == message["content"]
                    && projected["tool_calls"] == message["tool_calls"],
                "Responses replay does not match tool history"
            );
            input.extend(items.iter().cloned());
            continue;
        }
        if role == "tool" {
            input.push(json!({"type":"function_call_output", "call_id":message["tool_call_id"], "output":message["content"]}));
            continue;
        }
        if !message["content"].is_null() {
            let content = if let Some(parts) = message["content"].as_array() {
                Value::Array(parts.iter().map(|part| match part["type"].as_str() {
                    Some("text") => Ok(json!({"type":"input_text", "text":part["text"]})),
                    Some("image_url") => Ok(json!({"type":"input_image", "image_url":part["image_url"]["url"], "detail":part["image_url"]["detail"]})),
                    _ => anyhow::bail!("unsupported Responses input content"),
                }).collect::<Result<Vec<_>>>()?)
            } else {
                message["content"].clone()
            };
            input.push(json!({"role":role, "content":content}));
        }
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                input.push(json!({"type":"function_call", "call_id":call["id"],
                    "name":call["function"]["name"], "arguments":call["function"]["arguments"]}));
            }
        }
    }
    let mut body = json!({"model":chat["model"], "input":input, "max_output_tokens":chat["max_tokens"],
        "stream":false, "store":false, "include":["reasoning.encrypted_content"]});
    if let Some(tools) = chat["tools"].as_array() {
        body["tools"] = Value::Array(tools.iter().map(|tool| {
            let function = &tool["function"];
            let mut definition = json!({"type":"function", "name":function["name"], "parameters":function["parameters"],
                // Existing tool schemas contain optional non-null fields; retain
                // their semantics instead of the Responses strict-mode default.
                "strict":function.get("strict").cloned().unwrap_or(json!(false))});
            if let Some(description) = function.get("description") { definition["description"] = description.clone(); }
            definition
        }).collect());
        body["parallel_tool_calls"] = json!(false);
    }
    if let Some(choice) = chat.get("tool_choice") {
        body["tool_choice"] = choice.clone();
    }
    Ok(body)
}

fn output_message(items: &[Value]) -> Result<Value> {
    ensure!(items.len() <= 128, "too many Responses output items");
    let mut text = Vec::new();
    let mut calls = Vec::new();
    let mut item_ids = HashSet::new();
    for item in items {
        ensure!(item.is_object(), "invalid Responses output item");
        if let Some(id) = item.get("id") {
            let id = id.as_str().context("invalid Responses item id")?;
            ensure!(
                !id.is_empty()
                    && id.len() <= 256
                    && !id.chars().any(char::is_control)
                    && item_ids.insert(id),
                "invalid or duplicate Responses item id"
            );
        }
        ensure!(
            item.get("status")
                .is_none_or(|status| status.is_null() || status == "completed"),
            "Responses output item is not complete"
        );
        match item["type"].as_str() {
            Some("reasoning") => {
                ensure!(
                    item.get("encrypted_content")
                        .is_none_or(|v| v.is_null() || v.is_string()),
                    "invalid encrypted reasoning"
                );
                ensure!(item["summary"].is_array(), "invalid reasoning summary");
            }
            Some("message") => {
                ensure!(
                    item["role"] == "assistant",
                    "Responses output role must be assistant"
                );
                for part in item["content"]
                    .as_array()
                    .context("invalid Responses message content")?
                {
                    if part["type"] == "refusal" {
                        return Err(ProviderBlocked.into());
                    }
                    ensure!(
                        part["type"] == "output_text",
                        "unsupported Responses output content"
                    );
                    text.push(
                        part["text"]
                            .as_str()
                            .context("Responses output text must be a string")?,
                    );
                }
            }
            Some("function_call") => calls.push(json!({"id":item["call_id"], "type":"function",
                "function":{"name":item["name"], "arguments":item["arguments"]}})),
            _ => anyhow::bail!("unsupported Responses output item type"),
        }
    }
    let mut message = json!({"role":"assistant", "content":if text.is_empty() { Value::Null } else { json!(text.join("\n")) }});
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    // Reuse the host's bounded ID/argument validation, including duplicate IDs.
    tool_calls(&message)?;
    Ok(message)
}

pub(super) fn parse(value: Value, allowed_tools: &HashSet<String>) -> Result<LlmResponse> {
    // Reject every item if any part is refused, including a mixed tool/refusal response.
    if provider_blocked(&value)
        || blocked_finish_reason(&value["incomplete_details"]["reason"])
        || value["output"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|p| p["type"] == "refusal"))
            })
        })
    {
        return Err(ProviderBlocked.into());
    }
    ensure!(
        value.get("error").is_none_or(Value::is_null),
        "model provider returned an error"
    );
    let items = value["output"]
        .as_array()
        .context("model response missing output items")?;
    if value["status"] == "incomplete"
        && value["incomplete_details"]["reason"] == "max_output_tokens"
        && items.iter().any(|item| item["type"] == "function_call")
    {
        return Err(TruncatedToolResponse.into());
    }
    ensure!(
        value["status"] == "completed",
        "Responses generation did not complete"
    );
    let message = output_message(items)?;
    let mut parsed = super::parse_response(
        json!({"model":value["model"],
            "choices":[{"finish_reason":"stop", "message":message}],
            "usage":{"prompt_tokens":value["usage"]["input_tokens"], "completion_tokens":value["usage"]["output_tokens"]}
        }),
        allowed_tools,
    )?;
    if parsed.message["tool_calls"].is_array() {
        // This is counted in the existing protocol input budget. It is not
        // persisted as chat text, used as an instruction, or shown to the user.
        parsed.message[REPLAY] = Value::Array(items.clone());
    }
    Ok(parsed)
}

pub(super) fn metadata(value: &Value) -> TruncationMetadata {
    TruncationMetadata {
        prompt_tokens: value["usage"]["input_tokens"].as_u64(),
        completion_tokens: value["usage"]["output_tokens"].as_u64(),
        total_tokens: value["usage"]["total_tokens"].as_u64(),
        reasoning_tokens: value["usage"]["output_tokens_details"]["reasoning_tokens"].as_u64(),
        tool_call_count: value["output"].as_array().map_or(0, |items| {
            items
                .iter()
                .filter(|item| item["type"] == "function_call")
                .count()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> HashSet<String> {
        HashSet::from(["clock_now".into()])
    }
    fn function() -> Value {
        json!({"type":"function_call","id":"fc_1","call_id":"call_1",
            "name":"clock_now","arguments":"{}","status":"completed"})
    }
    fn text_item(text: &str) -> Value {
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]})
    }
    fn response(output: Vec<Value>) -> Value {
        json!({"model":"test","status":"completed","output":output,
            "usage":{"input_tokens":17,"output_tokens":23,"total_tokens":40,"output_tokens_details":{"reasoning_tokens":11}}})
    }
    fn chat(messages: Vec<Value>) -> Value {
        json!({"model":"test","messages":messages,"max_tokens":1000,"stream":false})
    }

    #[test]
    fn maps_multimodal_messages_and_optional_tool_schemas() {
        let mut original = chat(vec![
            json!({"role":"system","content":"system prompt"}),
            json!({"role":"assistant","content":"earlier answer"}),
            json!({"role":"user","content":[{"type":"text","text":"describe"},
                {"type":"image_url","image_url":{"url":"data:image/jpeg;base64,synthetic","detail":"auto"}}]}),
        ]);
        original["tools"] = json!([{"type":"function","function":{"name":"clock_now","description":"Read time",
            "parameters":{"type":"object","properties":{"zone":{"type":"string"}},"required":[]}}}]);
        original["tool_choice"] = json!("auto");
        let body = request(original).unwrap();
        assert_eq!(
            body["input"][0],
            json!({"role":"system","content":"system prompt"})
        );
        assert_eq!(body["input"][1]["content"], "earlier answer");
        assert_eq!(body["input"][2]["content"][0]["type"], "input_text");
        assert_eq!(
            body["input"][2]["content"][1],
            json!({"type":"input_image","image_url":"data:image/jpeg;base64,synthetic","detail":"auto"})
        );
        assert_eq!(body["tools"][0]["name"], "clock_now");
        assert_eq!(body["tools"][0]["strict"], false);
        assert_eq!(body["tools"][0]["parameters"]["required"], json!([]));
        assert_eq!(body["max_output_tokens"], 1000);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        for key in [
            "messages",
            "max_tokens",
            "previous_response_id",
            "conversation",
        ] {
            assert!(body.get(key).is_none());
        }
    }

    #[test]
    fn replays_reasoning_and_all_output_items_with_matching_call_ids() {
        let output = vec![
            json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque"}),
            text_item("checking"),
            function(),
        ];
        let parsed = parse(response(output.clone()), &allowed()).unwrap();
        assert_eq!(parsed.message["tool_calls"][0]["id"], "call_1");
        assert_eq!(parsed.usage.input_tokens, 17);
        assert_eq!(parsed.usage.output_tokens, 23);
        let mut original = chat(vec![
            json!({"role":"user","content":"time?"}),
            parsed.message.clone(),
            json!({"role":"tool","tool_call_id":"call_1","content":"result"}),
        ]);
        original["tool_choice"] = json!("none");
        validate_messages(original["messages"].as_array().unwrap()).unwrap();
        let body = request(original).unwrap();
        assert_eq!(body["input"].as_array().unwrap()[1..4], output);
        assert_eq!(
            body["input"][4],
            json!({"type":"function_call_output","call_id":"call_1","output":"result"})
        );
        assert_eq!(body["tool_choice"], "none");
        assert!(body.get("tools").is_none());
        let mut changed = parsed.message;
        changed["tool_calls"][0]["id"] = json!("other");
        assert!(request(chat(vec![changed])).is_err());
    }

    #[test]
    fn final_text_combines_output_parts_without_persisting_reasoning() {
        let value = response(vec![
            json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"private"}),
            text_item("first"),
            text_item("second"),
        ]);
        let stats = metadata(&value);
        assert_eq!(stats.reasoning_tokens, Some(11));
        assert_eq!(stats.total_tokens, Some(40));
        let parsed = parse(value, &allowed()).unwrap();
        assert_eq!(
            parsed.message,
            json!({"role":"assistant","content":"first\nsecond"})
        );
        assert_eq!(parsed.model, "test");
    }

    #[test]
    fn rejects_unoffered_and_malformed_calls_before_dispatch() {
        assert!(
            parse(response(vec![function()]), &HashSet::new())
                .unwrap_err()
                .is::<UnofferedTool>()
        );
        for (field, value) in [
            ("call_id", json!("")),
            ("call_id", json!("bad\n")),
            ("call_id", json!("x".repeat(129))),
            ("arguments", json!("{")),
            ("arguments", json!("[]")),
            ("arguments", json!({})),
            ("name", json!("bad name")),
            ("status", json!("in_progress")),
        ] {
            let mut call = function();
            call[field] = value;
            assert!(parse(response(vec![call]), &allowed()).is_err(), "{field}");
        }
        let mut duplicate = function();
        duplicate["id"] = json!("fc_2");
        assert!(parse(response(vec![function(), duplicate]), &allowed()).is_err());
        let mut duplicate_item = function();
        duplicate_item["call_id"] = json!("call_2");
        assert!(parse(response(vec![function(), duplicate_item]), &allowed()).is_err());
        assert!(
            parse(
                response(vec![json!({"type":"web_search_call"})]),
                &allowed()
            )
            .is_err()
        );
        let mut wrong_role = text_item("hidden");
        wrong_role["role"] = json!("system");
        assert!(parse(response(vec![wrong_role]), &allowed()).is_err());
        assert!(parse(response(vec![]), &allowed()).is_err());
    }

    #[test]
    fn refuses_entire_mixed_or_truncated_batch_without_exposing_payload() {
        let mut broken = function();
        broken["arguments"] = json!("{SENSITIVE");
        let refusal = json!({"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"SENSITIVE"}]});
        let error = parse(response(vec![broken.clone(), refusal]), &allowed()).unwrap_err();
        assert!(error.is::<ProviderBlocked>());
        assert!(!format!("{error:#}").contains("SENSITIVE"));
        let mut truncated = response(vec![broken]);
        truncated["status"] = json!("incomplete");
        truncated["incomplete_details"] = json!({"reason":"max_output_tokens"});
        assert!(
            parse(truncated, &allowed())
                .unwrap_err()
                .is::<TruncatedToolResponse>()
        );
        for status in ["failed", "incomplete", "queued", "in_progress", "cancelled"] {
            let mut unfinished = response(vec![function()]);
            unfinished["status"] = json!(status);
            assert!(parse(unfinished, &allowed()).is_err());
        }
    }

    #[test]
    fn old_config_defaults_to_chat_and_selected_endpoint_is_required() {
        let old: LlmConfig = serde_json::from_value(
            json!({"chat_completions_url":"http://localhost/v1/chat/completions","model":"test"}),
        )
        .unwrap();
        assert_eq!(old.protocol, ApiProtocol::ChatCompletions);
        assert_eq!(old.responses_url, "");
        let mut cfg = old.clone();
        cfg.protocol = ApiProtocol::Responses;
        assert!(Llm::new(cfg.clone(), reqwest::Client::new()).is_err());
        cfg.responses_url = "http://localhost/v1/responses".into();
        assert_eq!(cfg.endpoint_url(), "http://localhost/v1/responses");
        assert!(Llm::new(cfg.clone(), reqwest::Client::new()).is_ok());
        assert_eq!(serde_json::to_value(cfg).unwrap()["protocol"], "responses");
        assert!(serde_json::from_value::<LlmConfig>(json!({"protocol":"automatic"})).is_err());
    }

    #[tokio::test]
    async fn responses_transport_preserves_http_failure_and_body_limits() {
        for (status, payload, max_bytes) in [
            ("404 Not Found", "SENSITIVE".into(), 1024),
            ("200 OK", "x".repeat(1025), 1024),
            ("200 OK", "not JSON".into(), 1024),
        ] {
            let (url, server) = crate::milky::tests::server(status, &payload).await;
            let cfg = LlmConfig {
                protocol: ApiProtocol::Responses,
                responses_url: url,
                chat_completions_url: "http://127.0.0.1:1/must-not-fallback".into(),
                model: "test".into(),
                max_response_bytes: max_bytes,
                ..Default::default()
            };
            let llm = Llm::new(cfg, reqwest::Client::new()).unwrap();
            let error = llm
                .complete(
                    vec![json!({"role":"user","content":"test"})],
                    vec![],
                    100,
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert!(!format!("{error:#}").contains("SENSITIVE"));
            server.await.unwrap();
        }
    }
}
