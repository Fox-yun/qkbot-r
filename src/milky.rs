//! Small Milky HTTP/WebSocket adapter. A missing send acknowledgement is never retried here.
use std::{
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;

use crate::types::{Incoming, MessageKey, Scene, Scope, now};

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MilkyConfig {
    pub url: String,
    pub access_token: Option<String>,
    pub bot_id: Option<i64>,
    pub max_response_bytes: usize,
    pub request_timeout_secs: u64,
}

impl std::fmt::Debug for MilkyConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MilkyConfig")
            .field("url", &self.url)
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "[redacted]"),
            )
            .field("bot_id", &self.bot_id)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("request_timeout_secs", &self.request_timeout_secs)
            .finish()
    }
}

impl Default for MilkyConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:3000".into(),
            access_token: None,
            bot_id: None,
            max_response_bytes: 256 * 1024,
            request_timeout_secs: 15,
        }
    }
}

#[derive(Clone)]
pub struct Milky {
    config: MilkyConfig,
    http: reqwest::Client,
    base: reqwest::Url,
    logged_in: Arc<AtomicI64>,
    // Even generations are unverified; odd generations are verified. A single
    // atomic snapshot prevents old requests from surviving invalidate/relogin.
    verification: Arc<AtomicU64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    Confirmed {
        message_seq: i64,
    },
    /// No HTTP request was submitted; the same account can recover after login.
    Deferred {
        reason: String,
    },
    Failed {
        reason: String,
    },
    Unknown {
        reason: String,
    },
}

enum EventAction {
    Incoming(Incoming),
    Ignored,
    Reauthenticate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MilkyContact {
    pub id: i64,
    pub name: String,
    pub remark: String,
}

#[derive(Debug, Clone)]
pub struct MilkyContacts {
    pub bot_id: i64,
    pub groups: Vec<MilkyContact>,
    pub friends: Vec<MilkyContact>,
}

#[derive(Debug)]
pub struct MilkyAccountChanged;
impl std::fmt::Display for MilkyAccountChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Milky account changed while reading contacts")
    }
}
impl std::error::Error for MilkyAccountChanged {}

impl Milky {
    pub fn new(config: MilkyConfig, http: reqwest::Client) -> Result<Self> {
        let mut base = reqwest::Url::parse(&config.url).context("invalid Milky URL")?;
        ensure!(
            matches!(base.scheme(), "http" | "https"),
            "Milky URL must use http or https"
        );
        ensure!(
            base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none(),
            "Milky URL must not contain credentials, query or fragment"
        );
        ensure!(
            (1024..=4 * 1024 * 1024).contains(&config.max_response_bytes),
            "Milky body limit must be 1 KiB..4 MiB"
        );
        ensure!(
            (1..=120).contains(&config.request_timeout_secs),
            "Milky timeout must be 1..120 seconds"
        );
        ensure!(
            config.bot_id.is_none_or(|id| id > 0),
            "Milky bot_id must be positive"
        );
        if let Some(token) = &config.access_token {
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .context("invalid Milky access token")?;
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self {
            config,
            http,
            base,
            logged_in: Arc::new(AtomicI64::new(0)),
            verification: Arc::new(AtomicU64::new(0)),
        })
    }

    fn request(&self, api: &str, body: Value) -> Result<reqwest::RequestBuilder> {
        let url = self.base.join(&format!("api/{api}"))?;
        let mut request = self
            .http
            .post(url)
            .timeout(Duration::from_secs(self.config.request_timeout_secs))
            .json(&body);
        if let Some(token) = &self.config.access_token {
            request = request.bearer_auth(token);
        }
        Ok(request)
    }

    async fn call(&self, api: &str, body: Value) -> Result<Value> {
        let response = self
            .request(api, body)?
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Milky transport failed"))?;
        ensure!(
            response.status().is_success(),
            "Milky HTTP status {}",
            response.status().as_u16()
        );
        let body = read_bounded(response, self.config.max_response_bytes).await?;
        let response: Value = serde_json::from_slice(&body).context("invalid Milky JSON")?;
        check_envelope(&response)?;
        Ok(response["data"].clone())
    }

    pub async fn login(&self) -> Result<i64> {
        let generation = self.invalidate_account();
        let id = self.current_account().await?;
        ensure!(
            self.verification.load(Ordering::Acquire) == generation,
            "Milky login verification expired"
        );
        match self
            .logged_in
            .compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {}
            Err(previous) => {
                ensure!(previous == id, "Milky login account changed during runtime")
            }
        }
        ensure!(
            self.verification
                .compare_exchange(
                    generation,
                    generation.wrapping_add(1),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok(),
            "Milky login verification expired"
        );
        Ok(id)
    }

    /// Read-only identity probe: administration must not change the event
    /// transport's account verification generation just to refresh a list.
    pub async fn current_account(&self) -> Result<i64> {
        let data = self.call("get_login_info", json!({})).await?;
        let id = data["uin"]
            .as_i64()
            .filter(|id| *id > 0)
            .context("Milky login missing uin")?;
        ensure!(
            self.config.bot_id.is_none_or(|expected| expected == id),
            "Milky login account differs from configured bot_id"
        );
        Ok(id)
    }

    pub async fn verify_account(&self, expected: i64) -> Result<()> {
        if expected <= 0 || self.current_account().await? != expected {
            return Err(MilkyAccountChanged.into());
        }
        Ok(())
    }

    /// Fetch real membership, bracketed by fresh account probes. Names and
    /// remarks are data only; malformed IDs never become an authorization key.
    pub async fn contacts(&self, no_cache: bool) -> Result<MilkyContacts> {
        let bot_id = self.current_account().await?;
        let (groups, friends) = tokio::try_join!(
            self.call("get_group_list", json!({"no_cache": no_cache})),
            self.call("get_friend_list", json!({"no_cache": no_cache})),
        )?;
        let groups = parse_contacts(&groups, "groups", "group_id", "group_name")?;
        let friends = parse_contacts(&friends, "friends", "user_id", "nickname")?;
        self.verify_account(bot_id).await?;
        Ok(MilkyContacts {
            bot_id,
            groups,
            friends,
        })
    }

    fn invalidate_account(&self) -> u64 {
        let previous = self
            .verification
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                Some(value.wrapping_add(2) & !1)
            })
            .expect("verification generation update always succeeds");
        previous.wrapping_add(2) & !1
    }

    pub fn is_verified(&self) -> bool {
        self.verification.load(Ordering::Acquire) & 1 == 1
    }

    pub async fn get_message(&self, key: &MessageKey) -> Result<Incoming> {
        let generation = self.verification.load(Ordering::Acquire);
        ensure!(
            generation & 1 == 1 && self.logged_in.load(Ordering::Acquire) == key.scope.bot_id,
            "Milky account is not verified for this message"
        );
        let data = self
            .call(
                "get_message",
                json!({"message_scene": key.scope.scene.as_str(),
            "peer_id": key.scope.peer_id, "message_seq": key.message_seq}),
            )
            .await?;
        ensure!(
            self.verification.load(Ordering::Acquire) == generation,
            "Milky account verification expired during quote lookup"
        );
        let message = normalize_message(key.scope.bot_id, &data["message"], now())?;
        ensure!(
            message.key == *key,
            "Milky quote response does not match requested message key"
        );
        Ok(message)
    }

    pub async fn send_text(&self, scope: &Scope, text: &str) -> SendOutcome {
        if scope.bot_id <= 0
            || self.logged_in.load(Ordering::Acquire) != scope.bot_id
            || scope.peer_id <= 0
        {
            return SendOutcome::Failed {
                reason: "Milky account or destination is not verified".into(),
            };
        }
        if !self.is_verified() {
            return SendOutcome::Deferred {
                reason: "Milky account is awaiting login verification; nothing was sent".into(),
            };
        }
        if text.is_empty() || text.len() > 32 * 1024 {
            return SendOutcome::Failed {
                reason: "outgoing text must be 1..32768 bytes".into(),
            };
        }
        let (api, destination) = match scope.scene {
            Scene::Group => ("send_group_message", "group_id"),
            Scene::Friend => ("send_private_message", "user_id"),
        };
        let mut body = json!({"message": [{"type": "text", "data": {"text": text}}]});
        body[destination] = json!(scope.peer_id);
        let request = match self.request(api, body) {
            Ok(request) => request,
            Err(_) => {
                return SendOutcome::Failed {
                    reason: "could not prepare Milky request".into(),
                };
            }
        };
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) if error.is_connect() || error.is_builder() => {
                return SendOutcome::Failed {
                    reason: "Milky connection could not be established".into(),
                };
            }
            Err(_) => {
                return SendOutcome::Unknown {
                    reason: "Milky send acknowledgement was lost".into(),
                };
            }
        };
        let status = response.status();
        if matches!(status.as_u16(), 400 | 401 | 403 | 404 | 405 | 415 | 429) {
            return SendOutcome::Failed {
                reason: format!("Milky rejected request (HTTP {})", status.as_u16()),
            };
        }
        if !status.is_success() {
            return SendOutcome::Unknown {
                reason: format!("Milky send returned HTTP {}", status.as_u16()),
            };
        }
        let bytes = match read_bounded(response, self.config.max_response_bytes).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return SendOutcome::Unknown {
                    reason: "Milky acknowledgement was incomplete or oversized".into(),
                };
            }
        };
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => {
                return SendOutcome::Unknown {
                    reason: "Milky acknowledgement was invalid JSON".into(),
                };
            }
        };
        if value["status"] == "failed" && value["retcode"].as_i64().is_some_and(|code| code != 0) {
            return SendOutcome::Failed {
                reason: format!("Milky rejected send (retcode {})", value["retcode"]),
            };
        }
        if check_envelope(&value).is_ok()
            && let Some(message_seq) = value["data"]["message_seq"]
                .as_i64()
                .filter(|seq| *seq >= 0)
        {
            return SendOutcome::Confirmed { message_seq };
        }
        SendOutcome::Unknown {
            reason: "Milky acknowledgement omitted a valid message sequence".into(),
        }
    }

    /// Bounded ingress: the host owns deduplication and command prioritization.
    pub async fn events(
        &self,
        bot_id: i64,
        sender: mpsc::Sender<Incoming>,
        cancel: CancellationToken,
    ) {
        let mut backoff = 1_u64;
        while !cancel.is_cancelled() && !sender.is_closed() {
            let started = std::time::Instant::now();
            let result = self.event_connection(bot_id, &sender, &cancel).await;
            // Without an event stream we cannot observe logout or account changes.
            // Invalidate before backoff; only a fresh successful login can recover.
            self.invalidate_account();
            if cancel.is_cancelled() || sender.is_closed() {
                break;
            }
            if result.is_err() {
                tracing::warn!("Milky event connection ended; reconnecting");
            }
            if started.elapsed() > Duration::from_secs(60) {
                backoff = 1;
            }
            tokio::select! { _ = cancel.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(backoff)) => {} }
            backoff = (backoff * 2).min(30);
        }
    }

    async fn event_connection(
        &self,
        bot_id: i64,
        sender: &mpsc::Sender<Incoming>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let account = tokio::select! { _ = cancel.cancelled() => return Ok(()), result = self.login() => result? };
        ensure!(account == bot_id, "Milky event account mismatch");
        let mut url = self.base.join("event")?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| anyhow::anyhow!("invalid WebSocket URL"))?;
        let mut request = url.as_str().into_client_request()?;
        if let Some(token) = &self.config.access_token {
            request
                .headers_mut()
                .insert("Authorization", format!("Bearer {token}").parse()?);
        }
        let config = WebSocketConfig::default()
            .max_message_size(Some(self.config.max_response_bytes))
            .max_frame_size(Some(self.config.max_response_bytes));
        let connect = tokio::time::timeout(
            Duration::from_secs(self.config.request_timeout_secs),
            connect_async_with_config(request, Some(config), true),
        );
        let (mut socket, _) = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = connect => result.context("Milky WebSocket connect timeout")?.map_err(|_| anyhow::anyhow!("Milky WebSocket connect failed"))?,
        };
        let mut ping = tokio::time::interval(Duration::from_secs(30));
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_received = std::time::Instant::now();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = ping.tick() => {
                    ensure!(last_received.elapsed() < Duration::from_secs(90), "Milky WebSocket heartbeat timed out");
                    tokio::time::timeout(Duration::from_secs(5), socket.send(Message::Ping(Vec::new().into()))).await??;
                }
                message = socket.next() => {
                    let message = message.context("Milky event stream closed")??;
                    last_received = std::time::Instant::now();
                    let raw = match message {
                        Message::Text(raw) => raw.as_bytes().to_vec(),
                        Message::Binary(raw) => raw.to_vec(),
                        Message::Ping(payload) => {
                            tokio::time::timeout(Duration::from_secs(5), socket.send(Message::Pong(payload))).await??;
                            continue;
                        }
                        Message::Close(_) => return Ok(()),
                        _ => continue,
                    };
                    match self.observe_event(bot_id, &raw, now()) {
                        Ok(EventAction::Incoming(incoming)) => match sender.try_send(incoming) {
                            Ok(()) => {},
                            Err(mpsc::error::TrySendError::Full(_)) => tracing::warn!("Milky ingress full; event dropped"),
                            Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                        },
                        Ok(EventAction::Ignored) => {},
                        Ok(EventAction::Reauthenticate) => bail!("Milky event requires account verification"),
                        Err(_) => tracing::warn!("Milky ignored invalid message event"),
                    }
                }
            }
        }
    }

    fn observe_event(&self, bot_id: i64, raw: &[u8], received_at: i64) -> Result<EventAction> {
        let value: Value = serde_json::from_slice(raw).context("invalid event JSON")?;
        let account_changed = value["self_id"].as_i64().is_some_and(|id| id != bot_id);
        let offline =
            value["self_id"].as_i64() == Some(bot_id) && value["event_type"] == "bot_offline";
        if account_changed || offline {
            self.invalidate_account();
            return Ok(EventAction::Reauthenticate);
        }
        match normalize_event_value(bot_id, &value, received_at)? {
            Some(incoming) => Ok(EventAction::Incoming(incoming)),
            None => Ok(EventAction::Ignored),
        }
    }
}

fn check_envelope(value: &Value) -> Result<()> {
    ensure!(
        value["status"] == "ok"
            && value["retcode"].as_i64() == Some(0)
            && value["data"].is_object(),
        "Milky API returned failure or malformed envelope"
    );
    Ok(())
}

pub(crate) async fn read_bounded(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= limit as u64),
        "HTTP response exceeds configured limit"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("HTTP response body could not be read"))?
    {
        ensure!(
            chunk.len() <= limit.saturating_sub(bytes.len()),
            "HTTP response exceeds configured limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn parse_contacts(
    data: &Value,
    field: &str,
    id_key: &str,
    name_key: &str,
) -> Result<Vec<MilkyContact>> {
    let entries = data[field]
        .as_array()
        .context("Milky contacts list missing")?;
    let mut ids = std::collections::HashSet::new();
    entries
        .iter()
        .map(|entry| {
            let id = entry[id_key]
                .as_i64()
                .filter(|id| *id > 0)
                .context("Milky contact ID invalid")?;
            ensure!(ids.insert(id), "Milky contacts contain duplicate IDs");
            Ok(MilkyContact {
                id,
                name: entry[name_key]
                    .as_str()
                    .context("Milky contact name missing")?
                    .to_owned(),
                remark: entry["remark"]
                    .as_str()
                    .context("Milky contact remark missing")?
                    .to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn normalize_event(
    bot_id: i64,
    raw: &[u8],
    received_at: i64,
) -> Result<Option<Incoming>> {
    let value: Value = serde_json::from_slice(raw).context("invalid event JSON")?;
    normalize_event_value(bot_id, &value, received_at)
}

fn normalize_event_value(bot_id: i64, value: &Value, received_at: i64) -> Result<Option<Incoming>> {
    ensure!(
        value["self_id"].as_i64() == Some(bot_id),
        "event self_id mismatch"
    );
    if value["event_type"] != "message_receive" || value["data"]["message_scene"] == "temp" {
        return Ok(None);
    }
    Ok(Some(normalize_message(
        bot_id,
        &value["data"],
        received_at,
    )?))
}

fn normalize_message(bot_id: i64, value: &Value, received_at: i64) -> Result<Incoming> {
    let scene = match value["message_scene"].as_str() {
        Some("group") => Scene::Group,
        Some("friend") => Scene::Friend,
        _ => bail!("unsupported Milky message scene"),
    };
    let peer_id = value["peer_id"]
        .as_i64()
        .filter(|id| *id > 0)
        .context("invalid peer_id")?;
    let sender_id = value["sender_id"]
        .as_i64()
        .filter(|id| *id > 0)
        .context("invalid sender_id")?;
    let message_seq = value["message_seq"]
        .as_i64()
        .filter(|seq| *seq >= 0)
        .context("invalid message_seq")?;
    let segments = value["segments"]
        .as_array()
        .context("missing message segments")?;
    ensure!(segments.len() <= 512, "too many message segments");
    let (mut text, mut mentions, mut quote_seq) = (String::new(), Vec::new(), None);
    for segment in segments {
        match segment["type"].as_str() {
            Some("text") => text.push_str(
                segment["data"]["text"]
                    .as_str()
                    .context("invalid text segment")?,
            ),
            Some("mention") => {
                let id = segment["data"]["user_id"]
                    .as_i64()
                    .filter(|id| *id > 0)
                    .context("invalid mention segment")?;
                if !mentions.contains(&id) {
                    mentions.push(id);
                }
                // Preserve addressees for context without turning plain text into a mention.
                if id != bot_id {
                    text.push_str(&format!(" @{} ", id));
                }
            }
            Some("reply") if quote_seq.is_none() => {
                quote_seq = segment["data"]["message_seq"]
                    .as_i64()
                    .filter(|seq| *seq >= 0);
            }
            _ => {}
        }
        ensure!(text.len() <= 32 * 1024, "incoming text exceeds limit");
    }
    Ok(Incoming {
        key: MessageKey {
            scope: Scope {
                bot_id,
                scene,
                peer_id,
            },
            message_seq,
        },
        sender_id,
        text,
        mentions,
        quote_seq,
        received_at,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub(crate) async fn server(
        status: &str,
        body: &str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let task = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let length = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
                if length == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..length]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let size: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + size {
                        break;
                    }
                }
            }
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (url, task)
    }

    fn event(segments: Value) -> Value {
        json!({"self_id": 123, "event_type": "message_receive", "data": {"message_scene": "group", "peer_id": 456,
            "sender_id": 789, "message_seq": 10, "segments": segments}})
    }

    #[test]
    fn contact_lists_use_real_milky_fields_and_preserve_large_identifiers() {
        let groups = parse_contacts(
            &json!({"groups":[{"group_id":9007199254740993_i64,
            "group_name":"群名称","remark":"群备注"}]}),
            "groups",
            "group_id",
            "group_name",
        )
        .unwrap();
        assert_eq!(
            groups,
            vec![MilkyContact {
                id: 9_007_199_254_740_993,
                name: "群名称".into(),
                remark: "群备注".into()
            }]
        );
        let friends = parse_contacts(
            &json!({"friends":[{"user_id":789,"nickname":"好友名称","remark":""}]}),
            "friends",
            "user_id",
            "nickname",
        )
        .unwrap();
        assert_eq!(friends[0].id, 789);
        assert_eq!(friends[0].name, "好友名称");
        assert!(friends[0].remark.is_empty());
    }

    #[test]
    fn malformed_or_duplicate_contact_identifiers_cannot_authorize_a_scope() {
        for data in [
            json!({}),
            json!({"groups":null}),
            json!({"groups":[{"group_id":"123","group_name":"name","remark":""}]}),
            json!({"groups":[{"group_id":0,"group_name":"name","remark":""}]}),
            json!({"groups":[{"group_id":123,"remark":""}]}),
            json!({"groups":[{"group_id":123,"group_name":"name"}]}),
            json!({"groups":[{"group_id":123,"group_name":"one","remark":""},{"group_id":123,"group_name":"two","remark":""}]}),
        ] {
            assert!(parse_contacts(&data, "groups", "group_id", "group_name").is_err());
        }
    }

    #[tokio::test]
    async fn administrative_identity_probe_does_not_invalidate_event_verification() {
        let (url, server) = server(
            "200 OK",
            r#"{"status":"ok","retcode":0,"data":{"uin":123}}"#,
        )
        .await;
        let milky = Milky::new(
            MilkyConfig {
                url,
                ..Default::default()
            },
            reqwest::Client::new(),
        )
        .unwrap();
        milky.logged_in.store(123, Ordering::Release);
        milky.verification.store(7, Ordering::Release);
        milky.verify_account(123).await.unwrap();
        assert_eq!(milky.verification.load(Ordering::Acquire), 7);
        assert!(milky.is_verified());
        server.await.unwrap();
    }

    #[test]
    fn only_actual_top_level_mentions_trigger() {
        let value = event(json!([
            {"type":"text","data":{"text":"@123 hello"}},
            {"type":"reply","data":{"message_seq":7,"segments":[{"type":"mention","data":{"user_id":123}}]}}
        ]));
        let incoming = normalize_event(123, &serde_json::to_vec(&value).unwrap(), 99)
            .unwrap()
            .unwrap();
        assert!(incoming.mentions.is_empty());
        assert_eq!(incoming.quote_seq, Some(7));
        assert_eq!(incoming.received_at, 99);
        let value = event(
            json!([{"type":"mention","data":{"user_id":123}},{"type":"text","data":{"text":"hello"}}]),
        );
        assert_eq!(
            normalize_event(123, &serde_json::to_vec(&value).unwrap(), 99)
                .unwrap()
                .unwrap()
                .mentions,
            vec![123]
        );
    }

    #[test]
    fn reject_cross_account_and_temp_messages() {
        let mut value = event(json!([]));
        assert!(normalize_event(999, &serde_json::to_vec(&value).unwrap(), 1).is_err());
        value["data"]["message_scene"] = json!("temp");
        assert!(
            normalize_event(123, &serde_json::to_vec(&value).unwrap(), 1)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn login_uses_uin_and_per_request_authorization() {
        let (url, server) = server(
            "200 OK",
            r#"{"status":"ok","retcode":0,"data":{"uin":123}}"#,
        )
        .await;
        let milky = Milky::new(
            MilkyConfig {
                url,
                access_token: Some("milky-only".into()),
                bot_id: Some(123),
                ..Default::default()
            },
            reqwest::Client::new(),
        )
        .unwrap();
        assert_eq!(milky.login().await.unwrap(), 123);
        let request = server.await.unwrap().to_lowercase();
        assert!(request.starts_with("post /api/get_login_info"));
        assert!(request.contains("authorization: bearer milky-only"));
    }

    #[tokio::test]
    async fn http_200_failure_is_not_success() {
        let (url, server) = server(
            "200 OK",
            r#"{"status":"failed","retcode":-403,"message":"offline"}"#,
        )
        .await;
        let milky = Milky::new(
            MilkyConfig {
                url,
                ..Default::default()
            },
            reqwest::Client::new(),
        )
        .unwrap();
        assert!(milky.login().await.is_err());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn missing_send_acknowledgement_is_unknown() {
        let (url, server) = server("200 OK", r#"{"status":"ok","retcode":0,"data":{}}"#).await;
        let milky = Milky::new(
            MilkyConfig {
                url,
                ..Default::default()
            },
            reqwest::Client::new(),
        )
        .unwrap();
        milky.logged_in.store(123, Ordering::Release);
        milky.verification.store(1, Ordering::Release);
        assert!(matches!(
            milky
                .send_text(
                    &Scope {
                        bot_id: 123,
                        scene: Scene::Group,
                        peer_id: 456
                    },
                    "hello"
                )
                .await,
            SendOutcome::Unknown { .. }
        ));
        let request = server.await.unwrap();
        assert!(request.contains("\"group_id\":456"));
        assert!(request.contains("\"data\":{\"text\":\"hello\"}"));
    }

    #[tokio::test]
    async fn offline_or_changed_account_blocks_sends_and_quotes_until_verified() {
        for event in [
            json!({"self_id":123,"event_type":"bot_offline","data":{"reason":"logged out"}}),
            json!({"self_id":999,"event_type":"message_receive","data":{}}),
        ] {
            let milky = Milky::new(MilkyConfig::default(), reqwest::Client::new()).unwrap();
            milky.logged_in.store(123, Ordering::Release);
            milky.verification.store(1, Ordering::Release);
            assert!(matches!(
                milky
                    .observe_event(123, &serde_json::to_vec(&event).unwrap(), now())
                    .unwrap(),
                EventAction::Reauthenticate
            ));
            assert!(!milky.is_verified());
            assert_eq!(milky.logged_in.load(Ordering::Acquire), 123);
            let scope = Scope {
                bot_id: 123,
                scene: Scene::Group,
                peer_id: 456,
            };
            assert!(matches!(
                milky.send_text(&scope, "must stay local").await,
                SendOutcome::Deferred { .. }
            ));
            let error = milky
                .get_message(&MessageKey {
                    scope,
                    message_seq: 7,
                })
                .await
                .unwrap_err();
            assert!(error.to_string().contains("not verified"));
            let wrong_account = Scope {
                bot_id: 999,
                scene: Scene::Group,
                peer_id: 456,
            };
            assert!(matches!(
                milky.send_text(&wrong_account, "wrong account").await,
                SendOutcome::Failed { .. }
            ));
        }
    }

    #[test]
    fn malformed_or_unrelated_events_do_not_revoke_account_verification() {
        let milky = Milky::new(MilkyConfig::default(), reqwest::Client::new()).unwrap();
        milky.logged_in.store(123, Ordering::Release);
        milky.verification.store(1, Ordering::Release);
        assert!(milky.observe_event(123, b"not json", now()).is_err());
        assert!(
            milky
                .observe_event(123, br#"{"event_type":"unknown"}"#, now())
                .is_err()
        );
        assert!(matches!(
            milky
                .observe_event(123, br#"{"self_id":123,"event_type":"unknown"}"#, now())
                .unwrap(),
            EventAction::Ignored
        ));
        assert!(milky.is_verified());
    }

    #[tokio::test]
    async fn failed_relogin_cannot_rebind_runtime_account_and_original_account_recovers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for body in [
                r#"{"status":"ok","retcode":0,"data":{"uin":123}}"#,
                r#"{"status":"ok","retcode":0,"data":{"uin":999}}"#,
                r#"{"status":"failed","retcode":-1,"message":"offline"}"#,
                r#"{"status":"ok","retcode":0,"data":{"uin":123}}"#,
            ] {
                let (mut stream, _) =
                    tokio::time::timeout(Duration::from_secs(5), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut request = [0; 4096];
                tokio::time::timeout(Duration::from_secs(5), stream.read(&mut request))
                    .await
                    .unwrap()
                    .unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let milky = Milky::new(
            MilkyConfig {
                url,
                ..Default::default()
            },
            reqwest::Client::new(),
        )
        .unwrap();
        assert_eq!(milky.login().await.unwrap(), 123);
        assert!(milky.is_verified());
        milky
            .observe_event(
                123,
                br#"{"self_id":123,"event_type":"bot_offline","data":{}}"#,
                now(),
            )
            .unwrap();
        assert!(!milky.is_verified());
        assert!(
            milky
                .login()
                .await
                .unwrap_err()
                .to_string()
                .contains("changed during runtime")
        );
        assert_eq!(milky.logged_in.load(Ordering::Acquire), 123);
        assert!(!milky.is_verified());
        assert!(milky.login().await.is_err());
        assert!(!milky.is_verified());
        assert_eq!(milky.login().await.unwrap(), 123);
        assert!(milky.is_verified());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn quote_response_is_discarded_if_account_becomes_unverified_in_flight() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (requested, arrived) = tokio::sync::oneshot::channel();
        let (release, proceed) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = [0; 4096];
            tokio::time::timeout(Duration::from_secs(5), stream.read(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            requested.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(5), proceed)
                .await
                .unwrap()
                .unwrap();
            let body = json!({"status":"ok","retcode":0,"data":{"message":{
                "message_scene":"group","peer_id":456,"message_seq":7,"sender_id":789,"segments":[]
            }}})
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let milky = Milky::new(
            MilkyConfig {
                url,
                ..Default::default()
            },
            reqwest::Client::new(),
        )
        .unwrap();
        milky.logged_in.store(123, Ordering::Release);
        milky.verification.store(1, Ordering::Release);
        let reader = milky.clone();
        let request = tokio::spawn(async move {
            reader
                .get_message(&MessageKey {
                    scope: Scope {
                        bot_id: 123,
                        scene: Scene::Group,
                        peer_id: 456,
                    },
                    message_seq: 7,
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        milky
            .observe_event(
                123,
                br#"{"self_id":999,"event_type":"message_receive","data":{}}"#,
                now(),
            )
            .unwrap();
        release.send(()).unwrap();
        let error = request.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("verification expired"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn quote_response_is_discarded_after_offline_and_successful_relogin() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (requested, arrived) = tokio::sync::oneshot::channel();
        let (release, proceed) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut quote, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = [0; 4096];
            let count = tokio::time::timeout(Duration::from_secs(5), quote.read(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            assert!(String::from_utf8_lossy(&bytes[..count]).starts_with("POST /api/get_message "));
            requested.send(()).unwrap();

            let (mut login, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let count = tokio::time::timeout(Duration::from_secs(5), login.read(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            assert!(
                String::from_utf8_lossy(&bytes[..count]).starts_with("POST /api/get_login_info ")
            );
            let body = r#"{"status":"ok","retcode":0,"data":{"uin":123}}"#;
            login
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            drop(login);

            tokio::time::timeout(Duration::from_secs(5), proceed)
                .await
                .unwrap()
                .unwrap();
            let body = json!({"status":"ok","retcode":0,"data":{"message":{
                "message_scene":"group","peer_id":456,"message_seq":7,"sender_id":789,"segments":[]
            }}})
            .to_string();
            quote
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let milky = Milky::new(
            MilkyConfig {
                url,
                ..Default::default()
            },
            reqwest::Client::new(),
        )
        .unwrap();
        milky.logged_in.store(123, Ordering::Release);
        milky.verification.store(1, Ordering::Release);
        let reader = milky.clone();
        let quote = tokio::spawn(async move {
            reader
                .get_message(&MessageKey {
                    scope: Scope {
                        bot_id: 123,
                        scene: Scene::Group,
                        peer_id: 456,
                    },
                    message_seq: 7,
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        milky
            .observe_event(
                123,
                br#"{"self_id":123,"event_type":"bot_offline","data":{}}"#,
                now(),
            )
            .unwrap();
        assert!(!milky.is_verified());
        assert_eq!(milky.login().await.unwrap(), 123);
        assert!(milky.is_verified());
        assert!(milky.verification.load(Ordering::Acquire) > 1);
        release.send(()).unwrap();
        assert!(
            quote
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("verification expired")
        );
        assert!(milky.is_verified());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn login_response_cannot_restore_a_generation_invalidated_in_flight() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (requested, arrived) = tokio::sync::oneshot::channel();
        let (release, proceed) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = [0; 4096];
            tokio::time::timeout(Duration::from_secs(5), stream.read(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            requested.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(5), proceed)
                .await
                .unwrap()
                .unwrap();
            let body = r#"{"status":"ok","retcode":0,"data":{"uin":123}}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let milky = Milky::new(
            MilkyConfig {
                url,
                ..Default::default()
            },
            reqwest::Client::new(),
        )
        .unwrap();
        milky.logged_in.store(123, Ordering::Release);
        milky.verification.store(1, Ordering::Release);
        let verifier = milky.clone();
        let login = tokio::spawn(async move { verifier.login().await });
        tokio::time::timeout(Duration::from_secs(5), arrived)
            .await
            .unwrap()
            .unwrap();
        milky
            .observe_event(
                123,
                br#"{"self_id":123,"event_type":"bot_offline","data":{}}"#,
                now(),
            )
            .unwrap();
        release.send(()).unwrap();
        assert!(
            login
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("verification expired")
        );
        assert!(!milky.is_verified());
        assert_eq!(milky.logged_in.load(Ordering::Acquire), 123);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn response_size_is_bounded() {
        let (url, server) = server("200 OK", &"x".repeat(2048)).await;
        let milky = Milky::new(
            MilkyConfig {
                url,
                max_response_bytes: 1024,
                ..Default::default()
            },
            reqwest::Client::new(),
        )
        .unwrap();
        assert!(
            milky
                .login()
                .await
                .unwrap_err()
                .to_string()
                .contains("limit")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn chunked_response_is_bounded_without_content_length() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut request = [0; 4096];
            tokio::time::timeout(Duration::from_secs(5), stream.read(&mut request))
                .await
                .unwrap()
                .unwrap();
            let body = "x".repeat(2048);
            let response = format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n800\r\n{body}\r\n0\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let milky = Milky::new(
            MilkyConfig {
                url,
                max_response_bytes: 1024,
                ..Default::default()
            },
            reqwest::Client::new(),
        )
        .unwrap();
        assert!(
            milky
                .login()
                .await
                .unwrap_err()
                .to_string()
                .contains("limit")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    // tungstenite's handshake callback requires its Response<String> error type.
    #[allow(clippy::result_large_err)]
    async fn websocket_auth_events_ping_and_shutdown_work_together() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let fixture = event(json!([{"type":"text","data":{"text":"from socket"}}])).to_string();
        let server = tokio::spawn(async move {
            let (mut login, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut bytes = [0; 4096];
            tokio::time::timeout(Duration::from_secs(5), login.read(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            let body = r#"{"status":"ok","retcode":0,"data":{"uin":123}}"#;
            login
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            drop(login);
            let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(
                stream,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                    assert_eq!(request.uri().path(), "/event");
                    assert_eq!(request.headers()["Authorization"], "Bearer socket-only");
                    Ok(response)
                },
            )
            .await
            .unwrap();
            socket.send(Message::Text(fixture.into())).await.unwrap();
            socket
                .send(Message::Ping(b"check".to_vec().into()))
                .await
                .unwrap();
            loop {
                match socket.next().await.unwrap().unwrap() {
                    Message::Pong(bytes) if bytes.as_ref() == b"check" => break,
                    Message::Ping(bytes) => socket.send(Message::Pong(bytes)).await.unwrap(),
                    _ => {}
                }
            }
        });
        let milky = Milky::new(
            MilkyConfig {
                url,
                access_token: Some("socket-only".into()),
                ..Default::default()
            },
            reqwest::Client::new(),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let child = cancel.clone();
        let (sender, mut receiver) = mpsc::channel(4);
        let client = tokio::spawn(async move {
            milky.events(123, sender, child).await;
        });
        let incoming = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(incoming.text, "from socket");
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(3), client)
            .await
            .unwrap()
            .unwrap();
    }
}
