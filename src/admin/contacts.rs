//! Account-scoped real QQ membership for the administrator, independent of AI startup.
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    access::AccessStore,
    milky::{Milky, MilkyAccountChanged, MilkyContact, MilkyContacts},
    types::{Scene, Scope, now},
};

use super::{runtime::Runtime, settings::ConfigStore};

const CACHE_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactsError {
    Offline,
    Stale,
    Invalid,
}
impl ContactsError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Offline => "contacts_offline",
            Self::Stale => "contacts_stale",
            Self::Invalid => "contacts_invalid",
        }
    }
    pub fn status(self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            Self::Offline => StatusCode::BAD_GATEWAY,
            Self::Stale => StatusCode::CONFLICT,
            Self::Invalid => StatusCode::BAD_REQUEST,
        }
    }
}
impl std::fmt::Display for ContactsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Offline => {
                "暂时无法读取 QQ 列表或范围设置，请确认账号已登录、Milky 可用及响应大小限制"
            }
            Self::Stale => "QQ 账号或连接配置已变化，请重新刷新列表",
            Self::Invalid => "请求范围无效或目标不属于当前账号的好友和群列表",
        })
    }
}
impl std::error::Error for ContactsError {}

#[derive(Debug, Clone, Serialize)]
pub struct ContactView {
    pub id: String,
    pub name: String,
    pub remark: String,
    pub enabled: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct ContactsView {
    pub bot_id: String,
    pub groups: Vec<ContactView>,
    pub friends: Vec<ContactView>,
    pub fetched_at: i64,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToggleRequest {
    pub bot_id: String,
    pub scene: Scene,
    pub peer_id: String,
    pub enabled: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct ToggleView {
    pub bot_id: String,
    pub scene: Scene,
    pub peer_id: String,
    pub enabled: bool,
}

struct CachedContacts {
    completed_at: Instant,
    fetched_at: i64,
    contacts: MilkyContacts,
}
#[derive(Default)]
struct State {
    version: Option<u64>,
    client: Option<Milky>,
    cached: Option<CachedContacts>,
    failure: Option<(Instant, ContactsError)>,
}
pub struct ContactsService {
    store: Arc<ConfigStore>,
    runtime: Arc<Runtime>,
    state: Mutex<State>,
}
impl ContactsService {
    pub fn new(store: Arc<ConfigStore>, runtime: Arc<Runtime>) -> Self {
        Self {
            store,
            runtime,
            state: Mutex::new(State::default()),
        }
    }

    async fn client(&self, state: &mut State) -> Result<(Milky, u64)> {
        let (config, version) = self
            .store
            .milky_config_version()
            .await
            .map_err(|_| ContactsError::Invalid)?;
        if state.version != Some(version) {
            *state = State {
                version: Some(version),
                ..State::default()
            };
        }
        if state.client.is_none() {
            let http = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .pool_max_idle_per_host(2)
                .build()
                .map_err(|_| ContactsError::Invalid)?;
            state.client = Some(Milky::new(config, http).map_err(|_| ContactsError::Invalid)?);
        }
        Ok((
            state.client.as_ref().expect("client initialized").clone(),
            version,
        ))
    }

    async fn unchanged(&self, version: u64) -> Result<()> {
        if self
            .store
            .milky_config_version()
            .await
            .map_err(|_| ContactsError::Invalid)?
            .1
            != version
        {
            return Err(ContactsError::Stale.into());
        }
        Ok(())
    }

    /// Even a cache hit probes the live account before returning any personal
    /// names. The cache avoids refetching membership, never identity checking.
    pub async fn list(&self, force: bool) -> Result<ContactsView> {
        let requested_at = Instant::now();
        let mut state = self.state.lock().await;
        let (client, version) = self.client(&mut state).await?;
        if let Some((completed_at, error)) = state.failure
            && completed_at >= requested_at
        {
            return Err(error.into());
        }
        let result = self
            .list_inner(&client, version, &mut state, requested_at, force)
            .await;
        if let Err(error) = &result {
            state.cached = None;
            state.failure = Some((
                Instant::now(),
                error
                    .downcast_ref::<ContactsError>()
                    .copied()
                    .unwrap_or(ContactsError::Offline),
            ));
        } else {
            state.failure = None;
        }
        result
    }

    async fn list_inner(
        &self,
        client: &Milky,
        version: u64,
        state: &mut State,
        requested_at: Instant,
        force: bool,
    ) -> Result<ContactsView> {
        let bot_id = client
            .current_account()
            .await
            .map_err(|_| ContactsError::Offline)?;
        self.unchanged(version).await?;
        if state
            .cached
            .as_ref()
            .is_some_and(|cache| cache.contacts.bot_id != bot_id)
        {
            state.cached = None;
        }
        let reuse = state.cached.as_ref().is_some_and(|cache| {
            cache.completed_at >= requested_at
                || (!force && cache.completed_at.elapsed() < CACHE_TTL)
        });
        if !reuse {
            let contacts = client.contacts(force).await.map_err(transport_error)?;
            if contacts.bot_id != bot_id {
                return Err(ContactsError::Stale.into());
            }
            self.unchanged(version).await?;
            state.cached = Some(CachedContacts {
                completed_at: Instant::now(),
                fetched_at: now(),
                contacts,
            });
        }
        let access = AccessStore::new(
            self.runtime
                .data()
                .await
                .map_err(|_| ContactsError::Offline)?,
        );
        let disabled: HashSet<Scope> = access
            .disabled(bot_id)
            .await
            .map_err(|_| ContactsError::Offline)?
            .into_iter()
            .collect();
        // Include database wait time inside the account/configuration fence.
        if client
            .current_account()
            .await
            .map_err(|_| ContactsError::Offline)?
            != bot_id
        {
            return Err(ContactsError::Stale.into());
        }
        self.unchanged(version).await?;
        let cache = state.cached.as_ref().expect("membership loaded");
        let convert = |contact: &MilkyContact, scene| ContactView {
            id: contact.id.to_string(),
            name: contact.name.clone(),
            remark: contact.remark.clone(),
            enabled: !disabled.contains(&Scope {
                bot_id,
                scene,
                peer_id: contact.id,
            }),
        };
        Ok(ContactsView {
            bot_id: bot_id.to_string(),
            groups: cache
                .contacts
                .groups
                .iter()
                .map(|entry| convert(entry, Scene::Group))
                .collect(),
            friends: cache
                .contacts
                .friends
                .iter()
                .map(|entry| convert(entry, Scene::Friend))
                .collect(),
            fetched_at: cache.fetched_at,
        })
    }

    pub async fn set_enabled(&self, request: ToggleRequest) -> Result<ToggleView> {
        let bot_id = parse_id(&request.bot_id)?;
        let peer_id = parse_id(&request.peer_id)?;
        let scope = Scope {
            bot_id,
            scene: request.scene,
            peer_id,
        };
        let mut state = self.state.lock().await;
        let (client, version) = self.client(&mut state).await?;
        // Never authorize a write from a cached list: the account may have left
        // a group, removed a friend, or switched since the UI was rendered.
        let contacts = client.contacts(true).await.map_err(transport_error)?;
        if contacts.bot_id != bot_id {
            state.cached = None;
            return Err(ContactsError::Stale.into());
        }
        let entries = match request.scene {
            Scene::Group => &contacts.groups,
            Scene::Friend => &contacts.friends,
        };
        if !entries.iter().any(|entry| entry.id == peer_id) {
            return Err(ContactsError::Invalid.into());
        }
        let access = AccessStore::new(
            self.runtime
                .data()
                .await
                .map_err(|_| ContactsError::Offline)?,
        );
        if client
            .current_account()
            .await
            .map_err(|_| ContactsError::Offline)?
            != bot_id
        {
            state.cached = None;
            return Err(ContactsError::Stale.into());
        }
        self.unchanged(version).await?;
        // Persistence is keyed by the verified account, never a global peer ID.
        access
            .set_enabled(scope.clone(), request.enabled)
            .await
            .map_err(|_| ContactsError::Offline)?;
        self.runtime.cancel_scopes(&[scope.key()]).await;
        state.cached = Some(CachedContacts {
            completed_at: Instant::now(),
            fetched_at: now(),
            contacts,
        });
        state.failure = None;
        Ok(ToggleView {
            bot_id: bot_id.to_string(),
            scene: request.scene,
            peer_id: peer_id.to_string(),
            enabled: request.enabled,
        })
    }
}

fn parse_id(value: &str) -> Result<i64> {
    if value.is_empty() || value.len() > 19 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ContactsError::Invalid.into());
    }
    value
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| ContactsError::Invalid.into())
}

fn transport_error(error: anyhow::Error) -> ContactsError {
    if error.is::<MilkyAccountChanged>() {
        ContactsError::Stale
    } else {
        ContactsError::Offline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::{
        Json, Router,
        extract::State as AxumState,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use tokio::sync::Semaphore;
    use tokio_util::sync::CancellationToken;

    struct Mock {
        account: AtomicI64,
        groups: AtomicUsize,
        friends: AtomicUsize,
        flip_on_groups: AtomicBool,
        gate: Semaphore,
        forced: std::sync::Mutex<Vec<bool>>,
    }
    async fn identity(AxumState(state): AxumState<Arc<Mock>>) -> Json<Value> {
        Json(json!({"status":"ok","retcode":0,"data":{"uin":state.account.load(Ordering::SeqCst)}}))
    }
    async fn groups(
        AxumState(state): AxumState<Arc<Mock>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        state.groups.fetch_add(1, Ordering::SeqCst);
        state.forced.lock().unwrap().push(body["no_cache"] == true);
        let _permit = state.gate.acquire().await.unwrap();
        if state.flip_on_groups.swap(false, Ordering::SeqCst) {
            state.account.fetch_add(1, Ordering::SeqCst);
        }
        Json(json!({"status":"ok","retcode":0,"data":{"groups":[
            {"group_id":456,"group_name":format!("群-{}",state.account.load(Ordering::SeqCst)),"remark":"群备注"}
        ]}}))
    }
    async fn friends(
        AxumState(state): AxumState<Arc<Mock>>,
        _headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        state.friends.fetch_add(1, Ordering::SeqCst);
        state.forced.lock().unwrap().push(body["no_cache"] == true);
        Json(json!({"status":"ok","retcode":0,"data":{"friends":[
            {"user_id":789,"nickname":"好友昵称","remark":"好友备注"}
        ]}}))
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        store: Arc<ConfigStore>,
        runtime: Arc<Runtime>,
        service: Arc<ContactsService>,
        mock: Arc<Mock>,
        cancel: CancellationToken,
        tasks: Vec<tokio::task::JoinHandle<()>>,
    }
    impl Fixture {
        async fn new() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mock = Arc::new(Mock {
                account: AtomicI64::new(123),
                groups: AtomicUsize::new(0),
                friends: AtomicUsize::new(0),
                flip_on_groups: AtomicBool::new(false),
                gate: Semaphore::new(1),
                forced: std::sync::Mutex::new(Vec::new()),
            });
            let router = Router::new()
                .route("/api/get_login_info", post(identity))
                .route("/api/get_group_list", post(groups))
                .route("/api/get_friend_list", post(friends))
                .with_state(mock.clone());
            let cancel = CancellationToken::new();
            let stop = cancel.clone();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, router)
                    .with_graceful_shutdown(stop.cancelled_owned())
                    .await
                    .unwrap();
            });
            let directory = tempfile::tempdir().unwrap();
            let mut config = Config::default();
            config.app.database = directory
                .path()
                .join("test.db")
                .to_string_lossy()
                .into_owned();
            config.llm.model.clear(); // No model/provider calls; AI startup fails but data remains available.
            config.milky.url = format!("http://{address}");
            config.milky.bot_id = None;
            config.milky.request_timeout_secs = 2;
            let path = directory.path().join("config.toml");
            std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
            let store = Arc::new(ConfigStore::open(path).await.unwrap());
            let (effective, revision) = store.effective_with_revision().await.unwrap();
            let (runtime, worker) = Runtime::start(effective, revision, cancel.clone());
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if runtime.data().await.is_ok() && runtime.status().await.phase == "error" {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let service = Arc::new(ContactsService::new(store.clone(), runtime.clone()));
            Self {
                _directory: directory,
                store,
                runtime,
                service,
                mock,
                cancel,
                tasks: vec![server, worker],
            }
        }
        async fn finish(self) {
            self.cancel.cancel();
            for task in self.tasks {
                tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn lists_work_without_a_model_and_simultaneous_refreshes_share_membership() {
        let fixture = Fixture::new().await;
        let (one, two, three) = tokio::join!(
            fixture.service.list(true),
            fixture.service.list(true),
            fixture.service.list(false)
        );
        for response in [one, two, three] {
            let value = response.unwrap();
            assert_eq!(value.bot_id, "123");
            assert_eq!(value.groups[0].id, "456");
            assert_eq!(value.groups[0].remark, "群备注");
            assert_eq!(value.friends[0].name, "好友昵称");
            assert!(value.groups[0].enabled && value.friends[0].enabled);
        }
        fixture.service.list(false).await.unwrap();
        assert_eq!(fixture.mock.groups.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.mock.friends.load(Ordering::SeqCst), 1);
        fixture.finish().await;
    }

    #[tokio::test]
    async fn cached_contacts_are_not_returned_for_a_different_live_account() {
        let fixture = Fixture::new().await;
        fixture.service.list(false).await.unwrap();
        fixture.mock.account.store(999, Ordering::SeqCst);
        let value = fixture.service.list(false).await.unwrap();
        assert_eq!(value.bot_id, "999");
        assert_eq!(value.groups[0].name, "群-999");
        assert_eq!(fixture.mock.groups.load(Ordering::SeqCst), 2);
        fixture.finish().await;
    }

    #[tokio::test]
    async fn an_account_change_during_membership_fetch_discards_the_result() {
        let fixture = Fixture::new().await;
        fixture.mock.flip_on_groups.store(true, Ordering::SeqCst);
        let error = fixture.service.list(false).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<ContactsError>(),
            Some(&ContactsError::Stale)
        );
        assert!(fixture.service.state.lock().await.cached.is_none());
        fixture.finish().await;
    }

    #[tokio::test]
    async fn toggles_revalidate_real_membership_and_are_account_and_scene_scoped() {
        let fixture = Fixture::new().await;
        fixture.service.list(false).await.unwrap();
        let request = ToggleRequest {
            bot_id: "123".into(),
            scene: Scene::Group,
            peer_id: "456".into(),
            enabled: false,
        };
        let value = fixture.service.set_enabled(request.clone()).await.unwrap();
        assert!(!value.enabled);
        let access = AccessStore::new(fixture.runtime.data().await.unwrap());
        assert!(
            !access
                .enabled(&Scope {
                    bot_id: 123,
                    scene: Scene::Group,
                    peer_id: 456
                })
                .await
                .unwrap()
        );
        assert!(
            access
                .enabled(&Scope {
                    bot_id: 999,
                    scene: Scene::Group,
                    peer_id: 456
                })
                .await
                .unwrap()
        );
        assert!(
            access
                .enabled(&Scope {
                    bot_id: 123,
                    scene: Scene::Friend,
                    peer_id: 456
                })
                .await
                .unwrap()
        );
        assert!(!fixture.service.list(false).await.unwrap().groups[0].enabled);
        let error = fixture
            .service
            .set_enabled(ToggleRequest {
                peer_id: "99999".into(),
                ..request.clone()
            })
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ContactsError>(),
            Some(&ContactsError::Invalid)
        );
        fixture.mock.account.store(999, Ordering::SeqCst);
        let error = fixture.service.set_enabled(request).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<ContactsError>(),
            Some(&ContactsError::Stale)
        );
        assert!(
            access
                .enabled(&Scope {
                    bot_id: 999,
                    scene: Scene::Group,
                    peer_id: 456
                })
                .await
                .unwrap()
        );
        let forced = fixture.mock.forced.lock().unwrap().clone();
        assert!(forced.iter().skip(2).all(|force| *force));
        fixture.finish().await;
    }

    #[tokio::test]
    async fn changed_and_reverted_configuration_discards_an_inflight_old_list() {
        let fixture = Fixture::new().await;
        let permit = fixture.mock.gate.acquire().await.unwrap();
        let service = fixture.service.clone();
        let load = tokio::spawn(async move { service.list(false).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while fixture.mock.groups.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let original = std::fs::read(fixture.store.path()).unwrap();
        let mut changed = Config::load_raw(fixture.store.path()).unwrap();
        changed.milky.request_timeout_secs += 1;
        std::fs::write(fixture.store.path(), toml::to_string(&changed).unwrap()).unwrap();
        fixture.store.reload().await.unwrap();
        std::fs::write(fixture.store.path(), original).unwrap();
        fixture.store.reload().await.unwrap();
        drop(permit);
        let error = tokio::time::timeout(Duration::from_secs(3), load)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ContactsError>(),
            Some(&ContactsError::Stale)
        );
        fixture.finish().await;
    }

    #[tokio::test]
    async fn redirects_are_not_followed_or_reported_with_sensitive_urls() {
        let fixture = Fixture::new().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirected = Arc::new(AtomicUsize::new(0));
        let target_count = redirected.clone();
        let router = Router::new()
            .route(
                "/api/get_login_info",
                post(|| async {
                    (
                        StatusCode::TEMPORARY_REDIRECT,
                        [("location", "/unexpected?token=private-fixture")],
                    )
                }),
            )
            .route(
                "/unexpected",
                post(move || {
                    let count = target_count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Json(json!({"status":"ok","retcode":0,"data":{"uin":123}}))
                    }
                }),
            );
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = fixture.cancel.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
                .unwrap();
        });
        let mut changed = Config::load_raw(fixture.store.path()).unwrap();
        changed.milky.url = url;
        std::fs::write(fixture.store.path(), toml::to_string(&changed).unwrap()).unwrap();
        fixture.store.reload().await.unwrap();
        let error = fixture.service.list(false).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<ContactsError>(),
            Some(&ContactsError::Offline)
        );
        assert!(!format!("{error:#}").contains("private-fixture"));
        assert_eq!(redirected.load(Ordering::SeqCst), 0);
        fixture.finish().await;
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn decimal_identifiers_do_not_round_or_accept_untrusted_scope_syntax() {
        assert_eq!(parse_id("9007199254740993").unwrap(), 9_007_199_254_740_993);
        for value in [
            "",
            "0",
            "-1",
            "+123",
            "123:group:456",
            "1.5",
            "9223372036854775808",
        ] {
            assert!(parse_id(value).is_err());
        }
    }
}
