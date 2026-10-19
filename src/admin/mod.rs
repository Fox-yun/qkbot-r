//! Embedded, authenticated administration UI. No browser-side provider credentials.
pub mod qq;
pub mod runtime;
pub mod settings;

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State, rejection::JsonRejection},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Semaphore},
};
use tokio_util::sync::CancellationToken;

use self::{
    qq::QqClient,
    runtime::Runtime,
    settings::{ConfigStore, SaveRequest},
};

pub struct AdminState {
    store: Arc<ConfigStore>,
    runtime: Arc<Runtime>,
    token_hash: [u8; 32],
    requests: Arc<Semaphore>,
    qq: Mutex<Option<(Vec<u8>, QqClient)>>,
}

impl AdminState {
    pub fn new(store: Arc<ConfigStore>, runtime: Arc<Runtime>, token: &str) -> Result<Arc<Self>> {
        ensure!(token.len() >= 32, "管理员令牌至少需要 32 字节");
        HeaderValue::from_str(&format!("Bearer {token}")).context("管理员令牌包含无效字符")?;
        Ok(Arc::new(Self {
            store,
            runtime,
            token_hash: Sha256::digest(token.as_bytes()).into(),
            requests: Arc::new(Semaphore::new(16)),
            qq: Mutex::new(None),
        }))
    }

    async fn qq_client(&self) -> Result<QqClient> {
        let config = self.store.effective().await?.qq_login;
        let fingerprint = Sha256::digest(serde_json::to_vec(&config)?).to_vec();
        let mut cached = self.qq.lock().await;
        if let Some((prior, client)) = cached.as_ref()
            && prior == &fingerprint
        {
            return Ok(client.clone());
        }
        let client = QqClient::new(config)?;
        *cached = Some((fingerprint, client.clone()));
        Ok(client)
    }
}

pub fn router(state: Arc<AdminState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(script))
        .route("/style.css", get(style))
        .route("/api/status", get(status))
        .route("/api/config", get(config).put(save))
        .route("/api/runtime/apply", post(apply))
        .route("/api/runtime/stop", post(stop))
        .route("/api/qq/status", get(qq_status))
        .route("/api/qq/qrcode", post(qq_qrcode))
        .fallback(|| async { error(StatusCode::NOT_FOUND, "页面或接口不存在") })
        // A valid 1 MiB TOML configuration can grow when encoded as JSON.
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), protect))
        .with_state(state)
}

pub async fn serve(
    listener: TcpListener,
    state: Arc<AdminState>,
    cancel: CancellationToken,
) -> Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .context("管理员页面服务失败")
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error":message.into()}))).into_response()
}

async fn protect(State(state): State<Arc<AdminState>>, request: Request, next: Next) -> Response {
    let response = async {
        let Ok(_permit) = state.requests.clone().try_acquire_owned() else {
            return error(StatusCode::SERVICE_UNAVAILABLE, "管理请求过多，请稍后重试");
        };
        if request.uri().path().starts_with("/api/") {
            let supplied = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.strip_prefix("Bearer "));
            let Some(supplied) = supplied else {
                return error(StatusCode::UNAUTHORIZED, "请先输入管理员令牌");
            };
            let digest: [u8; 32] = Sha256::digest(supplied.as_bytes()).into();
            let different = digest
                .iter()
                .zip(state.token_hash)
                .fold(0_u8, |a, (x, y)| a | (x ^ y));
            if different != 0 {
                return error(StatusCode::UNAUTHORIZED, "管理员令牌无效");
            }
            // No cookies or CORS. Also reject authenticated cross-origin browser requests.
            if request
                .headers()
                .get("sec-fetch-site")
                .is_some_and(|h| h == "cross-site")
            {
                return error(StatusCode::FORBIDDEN, "不允许跨站管理请求");
            }
            if let Some(origin) = request.headers().get(header::ORIGIN) {
                let matches = origin
                    .to_str()
                    .ok()
                    .and_then(|o| reqwest::Url::parse(o).ok())
                    .is_some_and(|url| {
                        matches!(url.scheme(), "http" | "https")
                            && url.username().is_empty()
                            && url.password().is_none()
                            && request
                                .headers()
                                .get(header::HOST)
                                .and_then(|h| h.to_str().ok())
                                .and_then(|host| {
                                    reqwest::Url::parse(&format!("{}://{host}", url.scheme())).ok()
                                })
                                .is_some_and(|host| host.origin() == url.origin())
                    });
                if !matches {
                    return error(StatusCode::FORBIDDEN, "不允许跨来源管理请求");
                }
            }
        }
        match tokio::time::timeout(Duration::from_secs(30), next.run(request)).await {
            Ok(response) => response,
            Err(_) => error(
                StatusCode::REQUEST_TIMEOUT,
                "管理请求超时，请刷新状态后重试",
            ),
        }
    }
    .await;
    secure_headers(response)
}

fn secure_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("content-security-policy", HeaderValue::from_static(
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
    response
}

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("static/index.html"),
    )
}
async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("static/app.js"),
    )
}
async fn style() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("static/style.css"),
    )
}
async fn status(State(state): State<Arc<AdminState>>) -> impl IntoResponse {
    Json(state.runtime.status().await)
}
async fn config(State(state): State<Arc<AdminState>>) -> Response {
    match state.store.snapshot().await {
        Ok(value) => Json(value).into_response(),
        Err(error_) => error(StatusCode::BAD_REQUEST, error_.to_string()),
    }
}
async fn save(
    State(state): State<Arc<AdminState>>,
    body: Result<Json<SaveRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = body else {
        return error(StatusCode::BAD_REQUEST, "配置请求无效或超出大小限制");
    };
    match state.store.save(request).await {
        Ok(value) => Json(value).into_response(),
        Err(error_)
            if error_
                .downcast_ref::<settings::RevisionConflict>()
                .is_some() =>
        {
            error(StatusCode::CONFLICT, error_.to_string())
        }
        Err(error_) => error(StatusCode::BAD_REQUEST, error_.to_string()),
    }
}
async fn apply(State(state): State<Arc<AdminState>>) -> Response {
    let (config, revision) = match state.store.effective_with_revision().await {
        Ok(value) => value,
        Err(error_) => return error(StatusCode::BAD_REQUEST, error_.to_string()),
    };
    match state.runtime.apply(config, revision) {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"accepted":true}))).into_response(),
        Err(error_) => error(StatusCode::CONFLICT, error_.to_string()),
    }
}
async fn stop(State(state): State<Arc<AdminState>>) -> Response {
    match state.runtime.stop() {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"accepted":true}))).into_response(),
        Err(error_) => error(StatusCode::CONFLICT, error_.to_string()),
    }
}
async fn qq_status(State(state): State<Arc<AdminState>>) -> Response {
    let result = async { state.qq_client().await?.status().await }.await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error_) => error(StatusCode::BAD_GATEWAY, error_.to_string()),
    }
}
async fn qq_qrcode(State(state): State<Arc<AdminState>>) -> Response {
    let result = async { state.qq_client().await?.qrcode().await }.await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error_) => error(StatusCode::BAD_GATEWAY, error_.to_string()),
    }
}
