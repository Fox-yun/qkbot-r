use crate::{
    host::Host,
    plugin::{Plugin, PluginSpec},
    search::{SearchConfig, SearchError, SearchRequest, SearchService},
    tools::{Effect, ExternalQuota, ExternalTool, Tool, ToolRegistry, ToolResult, ToolSpec},
    types::{Incoming, Scope},
};
use anyhow::{Result, bail};
use async_trait::async_trait;
use chrono_tz::Tz;
use rusqlite::Transaction;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct WebSearchPlugin;
#[async_trait]
impl Plugin for WebSearchPlugin {
    fn spec(&self) -> PluginSpec {
        PluginSpec {
            id: "web_search",
            commands: &["/搜索", "/search"],
            tools: &["web_search"],
            dependencies: &["host"],
        }
    }
    async fn command(&self, host: &Host, message: &Incoming) -> Result<String> {
        let query = message
            .text
            .trim()
            .split_once(char::is_whitespace)
            .map(|(_, query)| query.trim())
            .unwrap_or("");
        if query.is_empty() {
            return Ok("用法：/搜索 关键词（最多300字）；将向搜索服务发送关键词。".into());
        }
        match host
            .gateway
            .direct_message(message, "web_search", json!({"query":query}))
            .await?
        {
            crate::tools::ToolOutcome::Completed { receipt, .. } => Ok(receipt),
            crate::tools::ToolOutcome::Pending { .. } => bail!("搜索不支持确认操作"),
        }
    }
}

pub fn register(registry: &mut ToolRegistry, config: &SearchConfig) -> Result<()> {
    registry.register(Arc::new(WebSearchTool {
        config: config.clone(),
        service: SearchService::new(config.clone()),
    }))
}
pub(crate) struct WebSearchTool {
    pub(crate) config: SearchConfig,
    pub(crate) service: SearchService,
}
impl Tool for WebSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search",
            plugin: "web_search",
            version: 1,
            description: "联网检索必要关键词，返回不可信摘要和来源。明确联网或时效问题优先使用；只传必要查询，不传群聊、记忆、用户身份或密钥。用[source_id]引用，失败不声称核实。",
            parameters: json!({"type":"object","properties":{
                "query":{"type":"string","minLength":1,"maxLength":300},
                "time_range":{"type":"string","enum":["any","day","week","month","year"]},
                "max_results":{"type":"integer","minimum":1,"maximum":5}},"required":["query"],"additionalProperties":false}),
            effect: Effect::NetworkRead,
            max_output_bytes: self.config.max_output_bytes,
        }
    }
    fn external(&self) -> Option<&dyn ExternalTool> {
        Some(self)
    }
    fn permits(&self, _: &Scope, _: i64) -> bool {
        self.config.enabled && !self.config.api_key.trim().is_empty()
    }
    fn prepare(&self, args: Value, _: &Scope, _: i64, _: i64, _: Tz) -> Result<Value> {
        let request: SearchRequest = serde_json::from_value(args)
            .map_err(|_| anyhow::anyhow!("搜索参数无效或包含未知字段"))?;
        Ok(serde_json::to_value(request.normalize(&self.config)?)?)
    }
    fn preview(&self, _: &Value, _: Tz) -> Result<String> {
        Ok("联网搜索".into())
    }
    fn apply(
        &self,
        _: &Transaction<'_>,
        _: &Value,
        _: &Scope,
        _: i64,
        _: &str,
        _: i64,
    ) -> Result<ToolResult> {
        bail!("联网搜索必须在事务外执行")
    }
}
#[async_trait]
impl ExternalTool for WebSearchTool {
    fn quota(&self) -> ExternalQuota {
        ExternalQuota {
            per_turn: self.config.max_calls_per_turn,
            per_user: self.config.user_per_hour,
            per_scope: self.config.conversation_per_hour,
            global: self.config.global_per_hour,
        }
    }
    async fn execute(
        &self,
        args: &Value,
        operation: &str,
        cancel: &CancellationToken,
    ) -> Result<ToolResult> {
        let request = serde_json::from_value(args.clone())?;
        match self.service.search(request, operation, cancel).await {
            Ok(response) => Ok(ToolResult {
                receipt: response.receipt(),
                value: serde_json::to_value(response)?,
            }),
            Err(error) => {
                let error = error
                    .downcast_ref::<SearchError>()
                    .copied()
                    .unwrap_or(SearchError::Unavailable);
                Ok(ToolResult {
                    value: json!({"status":"failed","code":error.code(),"error":error.to_string()}),
                    receipt: error.to_string(),
                })
            }
        }
    }
}
