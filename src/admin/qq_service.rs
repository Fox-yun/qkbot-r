//! One cached QQ login client per configuration generation, without poll-time I/O.
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::{Mutex, Semaphore};

use super::{
    qq::{LoginQrCode, LoginStatus, QqClient, QqError},
    settings::ConfigStore,
};

const STATUS_TTL: Duration = Duration::from_secs(3);
type StatusResult = Result<LoginStatus, QqError>;

#[derive(Default)]
struct State {
    version: Option<u64>,
    client: Option<QqClient>,
    status: Option<(Instant, StatusResult)>,
    auth_blocked: bool,
}

pub struct QqService {
    store: Arc<ConfigStore>,
    state: Mutex<State>,
    qr: Semaphore,
}
impl QqService {
    pub fn new(store: Arc<ConfigStore>) -> Self {
        Self {
            store,
            state: Mutex::new(State::default()),
            qr: Semaphore::new(1),
        }
    }

    async fn client(&self, state: &mut State) -> Result<(QqClient, u64), QqError> {
        let (config, version) = self
            .store
            .qq_config_version()
            .await
            .map_err(|_| QqError::Config)?;
        if state.version != Some(version) {
            *state = State {
                version: Some(version),
                ..State::default()
            };
        }
        if state.client.is_none() {
            state.client = Some(QqClient::new(config).map_err(|_| QqError::Config)?);
        }
        Ok((
            state.client.as_ref().expect("client initialized").clone(),
            version,
        ))
    }

    async fn unchanged(&self, version: u64) -> Result<(), QqError> {
        if self
            .store
            .qq_config_version()
            .await
            .map_err(|_| QqError::Config)?
            .1
            != version
        {
            return Err(QqError::Stale);
        }
        Ok(())
    }

    pub async fn status(&self, force: bool) -> StatusResult {
        let requested_at = Instant::now();
        // Keeping this bounded HTTP operation under one asynchronous mutex also
        // merges simultaneous refreshes. Waiting requests consume its result;
        // they never launch a second request after the first finished.
        let mut state = self.state.lock().await;
        let (client, version) = self.client(&mut state).await?;
        if let Some((completed_at, result)) = &state.status
            && (*completed_at >= requested_at || (!force && completed_at.elapsed() < STATUS_TTL))
        {
            return result.clone();
        }
        if state.auth_blocked && !force {
            return Err(QqError::Auth);
        }
        let result = client.status().await.map_err(classify);
        self.unchanged(version).await?;
        state.auth_blocked = result.as_ref().is_err_and(|error| *error == QqError::Auth);
        state.status = Some((Instant::now(), result.clone()));
        result
    }

    pub async fn qrcode(&self) -> Result<LoginQrCode, QqError> {
        let _permit = self.qr.try_acquire().map_err(|_| QqError::Busy)?;
        let mut state = self.state.lock().await;
        let (client, version) = self.client(&mut state).await?;
        if state.auth_blocked {
            return Err(QqError::Auth);
        }
        let result = client.qrcode().await.map_err(classify);
        self.unchanged(version).await?;
        if result.as_ref().is_err_and(|error| *error == QqError::Auth) {
            state.auth_blocked = true;
            state.status = Some((Instant::now(), Err(QqError::Auth)));
        }
        result
    }
}

fn classify(error: anyhow::Error) -> QqError {
    error
        .downcast_ref::<QqError>()
        .copied()
        .unwrap_or(QqError::Transient)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::sync::CancellationToken;

    async fn fixture(
        status: u16,
        delay: Duration,
    ) -> (
        tempfile::TempDir,
        Arc<ConfigStore>,
        Arc<QqService>,
        Arc<AtomicUsize>,
        Arc<Semaphore>,
        CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::from_toml(include_str!("../../config.example.toml")).unwrap();
        config.plugins.ai.enabled = false;
        config.qq_login.url = format!("http://{}", listener.local_addr().unwrap());
        config.qq_login.token = "fixture-password".into();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        let store = Arc::new(ConfigStore::open(path).await.unwrap());
        let service = Arc::new(QqService::new(store.clone()));
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let gate = Arc::new(Semaphore::new(1));
        let replies = gate.clone();
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! { _ = stop.cancelled() => break, accepted = listener.accept() => accepted };
                let Ok((mut stream, _)) = accepted else {
                    break;
                };
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let length = stream.read(&mut buffer).await.unwrap();
                    if length == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..length]);
                    if request.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                count.fetch_add(1, Ordering::SeqCst);
                let _reply = replies.acquire().await.unwrap();
                tokio::time::sleep(delay).await;
                let body =
                    json!({"success":true,"data":{"online":false,"uin":"","nick":""}}).to_string();
                let response = format!(
                    "HTTP/1.1 {status} Reply\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        (directory, store, service, calls, gate, cancel, task)
    }

    #[tokio::test]
    async fn simultaneous_status_polls_and_forced_refreshes_share_one_request() {
        let (_directory, _store, service, calls, _gate, stop, task) =
            fixture(200, Duration::from_millis(100)).await;
        let (a, b, c) = tokio::join!(
            service.status(true),
            service.status(true),
            service.status(false)
        );
        assert!(!a.unwrap().online && !b.unwrap().online && !c.unwrap().online);
        service.status(false).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        stop.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn authentication_failure_pauses_upstream_until_manual_retry_or_configuration_change() {
        let (_directory, store, service, calls, _gate, stop, task) =
            fixture(403, Duration::ZERO).await;
        assert!(matches!(service.status(false).await, Err(QqError::Auth)));
        for _ in 0..3 {
            assert!(matches!(service.status(false).await, Err(QqError::Auth)));
        }
        assert!(matches!(service.qrcode().await, Err(QqError::Auth)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(service.status(true).await, Err(QqError::Auth)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let mut view = store.snapshot().await.unwrap();
        view["config"]["qq_login"]["url"] = json!(format!(
            "{}/changed",
            view["config"]["qq_login"]["url"].as_str().unwrap()
        ));
        let update=serde_json::from_value(json!({"revision":view["revision"],"config":view["config"],"secrets":{"llbot_token":{"action":"set","value":"new-password"}}})).unwrap();
        store.save(update).await.unwrap();
        assert!(matches!(service.status(false).await, Err(QqError::Auth)));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        stop.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn config_change_then_revert_still_discards_old_inflight_status() {
        let (_directory, store, service, calls, gate, stop, task) =
            fixture(200, Duration::ZERO).await;
        let hold_reply = gate.acquire().await.unwrap();
        let poller = {
            let service = service.clone();
            tokio::spawn(async move { service.status(false).await })
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let original = std::fs::read(store.path()).unwrap();
        let mut changed: Config = Config::load_raw(store.path()).unwrap();
        changed.qq_login.url.push_str("/temporary");
        std::fs::write(store.path(), toml::to_string(&changed).unwrap()).unwrap();
        store.reload().await.unwrap();
        std::fs::write(store.path(), original).unwrap();
        store.reload().await.unwrap();
        drop(hold_reply);
        assert!(matches!(poller.await.unwrap(), Err(QqError::Stale)));
        service.status(false).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        stop.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn qr_is_single_flight_and_configuration_errors_do_not_contact_upstream() {
        let (_directory, store, service, calls, gate, stop, task) =
            fixture(200, Duration::ZERO).await;
        let hold_reply = gate.acquire().await.unwrap();
        let first = {
            let service = service.clone();
            tokio::spawn(async move { service.qrcode().await })
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            while calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(service.qrcode().await, Err(QqError::Busy)));
        drop(hold_reply);
        assert!(matches!(first.await.unwrap(), Err(QqError::Transient)));
        let mut view = store.snapshot().await.unwrap();
        view["config"]["qq_login"]["enabled"] = json!(false);
        store
            .save(
                serde_json::from_value(
                    json!({"revision":view["revision"],"config":view["config"]}),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(service.status(false).await, Err(QqError::Config)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        stop.cancel();
        task.await.unwrap();
    }
}
