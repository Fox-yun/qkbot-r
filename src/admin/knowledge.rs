//! Authenticated, current-account management of unified knowledge: shared
//! episodes, the bot's own statements, impressions and interaction notes,
//! plus reply-outcome statistics.
use super::{AdminState, auth::TrustedSession};
use crate::plugins::ai::knowledge;
use anyhow::{Result, ensure};
use axum::{
    Extension, Json,
    extract::{
        Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct KnowledgeQuery {
    bot_id: Option<String>,
    kind: Option<String>,
    status: Option<String>,
    page: Option<usize>,
}

async fn account(
    state: &AdminState,
    config: &crate::config::Config,
    expected: Option<&str>,
) -> Result<i64> {
    let status = state.runtime.status().await;
    let bot = if status.online {
        status.bot_id
    } else {
        config.milky.bot_id
    }
    .filter(|id| *id > 0)
    .ok_or_else(|| anyhow::anyhow!("knowledge account unavailable"))?;
    ensure!(
        expected.is_none_or(|s| s == bot.to_string()),
        "knowledge account changed"
    );
    Ok(bot)
}

fn response(result: Result<Value>) -> Response {
    match result {
        Ok(v) => Json(v).into_response(),
        Err(_) => (
            StatusCode::CONFLICT,
            Json(json!({"error":"账号、条目版本或来源已变化，请刷新后核对。","code":"knowledge_conflict"})),
        )
            .into_response(),
    }
}

pub(super) async fn snapshot(
    State(state): State<Arc<AdminState>>,
    query: Result<Query<KnowledgeQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return super::error(StatusCode::BAD_REQUEST, "无效的知识查询");
    };
    response(
        async {
            let config = state.runtime.config_read().await;
            let bot = account(&state, &config, query.bot_id.as_deref()).await?;
            let page = query.page.unwrap_or(1);
            ensure!((1..=100_000).contains(&page), "invalid page");
            let filter = knowledge::Filter {
                kind: query.kind,
                status: query.status,
                page,
            };
            let mut value = state
                .runtime
                .data()
                .await?
                .call(move |db| knowledge::snapshot(db, bot, &filter))
                .await?;
            account(&state, &config, Some(&bot.to_string())).await?;
            value["enabled"] = json!(config.plugins.ai.knowledge.enabled);
            Ok(value)
        }
        .await,
    )
}

pub(super) async fn mutate(
    State(state): State<Arc<AdminState>>,
    Extension(session): Extension<TrustedSession>,
    body: Result<Json<knowledge::Mutation>, JsonRejection>,
) -> Response {
    if let Err(e) = state.auth.require_recent(&session) {
        return super::auth_error(e);
    }
    let Ok(Json(body)) = body else {
        return super::error(StatusCode::BAD_REQUEST, "无效的知识修改");
    };
    response(
        state
            .mutations
            .clone()
            .spawn(async move {
                let config = state.runtime.config_read().await;
                let bot = account(&state, &config, Some(&body.bot_id)).await?;
                let result = state
                    .runtime
                    .data()
                    .await?
                    .call(move |db| {
                        knowledge::mutate(db, bot, &body, &session.admin_id, &session.session_id)
                    })
                    .await?;
                account(&state, &config, Some(&bot.to_string())).await?;
                Ok(result)
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("knowledge mutation interrupted"))),
    )
}
