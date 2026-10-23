//! Administrator-only management of the current account's shared learning notes.
use super::{AdminState, auth::TrustedSession};
use crate::{
    config::Config,
    plugins::ai::learning::{self, LearningError},
};
use anyhow::{Result, ensure};
use axum::{
    Extension, Json,
    extract::{
        Path, Query, State,
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
pub(super) struct AccountQuery {
    bot_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Mutation {
    bot_id: String,
    request_id: String,
    revision: i64,
    action: Action,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Add {
        text: String,
        kind: String,
        scope: String,
        locked: bool,
    },
    Update {
        id: i64,
        version: i64,
        text: String,
        kind: String,
        scope: String,
        locked: bool,
    },
    Lock {
        id: i64,
        version: i64,
        locked: bool,
    },
    Delete {
        id: i64,
        version: i64,
    },
    Rollback {
        id: i64,
        version: i64,
        history_id: i64,
    },
}

fn invalid(message: &str) -> anyhow::Error {
    LearningError::Invalid(message.to_owned()).into()
}
#[derive(Debug)]
struct AccountUnavailable;
impl std::fmt::Display for AccountUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("请先连接机器人，或在已应用配置中指定机器人 QQ 号")
    }
}
impl std::error::Error for AccountUnavailable {}
fn failure(error: anyhow::Error) -> Response {
    if error.downcast_ref::<AccountUnavailable>().is_some() {
        return (StatusCode::CONFLICT, Json(json!({"error":"请先连接机器人，或在配置中指定机器人 QQ 号并保存应用，再刷新学习笔记。","code":"learning_account_unavailable"}))).into_response();
    }
    let (status, code, message) = match error.downcast_ref::<LearningError>() {
        Some(LearningError::Invalid(_)) => (
            StatusCode::BAD_REQUEST,
            "learning_invalid",
            "学习笔记请求无效，请检查内容长度、类型和范围",
        ),
        Some(LearningError::Conflict(_)) => (
            StatusCode::CONFLICT,
            "learning_conflict",
            "账号或笔记版本已变化，请刷新后核对；失效来源不可恢复",
        ),
        Some(LearningError::NotFound(_)) => (
            StatusCode::NOT_FOUND,
            "learning_not_found",
            "学习笔记或操作记录不存在",
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "learning_unavailable",
            "学习笔记暂不可用，请稍后刷新",
        ),
    };
    (status, Json(json!({"error":message,"code":code}))).into_response()
}
fn response(result: Result<Value>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => failure(error),
    }
}

async fn account(state: &AdminState, config: &Config, requested: Option<&str>) -> Result<i64> {
    let status = state.runtime.status().await;
    let bot_id = if status.online {
        status.bot_id
    } else {
        config.milky.bot_id
    }
    .filter(|id| *id > 0)
    .ok_or(AccountUnavailable)?;
    if requested.is_some_and(|value| value != bot_id.to_string()) {
        return Err(LearningError::Conflict("当前机器人账号已变化".into()).into());
    }
    Ok(bot_id)
}

fn normalize_account(mut value: Value, bot_id: i64) -> Value {
    value["bot_id"] = json!(bot_id.to_string());
    value
}

pub(super) async fn snapshot(
    State(state): State<Arc<AdminState>>,
    query: std::result::Result<Query<AccountQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return failure(invalid("无效查询"));
    };
    response(
        async {
            let config = state.runtime.config_read().await;
            let bot_id = account(&state, &config, query.bot_id.as_deref()).await?;
            let mut value = normalize_account(
                learning::snapshot_with_defaults(
                    &state.runtime.data().await?,
                    bot_id,
                    config.plugins.ai.defaults.clone(),
                )
                .await?,
                bot_id,
            );
            // Recheck after the database read so an account change cannot paint another account's notes.
            account(&state, &config, Some(&bot_id.to_string())).await?;
            value["max_note_chars"] = json!(config.plugins.ai.learning.max_note_chars);
            value["learning_enabled"] =
                json!(config.plugins.ai.enabled && config.plugins.ai.learning.enabled);
            Ok(value)
        }
        .await,
    )
}

fn validate_action(action: &mut Action, bot_id: i64, max_chars: usize) -> Result<()> {
    let (id, version) = match action {
        Action::Add { .. } => (None, None),
        Action::Update { id, version, .. }
        | Action::Lock { id, version, .. }
        | Action::Delete { id, version } => (Some(*id), Some(*version)),
        Action::Rollback {
            id,
            version,
            history_id,
        } => {
            if *history_id <= 0 {
                return Err(invalid("无效历史记录"));
            }
            (Some(*id), Some(*version))
        }
    };
    if id.is_some_and(|id| id <= 0) || version.is_some_and(|version| version <= 0) {
        return Err(invalid("无效记录版本"));
    }
    if let Action::Add {
        text, kind, scope, ..
    }
    | Action::Update {
        text, kind, scope, ..
    } = action
    {
        *text = text.trim().to_owned();
        if text.is_empty()
            || text.chars().count() > max_chars
            || !matches!(kind.as_str(), "experience" | "correction" | "term")
        {
            return Err(invalid("无效笔记内容"));
        }
        let prefix = format!("{bot_id}:group:");
        let scoped = scope.strip_prefix(&prefix).is_some_and(|peer| {
            peer.parse::<i64>()
                .is_ok_and(|id| id > 0 && id.to_string() == peer)
        });
        if scope.as_str() != "shared" && !scoped {
            return Err(invalid("笔记不属于当前账号群聊"));
        }
    }
    Ok(())
}

pub(super) async fn mutate(
    State(state): State<Arc<AdminState>>,
    Extension(session): Extension<TrustedSession>,
    body: std::result::Result<Json<Mutation>, JsonRejection>,
) -> Response {
    if let Err(error) = state.auth.require_recent(&session) {
        return super::auth_error(error);
    }
    let Ok(Json(mut body)) = body else {
        return failure(invalid("无效操作"));
    };
    response(
        state
            .mutations
            .clone()
            .spawn(async move {
                let config = state.runtime.config_read().await;
                let bot_id = account(&state, &config, Some(&body.bot_id)).await?;
                ensure!(
                    body.revision >= 0
                        && !body.request_id.is_empty()
                        && body.request_id.len() <= 128,
                    invalid("无效请求编号或版本")
                );
                validate_action(
                    &mut body.action,
                    bot_id,
                    config.plugins.ai.learning.max_note_chars,
                )?;
                let mut request = serde_json::to_value(body.action)?;
                request["revision"] = json!(body.revision);
                request["request_id"] = json!(body.request_id);
                request["admin_id"] = json!(session.admin_id);
                request["session_id"] = json!(session.session_id);
                request["_max_note_chars"] = json!(config.plugins.ai.learning.max_note_chars);
                request["_defaults"] = serde_json::to_value(&config.plugins.ai.defaults)?;
                let value = learning::mutate(&state.runtime.data().await?, bot_id, request).await?;
                Ok(normalize_account(value, bot_id))
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("learning mutation interrupted"))),
    )
}

pub(super) async fn operation(
    State(state): State<Arc<AdminState>>,
    Extension(session): Extension<TrustedSession>,
    Path(id): Path<String>,
    query: std::result::Result<Query<AccountQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return failure(invalid("无效查询"));
    };
    response(
        async {
            let config = state.runtime.config_read().await;
            let bot_id = account(&state, &config, query.bot_id.as_deref()).await?;
            let result = learning::operation(
                &state.runtime.data().await?,
                bot_id,
                id,
                session.admin_id,
                session.session_id,
            )
            .await?;
            Ok(result
                .map(|value| normalize_account(value, bot_id))
                .unwrap_or_else(|| json!({"status":"not_found"})))
        }
        .await,
    )
}
