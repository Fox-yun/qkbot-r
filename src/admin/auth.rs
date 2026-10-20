//! Single administrator, durable Argon2id credentials and bounded server sessions.
use std::{
    collections::{HashMap, VecDeque},
    fmt, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::types::now;

pub const SESSION_COOKIE: &str = "qkbot_session";
pub const CSRF_HEADER: &str = "x-csrf-token";
const ABSOLUTE_LIFETIME: Duration = Duration::from_secs(8 * 60 * 60);
const IDLE_LIFETIME: Duration = Duration::from_secs(30 * 60);
const RECENT_LIFETIME: Duration = Duration::from_secs(5 * 60);
const MAX_SESSIONS: usize = 32;
const ATTEMPTS_PER_MINUTE: usize = 10;
const PHC_PREFIX: &str = "$argon2id$v=19$m=19456,t=2,p=1$";

pub type AuthResult<T> = std::result::Result<T, AuthError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    Required,
    Csrf,
    Recent,
    Invalid,
    Busy,
    RateLimited,
    SetupComplete,
    InvalidPassword,
    Storage,
}
impl AuthError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Required => "auth_required",
            Self::Csrf => "csrf_invalid",
            Self::Recent => "reauth_required",
            Self::Invalid => "auth_invalid",
            Self::Busy => "auth_busy",
            Self::RateLimited => "auth_rate_limited",
            Self::SetupComplete => "setup_complete",
            Self::InvalidPassword => "invalid_password",
            Self::Storage => "auth_storage",
        }
    }
    pub fn status(self) -> StatusCode {
        match self {
            Self::Required | Self::Invalid => StatusCode::UNAUTHORIZED,
            Self::Csrf | Self::Recent => StatusCode::FORBIDDEN,
            Self::Busy => StatusCode::SERVICE_UNAVAILABLE,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::SetupComplete => StatusCode::CONFLICT,
            Self::InvalidPassword => StatusCode::BAD_REQUEST,
            Self::Storage => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}
impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Required => "请先登录管理员账号",
            Self::Csrf => "请求校验已失效，请刷新页面后重试",
            Self::Recent => "此操作需要重新验证管理员密码",
            Self::Invalid => "密码或初始化码不正确",
            Self::Busy => "正在验证管理员身份，请稍后重试",
            Self::RateLimited => "身份验证尝试过于频繁，请一分钟后重试",
            Self::SetupComplete => "管理员密码已经设置，请使用密码登录",
            Self::InvalidPassword => "密码至少12个字符且不超过1024字节，不能仅包含空白",
            Self::Storage => "管理员认证存储不可用，请检查服务日志和文件权限",
        })
    }
}
impl std::error::Error for AuthError {}

#[derive(Clone, Serialize)]
pub struct AuthView {
    pub setup_required: bool,
    pub authenticated: bool,
    pub csrf_token: Option<String>,
    pub recently_verified: bool,
    pub expires_at: Option<i64>,
}

pub struct AuthGrant {
    pub view: AuthView,
    pub cookie: HeaderValue,
}

/// Constructed only from a live server session; callers cannot forge one.
#[derive(Clone)]
#[allow(clippy::manual_non_exhaustive)] // Private marker also seals construction inside this crate.
pub struct TrustedSession {
    pub admin_id: String,
    pub session_id: String,
    pub recently_verified: bool,
    pub csrf_token: String,
    _trusted: (),
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Credential {
    version: u8,
    admin_id: String,
    password_hash: String,
}

struct Session {
    admin_id: String,
    csrf: String,
    created: Instant,
    interacted: Instant,
    verified: Instant,
}
impl Session {
    fn alive(&self, at: Instant) -> bool {
        at.duration_since(self.created) < ABSOLUTE_LIFETIME
            && at.duration_since(self.interacted) < IDLE_LIFETIME
    }
    fn recent(&self, at: Instant) -> bool {
        at.duration_since(self.verified) < RECENT_LIFETIME
    }
    fn view(&self, at: Instant) -> AuthView {
        let remaining = ABSOLUTE_LIFETIME
            .saturating_sub(at.duration_since(self.created))
            .min(IDLE_LIFETIME.saturating_sub(at.duration_since(self.interacted)));
        AuthView {
            setup_required: false,
            authenticated: true,
            csrf_token: Some(self.csrf.clone()),
            recently_verified: self.recent(at),
            expires_at: Some(now() + remaining.as_secs() as i64),
        }
    }
}

struct State {
    credential: Option<Credential>,
    setup_hash: Option<[u8; 32]>,
    sessions: HashMap<String, Session>,
    attempts: VecDeque<Instant>,
}

pub struct AuthService {
    path: PathBuf,
    secure_cookie: bool,
    state: Mutex<State>,
    hashing: Arc<Semaphore>,
}

impl AuthService {
    pub async fn open(
        config_path: &Path,
        setup_code: &str,
        secure_cookie: bool,
    ) -> Result<Arc<Self>> {
        let path = config_path
            .parent()
            .context("配置路径缺少父目录")?
            .join(".qkbot-admin")
            .join("credentials.json");
        let read_path = path.clone();
        let credential = tokio::task::spawn_blocking(move || {
            let directory = read_path.parent().ok_or(AuthError::Storage)?;
            fs::create_dir_all(directory).map_err(|_| AuthError::Storage)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                    .map_err(|_| AuthError::Storage)?;
            }
            read_credential(&read_path)
        })
        .await
        .context("管理员认证加载任务失败")??;
        let setup_hash = if credential.is_none() {
            anyhow::ensure!(setup_code.len() >= 32, "首次初始化码至少需要32字节");
            Some(Sha256::digest(setup_code.as_bytes()).into())
        } else {
            None
        };
        Ok(Arc::new(Self {
            path,
            secure_cookie,
            state: Mutex::new(State {
                credential,
                setup_hash,
                sessions: HashMap::new(),
                attempts: VecDeque::new(),
            }),
            hashing: Arc::new(Semaphore::new(1)),
        }))
    }

    pub fn is_configured(&self) -> bool {
        self.state
            .lock()
            .map_or(true, |state| state.credential.is_some())
    }

    pub async fn state(&self, headers: &HeaderMap) -> AuthView {
        if let Ok(session) = self.authenticate(headers, false).await
            && let Ok(state) = self.state.lock()
            && let Some(current) = state.sessions.get(&session.session_id)
            && current.alive(Instant::now())
        {
            return current.view(Instant::now());
        }
        AuthView {
            setup_required: !self.is_configured(),
            authenticated: false,
            csrf_token: None,
            recently_verified: false,
            expires_at: None,
        }
    }

    pub async fn authenticate(
        &self,
        headers: &HeaderMap,
        touch: bool,
    ) -> AuthResult<TrustedSession> {
        let token = session_cookie(headers).ok_or(AuthError::Required)?;
        let key = digest_key(token);
        let mut state = self.state.lock().map_err(|_| AuthError::Storage)?;
        let at = Instant::now();
        state.sessions.retain(|_, session| session.alive(at));
        let session = state.sessions.get_mut(&key).ok_or(AuthError::Required)?;
        if touch {
            session.interacted = at;
        }
        Ok(TrustedSession {
            admin_id: session.admin_id.clone(),
            session_id: key,
            recently_verified: session.recent(at),
            csrf_token: session.csrf.clone(),
            _trusted: (),
        })
    }

    pub fn check_csrf(&self, session: &TrustedSession, headers: &HeaderMap) -> AuthResult<()> {
        let supplied = headers
            .get(CSRF_HEADER)
            .and_then(|value| value.to_str().ok())
            .ok_or(AuthError::Csrf)?;
        let state = self.state.lock().map_err(|_| AuthError::Storage)?;
        let current = live_session(&state, session)?;
        if !constant_eq(supplied.as_bytes(), current.csrf.as_bytes()) {
            return Err(AuthError::Csrf);
        }
        Ok(())
    }

    pub fn require_recent(&self, session: &TrustedSession) -> AuthResult<()> {
        let state = self.state.lock().map_err(|_| AuthError::Storage)?;
        if !live_session(&state, session)?.recent(Instant::now()) {
            return Err(AuthError::Recent);
        }
        Ok(())
    }

    fn begin_attempt(&self) -> AuthResult<OwnedSemaphorePermit> {
        let permit = self
            .hashing
            .clone()
            .try_acquire_owned()
            .map_err(|_| AuthError::Busy)?;
        let mut state = self.state.lock().map_err(|_| AuthError::Storage)?;
        let at = Instant::now();
        while state
            .attempts
            .front()
            .is_some_and(|time| at.duration_since(*time) >= Duration::from_secs(60))
        {
            state.attempts.pop_front();
        }
        if state.attempts.len() >= ATTEMPTS_PER_MINUTE {
            return Err(AuthError::RateLimited);
        }
        state.attempts.push_back(at);
        Ok(permit)
    }

    pub async fn setup(self: &Arc<Self>, code: String, password: String) -> AuthResult<AuthGrant> {
        validate_password(&password)?;
        let permit = self.begin_attempt()?;
        let service = self.clone();
        // The owned permit and entire commit remain in this blocking job if an
        // HTTP caller disconnects. No durable/in-memory credential split occurs.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            {
                let state = service.state.lock().map_err(|_| AuthError::Storage)?;
                if state.credential.is_some() {
                    return Err(AuthError::SetupComplete);
                }
                let hash: [u8; 32] = Sha256::digest(code.as_bytes()).into();
                if !state
                    .setup_hash
                    .is_some_and(|expected| constant_eq(&expected, &hash))
                {
                    return Err(AuthError::Invalid);
                }
            }
            let credential = Credential {
                version: 1,
                admin_id: uuid::Uuid::new_v4().to_string(),
                password_hash: hash_password(&password)?,
            };
            let mut state = service.state.lock().map_err(|_| AuthError::Storage)?;
            if state.credential.is_some() {
                return Err(AuthError::SetupComplete);
            }
            persist_credential(&service.path, &credential, None)?;
            state.credential = Some(credential);
            state.setup_hash = None;
            state.sessions.clear();
            service.grant(&mut state, None)
        })
        .await
        .map_err(|_| AuthError::Storage)?
    }

    pub async fn login(self: &Arc<Self>, password: String) -> AuthResult<AuthGrant> {
        if password.len() > 1024 {
            return Err(AuthError::Invalid);
        }
        let permit = self.begin_attempt()?;
        let service = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let credential = service
                .state
                .lock()
                .map_err(|_| AuthError::Storage)?
                .credential
                .clone()
                .ok_or(AuthError::Required)?;
            verify_password(&password, &credential.password_hash)?;
            let mut state = service.state.lock().map_err(|_| AuthError::Storage)?;
            if state.credential.as_ref() != Some(&credential) {
                return Err(AuthError::Required);
            }
            service.grant(&mut state, None)
        })
        .await
        .map_err(|_| AuthError::Storage)?
    }

    pub async fn reauthenticate(
        self: &Arc<Self>,
        session: &TrustedSession,
        password: String,
    ) -> AuthResult<AuthGrant> {
        if password.len() > 1024 {
            return Err(AuthError::Invalid);
        }
        let permit = self.begin_attempt()?;
        let service = self.clone();
        let session = session.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let credential = {
                let state = service.state.lock().map_err(|_| AuthError::Storage)?;
                live_session(&state, &session)?;
                state.credential.clone().ok_or(AuthError::Required)?
            };
            verify_password(&password, &credential.password_hash)?;
            let mut state = service.state.lock().map_err(|_| AuthError::Storage)?;
            let created = live_session(&state, &session)?.created;
            if state.credential.as_ref() != Some(&credential) {
                return Err(AuthError::Required);
            }
            state.sessions.remove(&session.session_id);
            service.grant(&mut state, Some(created))
        })
        .await
        .map_err(|_| AuthError::Storage)?
    }

    pub async fn change_password(
        self: &Arc<Self>,
        session: &TrustedSession,
        new_password: String,
    ) -> AuthResult<AuthGrant> {
        self.require_recent(session)?;
        validate_password(&new_password)?;
        let permit = self.begin_attempt()?;
        let service = self.clone();
        let session = session.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            service.require_recent(&session)?;
            let password_hash = hash_password(&new_password)?;
            let mut state = service.state.lock().map_err(|_| AuthError::Storage)?;
            if !live_session(&state, &session)?.recent(Instant::now()) {
                return Err(AuthError::Recent);
            }
            let previous = state.credential.clone().ok_or(AuthError::Required)?;
            let credential = Credential {
                password_hash,
                ..previous.clone()
            };
            persist_credential(&service.path, &credential, Some(&previous))?;
            state.credential = Some(credential);
            state.sessions.clear();
            service.grant(&mut state, None)
        })
        .await
        .map_err(|_| AuthError::Storage)?
    }

    pub fn logout(&self, session: &TrustedSession) -> HeaderValue {
        if let Ok(mut state) = self.state.lock() {
            state.sessions.remove(&session.session_id);
        }
        self.cookie("", 0)
    }

    fn grant(&self, state: &mut State, created: Option<Instant>) -> AuthResult<AuthGrant> {
        let at = Instant::now();
        state.sessions.retain(|_, session| session.alive(at));
        if state.sessions.len() >= MAX_SESSIONS
            && let Some(key) = state
                .sessions
                .iter()
                .min_by_key(|(_, session)| session.interacted)
                .map(|(key, _)| key.clone())
        {
            state.sessions.remove(&key);
        }
        let token = random_token();
        let session = Session {
            admin_id: state
                .credential
                .as_ref()
                .ok_or(AuthError::Required)?
                .admin_id
                .clone(),
            csrf: random_token(),
            created: created.unwrap_or(at),
            interacted: at,
            verified: at,
        };
        let view = session.view(at);
        let max_age = ABSOLUTE_LIFETIME
            .saturating_sub(at.duration_since(session.created))
            .as_secs();
        state.sessions.insert(digest_key(&token), session);
        Ok(AuthGrant {
            view,
            cookie: self.cookie(&token, max_age),
        })
    }

    fn cookie(&self, value: &str, max_age: u64) -> HeaderValue {
        let secure = if self.secure_cookie { "; Secure" } else { "" };
        let mut cookie = HeaderValue::from_str(&format!(
            "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}"
        ))
        .expect("generated session cookie is ASCII");
        cookie.set_sensitive(true);
        cookie
    }
}

fn live_session<'a>(state: &'a State, trusted: &TrustedSession) -> AuthResult<&'a Session> {
    state
        .sessions
        .get(&trusted.session_id)
        .filter(|session| {
            session.alive(Instant::now())
                && session.admin_id == trusted.admin_id
                && constant_eq(session.csrf.as_bytes(), trusted.csrf_token.as_bytes())
        })
        .ok_or(AuthError::Required)
}
fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    let mut found = None;
    for header in headers.get_all(header::COOKIE) {
        for pair in header.to_str().ok()?.split(';') {
            let Some((name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if name == SESSION_COOKIE {
                if found.is_some()
                    || value.len() != 64
                    || !value.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return None;
                }
                found = Some(value);
            }
        }
    }
    found
}
fn random_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn digest_key(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    let a: [u8; 32] = Sha256::digest(a).into();
    let b: [u8; 32] = Sha256::digest(b).into();
    a.iter()
        .zip(b)
        .fold(0_u8, |difference, (x, y)| difference | (x ^ y))
        == 0
}
fn validate_password(password: &str) -> AuthResult<()> {
    if password.len() > 1024 || password.chars().count() < 12 || password.trim().is_empty() {
        return Err(AuthError::InvalidPassword);
    }
    Ok(())
}
fn hash_password(password: &str) -> AuthResult<String> {
    let salt =
        SaltString::encode_b64(uuid::Uuid::new_v4().as_bytes()).map_err(|_| AuthError::Storage)?;
    let params = Params::new(19 * 1024, 2, 1, Some(32)).map_err(|_| AuthError::Storage)?;
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| AuthError::Storage)
}
fn verify_password(password: &str, encoded: &str) -> AuthResult<()> {
    let hash = PasswordHash::new(encoded).map_err(|_| AuthError::Storage)?;
    Argon2::default()
        .verify_password(password.as_bytes(), &hash)
        .map_err(|_| AuthError::Invalid)
}
fn read_credential(path: &Path) -> AuthResult<Option<Credential>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(AuthError::Storage),
    };
    let mut bytes = Vec::new();
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| AuthError::Storage)?;
    if bytes.len() > 4096 {
        return Err(AuthError::Storage);
    }
    let credential: Credential = serde_json::from_slice(&bytes).map_err(|_| AuthError::Storage)?;
    if credential.version != 1
        || uuid::Uuid::parse_str(&credential.admin_id).is_err()
        || !credential.password_hash.starts_with(PHC_PREFIX)
        || credential.password_hash.len() > 256
    {
        return Err(AuthError::Storage);
    }
    let parsed = PasswordHash::new(&credential.password_hash).map_err(|_| AuthError::Storage)?;
    if parsed.salt.is_none() || parsed.hash.is_none() {
        return Err(AuthError::Storage);
    }
    Ok(Some(credential))
}
fn persist_credential(
    path: &Path,
    credential: &Credential,
    previous: Option<&Credential>,
) -> AuthResult<()> {
    if read_credential(path)?.as_ref() != previous {
        return Err(AuthError::Storage);
    }
    let mut file = tempfile::Builder::new()
        .prefix(".qkbot-admin-")
        .tempfile_in(path.parent().ok_or(AuthError::Storage)?)
        .map_err(|_| AuthError::Storage)?;
    let bytes = serde_json::to_vec(credential).map_err(|_| AuthError::Storage)?;
    file.write_all(&bytes).map_err(|_| AuthError::Storage)?;
    file.as_file().sync_all().map_err(|_| AuthError::Storage)?;
    if previous.is_none() {
        file.persist_noclobber(path)
            .map_err(|_| AuthError::Storage)?;
    } else {
        if read_credential(path)?.as_ref() != previous {
            return Err(AuthError::Storage);
        }
        file.persist(path).map_err(|_| AuthError::Storage)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const CODE: &str = "initialization-code-at-least-thirty-two-bytes";
    const PASSWORD: &str = "a long test administrator password";
    async fn service() -> (tempfile::TempDir, Arc<AuthService>) {
        let dir = tempfile::tempdir().unwrap();
        let service = AuthService::open(&dir.path().join("config.toml"), CODE, false)
            .await
            .unwrap();
        (dir, service)
    }
    fn headers(grant: &AuthGrant) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(grant.cookie.to_str().unwrap().split(';').next().unwrap())
                .unwrap(),
        );
        headers.insert(
            CSRF_HEADER,
            HeaderValue::from_str(grant.view.csrf_token.as_ref().unwrap()).unwrap(),
        );
        headers
    }
    #[tokio::test]
    async fn setup_persists_only_argon2_and_legacy_bearer_never_authenticates() {
        let (dir, service) = service().await;
        assert!(service.state(&HeaderMap::new()).await.setup_required);
        assert!(matches!(
            service.setup("wrong".into(), PASSWORD.into()).await,
            Err(AuthError::Invalid)
        ));
        let grant = service.setup(CODE.into(), PASSWORD.into()).await.unwrap();
        let cookie = grant.cookie.to_str().unwrap();
        assert!(cookie.contains("HttpOnly; SameSite=Strict") && !cookie.contains("; Secure"));
        let headers = headers(&grant);
        let trusted = service.authenticate(&headers, false).await.unwrap();
        service.check_csrf(&trusted, &headers).unwrap();
        assert_eq!(
            service.check_csrf(&trusted, &HeaderMap::new()),
            Err(AuthError::Csrf)
        );
        let mut bearer = HeaderMap::new();
        bearer.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {CODE}")).unwrap(),
        );
        assert!(matches!(
            service.authenticate(&bearer, true).await,
            Err(AuthError::Required)
        ));
        assert!(matches!(
            service.setup(CODE.into(), PASSWORD.into()).await,
            Err(AuthError::SetupComplete)
        ));
        let file = fs::read_to_string(dir.path().join(".qkbot-admin/credentials.json")).unwrap();
        assert!(file.contains(PHC_PREFIX) && !file.contains(PASSWORD) && !file.contains(CODE));
        let reopened = AuthService::open(&dir.path().join("config.toml"), "", true)
            .await
            .unwrap();
        assert!(!reopened.state(&HeaderMap::new()).await.setup_required);
        assert!(reopened.authenticate(&headers, false).await.is_err());
        let login = reopened.login(PASSWORD.into()).await.unwrap();
        assert!(login.cookie.to_str().unwrap().contains("; Secure"));
    }
    #[tokio::test]
    async fn polling_does_not_extend_idle_expiry_and_interaction_does_not_extend_absolute_expiry() {
        let (_dir, service) = service().await;
        let grant = service.setup(CODE.into(), PASSWORD.into()).await.unwrap();
        let headers = headers(&grant);
        let trusted = service.authenticate(&headers, false).await.unwrap();
        let prior = Instant::now() - Duration::from_secs(29 * 60);
        service
            .state
            .lock()
            .unwrap()
            .sessions
            .get_mut(&trusted.session_id)
            .unwrap()
            .interacted = prior;
        service.state(&headers).await;
        assert_eq!(
            service.state.lock().unwrap().sessions[&trusted.session_id].interacted,
            prior
        );
        service.authenticate(&headers, true).await.unwrap();
        assert!(service.state.lock().unwrap().sessions[&trusted.session_id].interacted > prior);
        service
            .state
            .lock()
            .unwrap()
            .sessions
            .get_mut(&trusted.session_id)
            .unwrap()
            .created = Instant::now() - ABSOLUTE_LIFETIME;
        assert!(service.authenticate(&headers, true).await.is_err());
    }
    #[tokio::test]
    async fn password_change_requires_recent_verification_and_revokes_every_old_session() {
        let (_dir, service) = service().await;
        let first = service.setup(CODE.into(), PASSWORD.into()).await.unwrap();
        let second = service.login(PASSWORD.into()).await.unwrap();
        let first_headers = headers(&first);
        let first_session = service.authenticate(&first_headers, false).await.unwrap();
        service
            .state
            .lock()
            .unwrap()
            .sessions
            .get_mut(&first_session.session_id)
            .unwrap()
            .verified = Instant::now() - RECENT_LIFETIME;
        assert_eq!(
            service.require_recent(&first_session),
            Err(AuthError::Recent)
        );
        assert!(matches!(
            service
                .change_password(&first_session, "new administrator password".into())
                .await,
            Err(AuthError::Recent)
        ));
        let reauth = service
            .reauthenticate(&first_session, PASSWORD.into())
            .await
            .unwrap();
        assert!(service.authenticate(&first_headers, false).await.is_err());
        let current = service
            .authenticate(&headers(&reauth), false)
            .await
            .unwrap();
        let changed = service
            .change_password(&current, "new administrator password".into())
            .await
            .unwrap();
        assert!(
            service
                .authenticate(&headers(&second), false)
                .await
                .is_err()
        );
        assert!(
            service
                .authenticate(&headers(&reauth), false)
                .await
                .is_err()
        );
        assert!(matches!(
            service.login(PASSWORD.into()).await,
            Err(AuthError::Invalid)
        ));
        let trusted = service
            .authenticate(&headers(&changed), false)
            .await
            .unwrap();
        service.logout(&trusted);
        assert!(
            service
                .authenticate(&headers(&changed), false)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn password_work_has_one_slot_and_a_global_attempt_limit() {
        let (_dir, service) = service().await;
        let permit = service.hashing.clone().acquire_owned().await.unwrap();
        assert!(matches!(
            service.login(PASSWORD.into()).await,
            Err(AuthError::Busy)
        ));
        drop(permit);
        for _ in 0..ATTEMPTS_PER_MINUTE {
            assert!(matches!(
                service.setup("wrong".into(), PASSWORD.into()).await,
                Err(AuthError::Invalid)
            ));
        }
        assert!(matches!(
            service.setup(CODE.into(), PASSWORD.into()).await,
            Err(AuthError::RateLimited)
        ));
    }
    #[test]
    fn session_count_is_bounded_and_duplicate_cookies_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let service = AuthService {
            path: dir.path().join(".qkbot-admin"),
            secure_cookie: false,
            hashing: Arc::new(Semaphore::new(1)),
            state: Mutex::new(State {
                credential: Some(Credential {
                    version: 1,
                    admin_id: uuid::Uuid::new_v4().to_string(),
                    password_hash: String::new(),
                }),
                setup_hash: None,
                sessions: HashMap::new(),
                attempts: VecDeque::new(),
            }),
        };
        let mut state = service.state.lock().unwrap();
        for _ in 0..100 {
            service.grant(&mut state, None).unwrap();
        }
        assert_eq!(state.sessions.len(), MAX_SESSIONS);
        let grant = service.grant(&mut state, None).unwrap();
        let pair = grant.cookie.to_str().unwrap().split(';').next().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{pair}; {pair}")).unwrap(),
        );
        assert!(session_cookie(&headers).is_none());
    }
}
