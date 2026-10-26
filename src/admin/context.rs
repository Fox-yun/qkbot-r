//! Read-only, account-scoped view of the applied context policy and eligible turns.
use super::AdminState;
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::sync::Arc;

pub(super) async fn overview(State(state): State<Arc<AdminState>>) -> Response {
    let result: anyhow::Result<serde_json::Value> = async {
        let config = state.runtime.config_read().await;
        let status = state.runtime.status().await;
        let bot = if status.online { status.bot_id } else { config.milky.bot_id }.filter(|id| *id > 0);
        let mut result = json!({"bot_id":bot.map(|id| id.to_string()),"enabled":config.plugins.ai.context.enabled,
            "config":config.plugins.ai.context,"compaction_config":config.plugins.ai.compaction,"compaction":[],"counts":{"turns":0,"memories":0,"notes":0},"recent":[]});
        let Some(bot) = bot else {
            result["message"] = json!("连接机器人后显示当前账号的问答来源。参数可先在配置中编辑并保存应用。");
            return Ok(result);
        };
        let policy = config.plugins.ai.context.clone();
        let data = state.runtime.data().await?;
        let snapshot = data.call(move |db| {
            let mut snapshot = crate::plugins::ai::context::continuity::overview(db, bot, 20, &policy)?;
            let memories: i64 = db.query_row("SELECT count(*) FROM ai_memories m JOIN ai_conversations c ON c.id=m.conversation_id WHERE c.scope LIKE ?1 AND m.active=1 AND (m.expires_at IS NULL OR m.expires_at>?2)",
                rusqlite::params![format!("{bot}:%"),crate::types::now()], |r| r.get(0))?;
            let notes: i64 = db.query_row("SELECT count(*) FROM ai_learning_notes WHERE bot_id=?1 AND deleted=0 AND invalidated=0", [bot], |r| r.get(0))?;
            snapshot["counts"]["memories"] = json!(memories);
            snapshot["counts"]["notes"] = json!(notes);
            snapshot["learning_batches"] = crate::plugins::ai::memory::lifecycle::overview(db, bot)?;
            snapshot["compaction"] = crate::plugins::ai::context::compaction::overview(db, bot)?;
            Ok(snapshot)
        }).await?;
        let current = state.runtime.status().await;
        let current_bot = if current.online { current.bot_id } else { config.milky.bot_id };
        anyhow::ensure!(current_bot == Some(bot), "机器人账号已变化");
        result["counts"] = snapshot["counts"].clone();
        result["recent"] = snapshot["recent"].clone();
        result["learning_batches"] = snapshot["learning_batches"].clone();
        result["compaction"] = snapshot["compaction"].clone();
        Ok(result)
    }.await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"上下文数据暂不可用，请稍后刷新"})),
        )
            .into_response(),
    }
}
