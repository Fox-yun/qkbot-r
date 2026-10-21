//! Authenticated management of saved stickers, available while the bot is stopped.
use super::{AdminState, auth::TrustedSession};
use crate::{
    config::Config,
    embedding::Embedding,
    plugins::ai::stickers::admin::{AdminActor, AdminError, AssetKind, IndexTarget, StickerAdmin},
};
use anyhow::Result;
use axum::{
    Extension, Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GroupsQuery {
    page: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ItemsQuery {
    scope: String,
    page: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ImageQuery {
    scope: String,
    revision: Option<i64>,
    variant: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeleteRequest {
    request_id: String,
    scope: String,
    id: i64,
    revision: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReindexRequest {
    request_id: String,
    scope: String,
    id: Option<i64>,
    revision: Option<i64>,
}

fn invalid() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":"图库请求参数无效","code":"sticker_invalid"})),
    )
        .into_response()
}

fn failure(error: anyhow::Error) -> Response {
    let (status, code, message) = match error.downcast_ref::<AdminError>() {
        Some(AdminError::Invalid(_)) => (
            StatusCode::BAD_REQUEST,
            "sticker_invalid",
            "图库请求参数无效",
        ),
        Some(AdminError::NotFound(_)) => (
            StatusCode::NOT_FOUND,
            "sticker_not_found",
            "该群的图片不存在或已删除",
        ),
        Some(AdminError::Conflict(_)) => (
            StatusCode::CONFLICT,
            "sticker_conflict",
            "图库已变化或请求编号已使用，请刷新后重试",
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "sticker_unavailable",
            "图库暂不可用，请稍后重试",
        ),
    };
    // Underlying errors can contain local filenames or database details.
    (status, Json(json!({"error":message,"code":code}))).into_response()
}

fn result(result: Result<Value>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => failure(error),
    }
}

async fn service(state: &AdminState, config: &Config) -> Result<StickerAdmin> {
    // Computing the namespace never contacts the provider or requires a secret.
    // The current applied config defines the target even while indexing is off.
    let mut embedding_config = config.embedding.clone();
    embedding_config.enabled = false;
    let embedding = Embedding::new(embedding_config)?;
    let target = IndexTarget {
        fingerprint: embedding.fingerprint().to_owned(),
        model: config.embedding.model.clone(),
        dimensions: embedding.dimensions(),
        enabled: config.embedding.enabled && config.plugins.ai.enabled && config.stickers.enabled,
    };
    let database = std::path::absolute(&config.app.database)?;
    let directory = database
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("stickers");
    StickerAdmin::new(state.runtime.data().await?, directory, Some(target))
}

pub(super) async fn groups(
    State(state): State<Arc<AdminState>>,
    query: std::result::Result<Query<GroupsQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return invalid();
    };
    result(
        async {
            let config = state.runtime.config().await;
            service(&state, &config)
                .await?
                .groups(query.page.unwrap_or(1))
                .await
        }
        .await,
    )
}

pub(super) async fn items(
    State(state): State<Arc<AdminState>>,
    query: std::result::Result<Query<ItemsQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return invalid();
    };
    result(
        async {
            let config = state.runtime.config().await;
            let mut value = service(&state, &config)
                .await?
                .items(&query.scope, query.page.unwrap_or(1))
                .await?;
            value["running"] = json!(state.runtime.status().await.phase == "running");
            Ok(value)
        }
        .await,
    )
}

pub(super) async fn image(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<i64>,
    query: std::result::Result<Query<ImageQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return invalid();
    };
    let variant = match query.variant.as_deref().unwrap_or("thumbnail") {
        "thumbnail" => AssetKind::Preview,
        "original" => AssetKind::Original,
        _ => return invalid(),
    };
    let read = async {
        let config = state.runtime.config().await;
        service(&state, &config)
            .await?
            .image(&query.scope, id, query.revision, variant)
            .await
    }
    .await;
    match read {
        Ok(image)
            if matches!(
                image.mime.as_str(),
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            ) && !image.bytes.is_empty()
                && image.bytes.len() <= 8 * 1024 * 1024 =>
        {
            (
                [
                    (header::CONTENT_TYPE, image.mime),
                    (header::CACHE_CONTROL, "no-store".into()),
                ],
                image.bytes,
            )
                .into_response()
        }
        Ok(_) => failure(anyhow::anyhow!("invalid saved sticker image")),
        Err(error) => failure(error),
    }
}

fn actor(session: TrustedSession) -> AdminActor {
    AdminActor {
        admin_id: session.admin_id,
        session_id: session.session_id,
    }
}

pub(super) async fn delete(
    State(state): State<Arc<AdminState>>,
    Extension(session): Extension<TrustedSession>,
    body: std::result::Result<Json<DeleteRequest>, JsonRejection>,
) -> Response {
    if let Err(error) = state.auth.require_recent(&session) {
        return super::auth_error(error);
    }
    let Ok(Json(body)) = body else {
        return invalid();
    };
    result(
        state
            .mutations
            .clone()
            .spawn(async move {
                let config = state.runtime.config_read().await;
                service(&state, &config)
                    .await?
                    .delete(
                        &actor(session),
                        &body.request_id,
                        &body.scope,
                        body.id,
                        body.revision,
                    )
                    .await
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("sticker mutation interrupted"))),
    )
}

pub(super) async fn reindex(
    State(state): State<Arc<AdminState>>,
    Extension(session): Extension<TrustedSession>,
    body: std::result::Result<Json<ReindexRequest>, JsonRejection>,
) -> Response {
    if let Err(error) = state.auth.require_recent(&session) {
        return super::auth_error(error);
    }
    let Ok(Json(body)) = body else {
        return invalid();
    };
    result(
        state
            .mutations
            .clone()
            .spawn(async move {
                let config = state.runtime.config_read().await;
                service(&state, &config)
                    .await?
                    .reindex(
                        &actor(session),
                        &body.request_id,
                        &body.scope,
                        body.id,
                        body.revision,
                    )
                    .await
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("sticker mutation interrupted"))),
    )
}
