//! Embedded administration with server sessions and a shared database service.
pub mod auth;
pub mod contacts;
pub mod embedding;
pub mod memory;
pub mod models;
pub mod plugins;
pub mod qq;
pub mod qq_service;
pub mod runtime;
pub mod settings;

use self::{
    auth::{AuthError, AuthGrant, AuthService, TrustedSession},
    contacts::ContactsService,
    memory::{Actor, MemoryAdmin},
    models::ModelsService,
    qq_service::QqService,
    runtime::Runtime,
    settings::{ConfigStore, SaveRequest},
};
use anyhow::{Context, Result};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Semaphore};
use tokio_util::sync::CancellationToken;

pub struct AdminState {
    store: Arc<ConfigStore>,
    runtime: Arc<Runtime>,
    auth: Arc<AuthService>,
    requests: Arc<Semaphore>,
    qq: QqService,
    contacts: ContactsService,
    models: ModelsService,
    embedding: embedding::EmbeddingTestService,
    mutations: tokio_util::task::TaskTracker,
}
impl AdminState {
    pub fn new(
        store: Arc<ConfigStore>,
        runtime: Arc<Runtime>,
        auth: Arc<AuthService>,
    ) -> Arc<Self> {
        Arc::new(Self {
            qq: QqService::new(store.clone()),
            contacts: ContactsService::new(store.clone(), runtime.clone()),
            models: ModelsService::new(store.clone()),
            embedding: embedding::EmbeddingTestService::new(store.clone()),
            store,
            runtime,
            auth,
            requests: Arc::new(Semaphore::new(16)),
            mutations: tokio_util::task::TaskTracker::new(),
        })
    }
    async fn memories(&self) -> Result<MemoryAdmin> {
        Ok(MemoryAdmin::new(
            self.runtime.data().await?,
            self.runtime.config().await.plugins.ai.clone(),
        ))
    }
}
pub fn router(state: Arc<AdminState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(script))
        .route("/style.css", get(style))
        .route("/api/auth/state", get(auth_state))
        .route("/api/auth/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/reauth", post(reauth))
        .route("/api/auth/password", post(password))
        .route("/api/status", get(status))
        .route("/api/config", get(config).put(save))
        .route("/api/llm/models", post(model_list))
        .route("/api/embedding/test", post(embedding_test))
        .route("/api/runtime/apply", post(apply))
        .route("/api/runtime/stop", post(stop))
        .route("/api/qq/status", get(qq_status))
        .route("/api/qq/qrcode", post(qq_qrcode))
        .route("/api/qq/contacts", get(qq_contacts))
        .route("/api/qq/contacts/toggle", post(toggle_contact))
        .route(
            "/api/plugins/scope",
            get(scope_plugins).post(toggle_scope_plugin),
        )
        .route("/api/budget", get(budget))
        .route("/api/memory/conversations", get(conversations))
        .route("/api/memory/conversations/{id}", get(conversation))
        .route("/api/memory/records", get(memories))
        .route("/api/memory/records/{id}", get(memory_detail))
        .route("/api/memory/records/{id}/source", get(memory_source))
        .route("/api/memory/mutate", post(mutate))
        .route("/api/ai/groups/persona", post(group_persona))
        .route("/api/memory/forget/preview", post(forget_preview))
        .route("/api/memory/forget/confirm", post(forget_confirm))
        .route("/api/memory/operations/{id}", get(operation))
        .fallback(|| async { error(StatusCode::NOT_FOUND, "页面或接口不存在") })
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), protect))
        .with_state(state)
}
pub async fn serve(
    listener: TcpListener,
    state: Arc<AdminState>,
    cancel: CancellationToken,
) -> Result<()> {
    let result = axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await;
    state.mutations.close();
    state.mutations.wait().await;
    result.context("管理员页面服务失败")
}
fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error":message.into()}))).into_response()
}
fn auth_error(e: AuthError) -> Response {
    (
        e.status(),
        Json(json!({"error":e.to_string(),"code":e.code()})),
    )
        .into_response()
}
fn grant(result: auth::AuthResult<AuthGrant>) -> Response {
    match result {
        Ok(grant) => ([(header::SET_COOKIE, grant.cookie)], Json(grant.view)).into_response(),
        Err(e) => auth_error(e),
    }
}
async fn protect(
    State(state): State<Arc<AdminState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let response = async {
        let Ok(_permit) = state.requests.clone().try_acquire_owned() else {
            return error(StatusCode::SERVICE_UNAVAILABLE, "管理请求过多，请稍后重试");
        };
        let path = request.uri().path().to_owned();
        if path.starts_with("/api/") {
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
                    .and_then(|v| reqwest::Url::parse(v).ok())
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
            if !matches!(
                path.as_str(),
                "/api/auth/state" | "/api/auth/setup" | "/api/auth/login"
            ) {
                let mutation = !matches!(
                    *request.method(),
                    Method::GET | Method::HEAD | Method::OPTIONS
                );
                // Passive polling must not keep an abandoned session alive indefinitely.
                let touch = mutation
                    || matches!(path.as_str(), "/api/config" | "/api/qq/contacts")
                    || (path.starts_with("/api/memory/")
                        && !path.starts_with("/api/memory/operations/"));
                let session = match state.auth.authenticate(request.headers(), touch).await {
                    Ok(s) => s,
                    Err(e) => return auth_error(e),
                };
                if mutation && let Err(e) = state.auth.check_csrf(&session, request.headers()) {
                    return auth_error(e);
                }
                request.extensions_mut().insert(session);
            }
        }
        match tokio::time::timeout(Duration::from_secs(30), next.run(request)).await {
            Ok(r) => r,
            Err(_) => error(StatusCode::REQUEST_TIMEOUT, "管理请求超时，请核对操作结果"),
        }
    }
    .await;
    secure_headers(response)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Setup {
    #[serde(alias = "setup_code")]
    code: String,
    password: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Password {
    password: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewPassword {
    new_password: String,
}
async fn auth_state(State(s): State<Arc<AdminState>>, headers: HeaderMap) -> impl IntoResponse {
    Json(s.auth.state(&headers).await)
}
async fn setup(State(s): State<Arc<AdminState>>, Json(v): Json<Setup>) -> Response {
    grant(s.auth.setup(v.code, v.password).await)
}
async fn login(State(s): State<Arc<AdminState>>, Json(v): Json<Password>) -> Response {
    grant(s.auth.login(v.password).await)
}
async fn reauth(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
    Json(v): Json<Password>,
) -> Response {
    grant(s.auth.reauthenticate(&a, v.password).await)
}
async fn password(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
    Json(v): Json<NewPassword>,
) -> Response {
    grant(s.auth.change_password(&a, v.new_password).await)
}
async fn logout(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
) -> Response {
    (
        [(header::SET_COOKIE, s.auth.logout(&a))],
        Json(json!({"authenticated":false})),
    )
        .into_response()
}
async fn status(State(s): State<Arc<AdminState>>) -> impl IntoResponse {
    Json(s.runtime.status().await)
}
async fn config(State(s): State<Arc<AdminState>>) -> Response {
    result(s.store.snapshot().await)
}
async fn save(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
    body: Result<Json<SaveRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(v)) = body else {
        return error(StatusCode::BAD_REQUEST, "配置请求无效或超出大小限制");
    };
    if let Err(e) = s.auth.require_recent(&a) {
        return auth_error(e);
    }
    match s.store.save(v).await {
        Ok(v) => Json(v).into_response(),
        Err(e) if e.downcast_ref::<settings::RevisionConflict>().is_some() => {
            error(StatusCode::CONFLICT, e.to_string())
        }
        Err(e) => error(StatusCode::BAD_REQUEST, e.to_string()),
    }
}
async fn apply(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
) -> Response {
    if let Err(e) = s.auth.require_recent(&a) {
        return auth_error(e);
    }
    let (c, r) = match s.store.effective_with_revision().await {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    match s.runtime.apply(c, r) {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"accepted":true}))).into_response(),
        Err(e) => error(StatusCode::CONFLICT, e.to_string()),
    }
}
async fn stop(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
) -> Response {
    if let Err(e) = s.auth.require_recent(&a) {
        return auth_error(e);
    }
    match s.runtime.stop() {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"accepted":true}))).into_response(),
        Err(e) => error(StatusCode::CONFLICT, e.to_string()),
    }
}
#[derive(Default, Deserialize)]
struct Force {
    #[serde(default)]
    force: bool,
}
async fn qq_status(State(s): State<Arc<AdminState>>, Query(q): Query<Force>) -> Response {
    match s.qq.status(q.force).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => qq_error(e),
    }
}
async fn qq_qrcode(State(s): State<Arc<AdminState>>) -> Response {
    match s.qq.qrcode().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => qq_error(e),
    }
}
async fn qq_contacts(State(s): State<Arc<AdminState>>, Query(q): Query<Force>) -> Response {
    result(async { Ok(serde_json::to_value(s.contacts.list(q.force).await?)?) }.await)
}
async fn toggle_contact(
    State(s): State<Arc<AdminState>>,
    Json(v): Json<contacts::ToggleRequest>,
) -> Response {
    result(
        s.mutations
            .clone()
            .spawn(async move { Ok(serde_json::to_value(s.contacts.set_enabled(v).await?)?) })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("开关操作异常，请刷新列表核对状态"))),
    )
}
async fn scope_plugins(
    State(s): State<Arc<AdminState>>,
    Query(query): Query<plugins::ScopeQuery>,
) -> Response {
    result(
        async {
            Ok(serde_json::to_value(
                plugins::list(&s.contacts, &s.runtime, query).await?,
            )?)
        }
        .await,
    )
}
async fn toggle_scope_plugin(
    State(s): State<Arc<AdminState>>,
    Json(request): Json<plugins::TogglePlugin>,
) -> Response {
    result(
        s.mutations
            .clone()
            .spawn(async move {
                Ok(serde_json::to_value(
                    plugins::toggle(&s.contacts, &s.runtime, request).await?,
                )?)
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("插件开关操作异常，请刷新核对状态"))),
    )
}
fn qq_error(e: qq::QqError) -> Response {
    (
        e.status(),
        Json(json!({"error":e.to_string(),"code":e.code(),"retry_after_ms":e.retry_after_ms()})),
    )
        .into_response()
}
fn result(r: Result<Value>) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            if let Some(plugin_error) = e.downcast_ref::<plugins::PluginsError>() {
                return (
                    plugin_error.status(),
                    Json(json!({"error":plugin_error.to_string(),"code":plugin_error.code()})),
                )
                    .into_response();
            }
            if let Some(contact_error) = e.downcast_ref::<contacts::ContactsError>() {
                return (
                    contact_error.status(),
                    Json(json!({"error":contact_error.to_string(),"code":contact_error.code()})),
                )
                    .into_response();
            }
            let status = match e.downcast_ref::<memory::AdminError>() {
                Some(memory::AdminError::Conflict(_)) => StatusCode::CONFLICT,
                Some(memory::AdminError::NotFound(_)) => StatusCode::NOT_FOUND,
                None if e.downcast_ref::<runtime::DataUnavailable>().is_some() => {
                    StatusCode::SERVICE_UNAVAILABLE
                }
                None => StatusCode::BAD_REQUEST,
            };
            error(status, e.to_string())
        }
    }
}
fn actor(s: TrustedSession) -> Actor {
    Actor {
        admin_id: s.admin_id,
        session_id: s.session_id,
    }
}
async fn budget(State(s): State<Arc<AdminState>>) -> Response {
    result(
        async {
            crate::budget::Budget::new(
                s.runtime.data().await?,
                s.runtime.config().await.limits.clone(),
            )
            .snapshot()
            .await
        }
        .await,
    )
}
async fn conversations(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
    Query(q): Query<memory::ConversationQuery>,
) -> Response {
    result(async { s.memories().await?.conversations(&actor(a), q).await }.await)
}
async fn conversation(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
    Path(id): Path<String>,
) -> Response {
    result(async { s.memories().await?.conversation(&actor(a), &id).await }.await)
}
async fn memories(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
    Query(q): Query<memory::MemoryQuery>,
) -> Response {
    result(async { s.memories().await?.memories(&actor(a), q).await }.await)
}
#[derive(Deserialize)]
struct EvidencePage {
    evidence_page: Option<usize>,
}
async fn memory_detail(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
    Path(id): Path<i64>,
    Query(q): Query<EvidencePage>,
) -> Response {
    result(
        async {
            s.memories()
                .await?
                .detail(&actor(a), id, q.evidence_page)
                .await
        }
        .await,
    )
}
#[derive(Deserialize)]
struct SourceKey {
    source_key: String,
}
async fn memory_source(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
    Path(id): Path<i64>,
    Query(q): Query<SourceKey>,
) -> Response {
    result(
        async {
            s.memories()
                .await?
                .source(&actor(a), id, &q.source_key)
                .await
        }
        .await,
    )
}
async fn finish_mutation(s: &AdminState, r: Result<Value>) -> Result<Value> {
    if let Ok(value) = &r {
        let scopes = value["scopes"]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        s.runtime.cancel_scopes(&scopes).await;
    }
    r
}
async fn mutate(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
    Json(v): Json<memory::MutationRequest>,
) -> Response {
    if matches!(
        v.action,
        memory::Action::Reset | memory::Action::Mode { .. }
    ) && let Err(e) = s.auth.require_recent(&a)
    {
        return auth_error(e);
    }
    // Keep post-commit cancellation paired with the durable mutation even if HTTP disconnects.
    result(
        s.mutations
            .clone()
            .spawn(async move {
                let r = async { s.memories().await?.mutate(&actor(a), v).await }.await;
                finish_mutation(&s, r).await
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("管理操作异常，请查询操作结果"))),
    )
}
async fn group_persona(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
    Json(request): Json<memory::GroupPersonaRequest>,
) -> Response {
    result(
        s.mutations
            .clone()
            .spawn(async move {
                let changed = s.contacts.set_group_persona(&actor(a), request).await;
                finish_mutation(&s, changed).await
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("批量人设操作异常，请查询操作结果"))),
    )
}
async fn forget_preview(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
    Json(v): Json<memory::ForgetPreviewRequest>,
) -> Response {
    result(async { s.memories().await?.preview_forget(&actor(a), v).await }.await)
}
async fn forget_confirm(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
    Json(v): Json<memory::ForgetConfirmRequest>,
) -> Response {
    if let Err(e) = s.auth.require_recent(&a) {
        return auth_error(e);
    }
    result(
        s.mutations
            .clone()
            .spawn(async move {
                let r = async { s.memories().await?.confirm_forget(&actor(a), v).await }.await;
                finish_mutation(&s, r).await
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("管理操作异常，请查询操作结果"))),
    )
}
async fn operation(
    State(s): State<Arc<AdminState>>,
    Extension(a): Extension<TrustedSession>,
    Path(id): Path<String>,
) -> Response {
    result(async { s.memories().await?.operation(&actor(a), &id).await }.await)
}

async fn embedding_test(
    State(s): State<Arc<AdminState>>,
    request: std::result::Result<Json<embedding::EmbeddingTestRequest>, JsonRejection>,
) -> Response {
    let result = match request {
        Ok(Json(request)) => s.embedding.test(request).await,
        Err(_) => Err(embedding::EmbeddingTestError::Invalid),
    };
    match result {
        Ok(view) => Json(view).into_response(),
        Err(error) => (
            error.status(),
            Json(json!({"error":error.to_string(),"code":error.code()})),
        )
            .into_response(),
    }
}

async fn model_list(
    State(s): State<Arc<AdminState>>,
    request: std::result::Result<Json<models::ModelsRequest>, JsonRejection>,
) -> Response {
    let result = match request {
        Ok(Json(request)) => s.models.list(request).await,
        Err(_) => Err(models::ModelsError::Invalid),
    };
    match result {
        Ok(view) => Json(view).into_response(),
        Err(error) => (
            error.status(),
            Json(json!({"error":error.to_string(),"code":error.code()})),
        )
            .into_response(),
    }
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
