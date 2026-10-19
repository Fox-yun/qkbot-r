use crate::{
    host::Host,
    plugin::{Plugin, PluginSpec},
    tools::{Effect, Tool, ToolRegistry, ToolResult, ToolSpec, object},
    types::{Incoming, Scope},
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::TimeZone;
use chrono_tz::Tz;
use rusqlite::Transaction;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct ClockPlugin;
#[async_trait]
impl Plugin for ClockPlugin {
    fn spec(&self) -> PluginSpec {
        PluginSpec {
            id: "clock",
            commands: &["/clock"],
            tools: &["clock_now"],
            dependencies: &["host"],
        }
    }
    async fn command(&self, host: &Host, message: &Incoming) -> Result<String> {
        let parts: Vec<_> = message.text.split_whitespace().collect();
        if !matches!(parts.as_slice(), ["/clock"] | ["/clock", "now"]) {
            return Ok("/clock 或 /clock now：查询当前时间".into());
        }
        match host
            .gateway
            .direct(
                message.key.scope.clone(),
                message.sender_id,
                "clock_now",
                json!({}),
                message.received_at,
            )
            .await?
        {
            crate::tools::ToolOutcome::Completed { receipt, .. } => Ok(receipt),
            crate::tools::ToolOutcome::Pending { preview, .. } => Ok(preview),
        }
    }
}

pub fn register(registry: &mut ToolRegistry) -> Result<()> {
    registry.register(Arc::new(Clock))
}
struct Clock;
impl Tool for Clock {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "clock_now",
            plugin: "clock",
            version: 1,
            description: "Get the current time in the bot's configured timezone.",
            parameters: json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
            effect: Effect::Read,
            max_output_bytes: 1024,
        }
    }
    fn prepare(&self, args: Value, _: &Scope, _: i64, _: i64, timezone: Tz) -> Result<Value> {
        object(&args, &[])?;
        Ok(json!({"timezone":timezone.name()}))
    }
    fn preview(&self, _: &Value, _: Tz) -> Result<String> {
        Ok("查询当前时间".into())
    }
    fn apply(
        &self,
        _: &Transaction<'_>,
        args: &Value,
        _: &Scope,
        _: i64,
        _: &str,
        now: i64,
    ) -> Result<ToolResult> {
        let tz: Tz = args["timezone"].as_str().unwrap_or("UTC").parse()?;
        let datetime = tz
            .timestamp_opt(now, 0)
            .single()
            .ok_or_else(|| anyhow::anyhow!("invalid time"))?
            .to_rfc3339();
        Ok(ToolResult {
            value: json!({"time":datetime,"timezone":tz.name()}),
            receipt: datetime,
        })
    }
}
