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
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;

use crate::types::{
    Incoming, MessageKey, NudgeEvent, OutgoingMessage, Scene, Scope, now, sanitize_display_name,
    validate_media_uri,
};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    Confirmed,
    Deferred,
    Failed { reason: String },
    Unknown { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMember {
    pub user_id: i64,
    pub group_id: i64,
    pub nickname: String,
    pub card: String,
    pub role: String,
    /// Bound to user_id/group_id above; bridge reads retain standard Milky fields.
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub enum ApiOutcome {
    Confirmed(Value),
    Deferred,
    Failed,
    Unknown,
}

// Ephemeral parser result moved immediately into the ingress channel. Boxing
// would add an allocation to every message just to shrink the ignored variant.
#[allow(clippy::large_enum_variant)]
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

    pub fn verified_bot_id(&self) -> Option<i64> {
        let generation = self.verification.load(Ordering::Acquire);
        let id = self.logged_in.load(Ordering::Acquire);
        (generation & 1 == 1 && id > 0 && self.verification.load(Ordering::Acquire) == generation)
            .then_some(id)
    }

    pub async fn get_message(&self, key: &MessageKey) -> Result<Incoming> {
        let message = self.get_message_raw(key).await?;
        normalize_message(key.scope.bot_id, &message, now())
    }

    fn scope_generation(&self, scope: &Scope) -> Result<u64> {
        let generation = self.verification.load(Ordering::Acquire);
        ensure!(
            scope.bot_id > 0
                && scope.peer_id > 0
                && generation & 1 == 1
                && self.logged_in.load(Ordering::Acquire) == scope.bot_id,
            "Milky account is not verified for this scope"
        );
        Ok(generation)
    }

    fn check_generation(&self, generation: u64) -> Result<()> {
        ensure!(
            self.verification.load(Ordering::Acquire) == generation,
            "Milky account verification expired during request"
        );
        Ok(())
    }

    pub async fn get_message_raw(&self, key: &MessageKey) -> Result<Value> {
        let generation = self.scope_generation(&key.scope)?;
        ensure!(key.message_seq >= 0, "notification keys cannot be quoted");
        let data = self
            .call(
                "get_message",
                json!({"message_scene": key.scope.scene.as_str(),
            "peer_id": key.scope.peer_id, "message_seq": key.message_seq}),
            )
            .await?;
        self.check_generation(generation)?;
        let message = normalize_message(key.scope.bot_id, &data["message"], now())?;
        ensure!(
            message.key == *key,
            "Milky quote response does not match requested message key"
        );
        Ok(data["message"].clone())
    }

    pub async fn get_group_members(
        &self,
        scope: &Scope,
        no_cache: bool,
    ) -> Result<Vec<GroupMember>> {
        let generation = self.scope_generation(scope)?;
        ensure!(
            scope.scene == Scene::Group,
            "group members require a group scope"
        );
        let data = self
            .call(
                "get_group_member_list",
                json!({"group_id":scope.peer_id,"no_cache":no_cache}),
            )
            .await?;
        self.check_generation(generation)?;
        let entries = data["members"]
            .as_array()
            .context("Milky members list missing")?;
        ensure!(entries.len() <= 10_000, "Milky members list too large");
        let mut ids = std::collections::HashSet::new();
        entries
            .iter()
            .map(|entry| {
                let member = parse_member(entry, scope.peer_id)?;
                ensure!(
                    ids.insert(member.user_id),
                    "Milky members contain duplicate IDs"
                );
                Ok(member)
            })
            .collect()
    }

    pub async fn get_member(
        &self,
        scope: &Scope,
        user_id: i64,
        no_cache: bool,
    ) -> Result<GroupMember> {
        let generation = self.scope_generation(scope)?;
        ensure!(
            scope.scene == Scene::Group && user_id > 0,
            "invalid group member lookup"
        );
        let data = self
            .call(
                "get_group_member_info",
                json!({"group_id":scope.peer_id,"user_id":user_id,"no_cache":no_cache}),
            )
            .await?;
        self.check_generation(generation)?;
        let member = parse_member(&data["member"], scope.peer_id)?;
        ensure!(
            member.user_id == user_id,
            "Milky member response identity mismatch"
        );
        Ok(member)
    }

    /// A resource identifier is accepted only when a verified message in this
    /// scope actually contains it. The returned URL is never fetched here.
    pub async fn get_resource_temp_url(
        &self,
        key: &MessageKey,
        resource_id: &str,
    ) -> Result<String> {
        ensure!(
            !resource_id.is_empty() && resource_id.len() <= 4096,
            "invalid resource ID"
        );
        let generation = self.scope_generation(&key.scope)?;
        let message = self.get_message_raw(key).await?;
        let contains = message["segments"].as_array().is_some_and(|segments| {
            segments.iter().any(|segment| {
                matches!(segment["type"].as_str(), Some("image" | "record" | "video"))
                    && (segment["data"]["resource_id"].as_str() == Some(resource_id)
                        || segment["data"]["thumb_resource_id"].as_str() == Some(resource_id))
            })
        });
        ensure!(
            contains,
            "resource does not belong to the requested message"
        );
        let data = self
            .call("get_resource_temp_url", json!({"resource_id":resource_id}))
            .await?;
        self.check_generation(generation)?;
        let url = data["url"].as_str().context("Milky resource URL missing")?;
        validate_media_uri(url)?;
        Ok(url.to_owned())
    }

    /// Explicit read-only bridge whitelist; arbitrary Milky API names, cookies,
    /// account writes and moderation are intentionally not proxyable.
    pub async fn proxy_read(
        &self,
        scope: Option<&Scope>,
        api: &str,
        params: Value,
    ) -> Result<Value> {
        let bot_id = self
            .verified_bot_id()
            .context("Milky account is not verified")?;
        let generation = self.verification.load(Ordering::Acquire);
        ensure!(generation & 1 == 1, "Milky account is not verified");
        if let Some(scope) = scope {
            ensure!(
                self.scope_generation(scope)? == generation,
                "Milky account verification expired"
            );
        }
        let params = params
            .as_object()
            .context("Milky parameters must be an object")?;
        let body = match api {
            "get_login_info" | "get_impl_info" => {
                ensure!(params.is_empty(), "unexpected Milky parameters");
                json!({})
            }
            "get_group_info" | "get_friend_info" => {
                let scope = scope.context("this lookup requires a scope")?;
                self.scope_generation(scope)?;
                let (scene, id_key) = if api == "get_group_info" {
                    (Scene::Group, "group_id")
                } else {
                    (Scene::Friend, "user_id")
                };
                ensure!(
                    scope.scene == scene
                        && scope.bot_id == bot_id
                        && params.get(id_key).and_then(Value::as_i64) == Some(scope.peer_id)
                        && params.keys().all(|key| key == id_key || key == "no_cache"),
                    "lookup is outside the bound scope"
                );
                let no_cache = match params.get("no_cache") {
                    None => false,
                    Some(value) => value.as_bool().context("invalid no_cache")?,
                };
                let mut body = json!({"no_cache":no_cache});
                body[id_key] = json!(scope.peer_id);
                body
            }
            _ => bail!("Milky API is not in the read-only whitelist"),
        };
        let data = self.call(api, body).await?;
        self.check_generation(generation)?;
        match api {
            "get_login_info" => ensure!(
                data["uin"].as_i64() == Some(bot_id),
                "Milky login identity changed"
            ),
            "get_group_info" => ensure!(
                data["group"]["group_id"].as_i64() == scope.map(|scope| scope.peer_id),
                "Milky group response identity mismatch"
            ),
            "get_friend_info" => ensure!(
                data["friend"]["user_id"].as_i64() == scope.map(|scope| scope.peer_id),
                "Milky friend response identity mismatch"
            ),
            _ => {}
        }
        Ok(data)
    }

    /// Native moderation only. A caller may request their own timed mute;
    /// muting another member requires current group administration privileges.
    pub async fn set_group_member_mute(
        &self,
        scope: &Scope,
        actor_id: i64,
        user_id: i64,
        duration: u32,
        cancel: &CancellationToken,
    ) -> Result<ActionOutcome> {
        let generation = self.scope_generation(scope)?;
        ensure!(
            scope.scene == Scene::Group
                && actor_id > 0
                && user_id > 0
                && user_id != scope.bot_id
                && duration <= 30 * 24 * 3600,
            "invalid group mute request"
        );
        let bot = self.get_member(scope, scope.bot_id, true).await?;
        let target = self.get_member(scope, user_id, true).await?;
        ensure!(
            outranks(&bot.role, &target.role),
            "bot cannot moderate this member"
        );
        if actor_id != user_id {
            let actor = self.get_member(scope, actor_id, true).await?;
            ensure!(
                outranks(&actor.role, &target.role),
                "caller cannot moderate this member"
            );
        } else {
            ensure!(
                target.role == "member",
                "self mute only applies to ordinary members"
            );
        }
        self.check_generation(generation)?;
        self.submit_action(
            "set_group_member_mute",
            json!({"group_id":scope.peer_id,"user_id":user_id,"duration":duration}),
            cancel,
        )
        .await
    }

    /// Explicit native administrator command. The trusted caller derives the
    /// superuser flag from host configuration, never from message text.
    pub async fn set_group_member_mute_as_admin(
        &self,
        scope: &Scope,
        actor_id: i64,
        user_id: i64,
        duration: u32,
        actor_is_superuser: bool,
        cancel: &CancellationToken,
    ) -> Result<ActionOutcome> {
        let generation = self.scope_generation(scope)?;
        ensure!(
            scope.scene == Scene::Group
                && actor_id > 0
                && user_id > 0
                && user_id != scope.bot_id
                && duration <= 30 * 24 * 3600,
            "invalid administrative mute request"
        );
        let bot = self.get_member(scope, scope.bot_id, true).await?;
        let actor = self.get_member(scope, actor_id, true).await?;
        let target = self.get_member(scope, user_id, true).await?;
        ensure!(
            admin_mute_allowed(&bot.role, &actor.role, &target.role, actor_is_superuser),
            "administrator or bot cannot moderate this member"
        );
        self.check_generation(generation)?;
        self.submit_action(
            "set_group_member_mute",
            json!({"group_id":scope.peer_id,"user_id":user_id,"duration":duration}),
            cancel,
        )
        .await
    }

    /// Only the bot's own confirmed messages may be recalled by native helpers.
    pub async fn recall_message(
        &self,
        key: &MessageKey,
        cancel: &CancellationToken,
    ) -> Result<ActionOutcome> {
        let generation = self.scope_generation(&key.scope)?;
        let message = self.get_message(key).await?;
        ensure!(
            message.sender_id == key.scope.bot_id,
            "only bot messages can be recalled"
        );
        self.check_generation(generation)?;
        let (api, field) = match key.scope.scene {
            Scene::Group => ("recall_group_message", "group_id"),
            Scene::Friend => ("recall_private_message", "user_id"),
        };
        let mut body = json!({"message_seq":key.message_seq});
        body[field] = json!(key.scope.peer_id);
        self.submit_action(api, body, cancel).await
    }

    pub async fn send_nudge(
        &self,
        scope: &Scope,
        user_id: i64,
        cancel: &CancellationToken,
    ) -> Result<ActionOutcome> {
        let generation = self.scope_generation(scope)?;
        ensure!(
            user_id > 0 && user_id != scope.bot_id,
            "invalid nudge target"
        );
        let (api, body) = match scope.scene {
            Scene::Group => {
                self.get_member(scope, user_id, true).await?;
                (
                    "send_group_nudge",
                    json!({"group_id":scope.peer_id,"user_id":user_id}),
                )
            }
            Scene::Friend => {
                ensure!(
                    user_id == scope.peer_id,
                    "nudge target is outside friend scope"
                );
                (
                    "send_friend_nudge",
                    json!({"user_id":user_id,"is_self":false}),
                )
            }
        };
        self.check_generation(generation)?;
        self.submit_action(api, body, cancel).await
    }

    /// Narrow bridge writes. Scope and current account are checked here as well
    /// as by the gateway's task, access revision and durable operation ledger.
    pub async fn bridge_action(
        &self,
        scope: &Scope,
        api: &str,
        body: Value,
        cancel: &CancellationToken,
    ) -> Result<ApiOutcome> {
        let generation = self.scope_generation(scope)?;
        match api {
            "send_group_message_reaction" => {
                ensure!(
                    scope.scene == Scene::Group && body["group_id"].as_i64() == Some(scope.peer_id),
                    "reaction scope mismatch"
                );
                ensure!(
                    body["message_seq"].as_i64().is_some_and(|seq| seq > 0),
                    "invalid reaction target"
                );
                ensure!(
                    body["reaction"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty() && s.len() <= 32),
                    "invalid reaction"
                );
                ensure!(
                    ["face", "emoji"].contains(&body["reaction_type"].as_str().unwrap_or("face")),
                    "invalid reaction type"
                );
            }
            "upload_group_file" | "upload_private_file" => {
                let (scene, key) = if api == "upload_group_file" {
                    (Scene::Group, "group_id")
                } else {
                    (Scene::Friend, "user_id")
                };
                ensure!(
                    scope.scene == scene && body[key].as_i64() == Some(scope.peer_id),
                    "file destination mismatch"
                );
                validate_media_uri(body["file_uri"].as_str().context("file URI missing")?)?;
                ensure!(
                    body["file_name"]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty()
                            && s.len() <= 240
                            && !s.contains(['/', '\\'])
                            && !s.chars().any(char::is_control)),
                    "invalid file name"
                );
                ensure!(
                    body.get("parent_folder_id")
                        .is_none_or(|id| id.as_str() == Some("/")),
                    "custom destination folders are not enabled"
                );
            }
            _ => anyhow::bail!("bridge action not allowed"),
        }
        self.check_generation(generation)?;
        self.submit_data(api, body, cancel).await
    }

    async fn submit_action(
        &self,
        api: &str,
        body: Value,
        cancel: &CancellationToken,
    ) -> Result<ActionOutcome> {
        Ok(match self.submit_data(api, body, cancel).await? {
            ApiOutcome::Confirmed(_) => ActionOutcome::Confirmed,
            ApiOutcome::Deferred => ActionOutcome::Deferred,
            ApiOutcome::Failed => ActionOutcome::Failed {
                reason: "Milky rejected action before a successful acknowledgement".into(),
            },
            ApiOutcome::Unknown => ActionOutcome::Unknown {
                reason: "Milky action acknowledgement is unknown".into(),
            },
        })
    }
    async fn submit_data(
        &self,
        api: &str,
        body: Value,
        cancel: &CancellationToken,
    ) -> Result<ApiOutcome> {
        if cancel.is_cancelled() {
            return Ok(ApiOutcome::Deferred);
        }
        let request = self.request(api, body)?;
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(ApiOutcome::Unknown),
            result = request.send() => result,
        };
        let response = match result {
            Ok(response) => response,
            Err(error) if error.is_connect() || error.is_builder() => {
                return Ok(ApiOutcome::Failed);
            }
            Err(_) => {
                return Ok(ApiOutcome::Unknown);
            }
        };
        if matches!(
            response.status().as_u16(),
            400 | 401 | 403 | 404 | 405 | 415 | 429
        ) {
            return Ok(ApiOutcome::Failed);
        }
        if !response.status().is_success() {
            return Ok(ApiOutcome::Unknown);
        }
        let bytes = tokio::select! {
            _ = cancel.cancelled() => return Ok(ApiOutcome::Unknown),
            result = read_bounded(response, self.config.max_response_bytes) => result,
        };
        let value = bytes
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        Ok(match value {
            Some(value) if check_envelope(&value).is_ok() => {
                ApiOutcome::Confirmed(value["data"].clone())
            }
            Some(value)
                if value["status"] == "failed"
                    && value["retcode"].as_i64().is_some_and(|code| code != 0) =>
            {
                ApiOutcome::Failed
            }
            _ => ApiOutcome::Unknown,
        })
    }

    pub async fn send_text(&self, scope: &Scope, text: &str) -> SendOutcome {
        self.send_message(
            scope,
            &OutgoingMessage::text(text),
            &CancellationToken::new(),
        )
        .await
    }

    pub async fn send_message(
        &self,
        scope: &Scope,
        message: &OutgoingMessage,
        cancel: &CancellationToken,
    ) -> SendOutcome {
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
        let generation = self.verification.load(Ordering::Acquire);
        if message.validate(32 * 1024).is_err() {
            return SendOutcome::Failed {
                reason: "outgoing message failed validation".into(),
            };
        }
        if cancel.is_cancelled() {
            return SendOutcome::Deferred {
                reason: "send cancelled before submission".into(),
            };
        }
        let (api, destination) = match scope.scene {
            Scene::Group => ("send_group_message", "group_id"),
            Scene::Friend => ("send_private_message", "user_id"),
        };
        let mut body = json!({"message": message.segments});
        body[destination] = json!(scope.peer_id);
        let request = match self.request(api, body) {
            Ok(request) => request,
            Err(_) => {
                return SendOutcome::Failed {
                    reason: "could not prepare Milky request".into(),
                };
            }
        };
        if self.check_generation(generation).is_err() || generation & 1 == 0 {
            return SendOutcome::Deferred {
                reason: "Milky verification expired before submission".into(),
            };
        }
        let submitted = tokio::select! {
            biased;
            _ = cancel.cancelled() => return SendOutcome::Unknown { reason:"send cancelled after submission may have succeeded".into() },
            result = request.send() => result,
        };
        let response = match submitted {
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
        let received = tokio::select! {
            _ = cancel.cancelled() => return SendOutcome::Unknown { reason:"send acknowledgement cancelled".into() },
            result = read_bounded(response, self.config.max_response_bytes) => result,
        };
        let bytes = match received {
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

fn outranks(actor: &str, target: &str) -> bool {
    matches!(
        (actor, target),
        ("owner", "admin" | "member") | ("admin", "member")
    )
}
fn admin_mute_allowed(bot: &str, actor: &str, target: &str, superuser: bool) -> bool {
    outranks(bot, target) && (superuser || outranks(actor, target))
}

fn parse_member(value: &Value, group_id: i64) -> Result<GroupMember> {
    let user_id = value["user_id"]
        .as_i64()
        .filter(|id| *id > 0)
        .context("invalid group member ID")?;
    ensure!(
        group_id > 0 && value["group_id"].as_i64() == Some(group_id),
        "group member scope mismatch"
    );
    let role = value["role"]
        .as_str()
        .context("missing group member role")?;
    ensure!(
        matches!(role, "owner" | "admin" | "member"),
        "invalid group member role"
    );
    let nickname = value["nickname"]
        .as_str()
        .context("missing group member nickname")?;
    let card = value["card"]
        .as_str()
        .context("missing group member card")?;
    Ok(GroupMember {
        user_id,
        group_id,
        nickname: sanitize_display_name(Some(nickname)).unwrap_or_default(),
        card: sanitize_display_name(Some(card)).unwrap_or_default(),
        role: role.to_owned(),
        raw: value.clone(),
    })
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
                remark: match entry.get("remark") {
                    None => String::new(),
                    Some(value) => value
                        .as_str()
                        .context("Milky contact remark invalid")?
                        .to_owned(),
                },
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
    if matches!(
        value["event_type"].as_str(),
        Some("group_nudge" | "friend_nudge")
    ) {
        return Ok(Some(normalize_nudge(bot_id, value, received_at)?));
    }
    if value["event_type"] != "message_receive" || value["data"]["message_scene"] == "temp" {
        return Ok(None);
    }
    Ok(Some(normalize_message(
        bot_id,
        &value["data"],
        received_at,
    )?))
}

fn normalize_nudge(bot_id: i64, value: &Value, received_at: i64) -> Result<Incoming> {
    let data = &value["data"];
    let positive = |field: &str| {
        data[field]
            .as_i64()
            .filter(|id| *id > 0)
            .context("invalid nudge identity")
    };
    let event_type = value["event_type"]
        .as_str()
        .context("missing nudge event type")?;
    ensure!(
        value["time"].as_i64().is_some_and(|time| time >= 0),
        "invalid nudge time"
    );
    let (scene, peer_id, initiator, target) = match event_type {
        "group_nudge" => (
            Scene::Group,
            positive("group_id")?,
            positive("sender_id")?,
            positive("receiver_id")?,
        ),
        "friend_nudge" => {
            let peer = positive("user_id")?;
            let from_self = data["is_self_send"]
                .as_bool()
                .context("invalid nudge sender flag")?;
            let to_self = data["is_self_receive"]
                .as_bool()
                .context("invalid nudge recipient flag")?;
            (
                Scene::Friend,
                peer,
                if from_self { bot_id } else { peer },
                if to_self { bot_id } else { peer },
            )
        }
        _ => bail!("unsupported notification"),
    };
    // Canonical JSON field order makes retransmission stable. Negative values are
    // internal-only and cannot collide with a valid nonnegative message sequence.
    let hash = Sha256::digest(serde_json::to_vec(value)?);
    let fingerprint =
        i64::from_be_bytes(hash[..8].try_into().expect("eight hash bytes")) & i64::MAX;
    Ok(Incoming {
        key: MessageKey {
            scope: Scope {
                bot_id,
                scene,
                peer_id,
            },
            message_seq: -fingerprint.max(1),
        },
        sender_id: initiator,
        sender_nickname: None,
        sender_card: None,
        text: String::new(),
        mentions: Vec::new(),
        quote_seq: None,
        received_at,
        raw_message: Value::Null,
        notification: Some(NudgeEvent {
            event_type: event_type.to_owned(),
            initiator,
            target,
            raw_event: value.clone(),
        }),
    })
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
    // Milky's member/friend object is metadata for a specific identity, not an
    // alternate source of identity. Missing or mismatched labels degrade to ID.
    let (sender_nickname, sender_card) = match scene {
        Scene::Group => {
            let member = &value["group_member"];
            if member["user_id"].as_i64() == Some(sender_id)
                && member["group_id"].as_i64() == Some(peer_id)
            {
                (
                    sanitize_display_name(member["nickname"].as_str()),
                    sanitize_display_name(member["card"].as_str()),
                )
            } else {
                (None, None)
            }
        }
        Scene::Friend => {
            let friend = &value["friend"];
            if peer_id == sender_id && friend["user_id"].as_i64() == Some(sender_id) {
                (sanitize_display_name(friend["nickname"].as_str()), None)
            } else {
                (None, None)
            }
        }
    };
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
        sender_nickname,
        sender_card,
        text,
        mentions,
        quote_seq,
        received_at,
        raw_message: value.clone(),
        notification: None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn verified(url: String) -> Milky {
        let milky = Milky::new(
            MilkyConfig {
                url,
                ..Default::default()
            },
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        )
        .unwrap();
        milky.logged_in.store(123, Ordering::Release);
        milky.verification.store(1, Ordering::Release);
        milky
    }
    #[test]
    fn administrator_moderation_never_bypasses_the_bots_actual_group_role() {
        assert!(admin_mute_allowed("admin", "admin", "member", false));
        assert!(admin_mute_allowed("owner", "member", "admin", true));
        assert!(!admin_mute_allowed("admin", "member", "member", false));
        assert!(!admin_mute_allowed("admin", "owner", "admin", true));
        assert!(!admin_mute_allowed("member", "owner", "member", true));
        assert!(!admin_mute_allowed("owner", "owner", "owner", true));
    }
    #[tokio::test]
    async fn administrator_unmute_checks_current_members_before_zero_duration_write() {
        use axum::{
            Json, Router,
            extract::{Path, State},
            routing::post,
        };
        use std::sync::Mutex;
        let writes = Arc::new(Mutex::new(Vec::<Value>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        async fn peer(
            State(writes): State<Arc<Mutex<Vec<Value>>>>,
            Path(api): Path<String>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            let data = if api == "get_group_member_info" {
                let role = match body["user_id"].as_i64().unwrap() {
                    123 => "owner",
                    7 => "admin",
                    _ => "member",
                };
                json!({"member":{"user_id":body["user_id"],"group_id":body["group_id"],"nickname":"fixture","card":"","role":role}})
            } else if api == "set_group_member_mute" {
                writes.lock().unwrap().push(body);
                json!({})
            } else {
                panic!("unexpected mock API")
            };
            Json(json!({"status":"ok","retcode":0,"data":data}))
        }
        let app = Router::new()
            .route("/api/{api}", post(peer))
            .with_state(writes.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        let milky = verified(url);
        let scope = Scope {
            bot_id: 123,
            scene: Scene::Group,
            peer_id: 456,
        };
        assert!(
            milky
                .set_group_member_mute_as_admin(&scope, 8, 9, 0, false, &cancel)
                .await
                .is_err()
        );
        assert!(writes.lock().unwrap().is_empty());
        assert!(matches!(
            milky
                .set_group_member_mute_as_admin(&scope, 7, 9, 0, false, &cancel)
                .await
                .unwrap(),
            ActionOutcome::Confirmed
        ));
        assert_eq!(
            writes.lock().unwrap().as_slice(),
            [json!({"group_id":456,"user_id":9,"duration":0})]
        );
        assert!(
            milky
                .set_group_member_mute_as_admin(&scope, 7, 123, 0, true, &cancel)
                .await
                .is_err()
        );
        assert_eq!(writes.lock().unwrap().len(), 1);
        cancel.cancel();
        task.await.unwrap();
    }

    #[test]
    fn nudge_uses_negative_dedup_identity_and_never_becomes_ai_text() {
        let event = json!({"event_type":"group_nudge","self_id":123,"time":12,"data":{"group_id":456,"sender_id":789,"receiver_id":123,"display_action":"不要信任此文本"}});
        let incoming = normalize_event_value(123, &event, 99).unwrap().unwrap();
        assert!(incoming.key.message_seq < 0);
        assert!(
            incoming.text.is_empty()
                && incoming.mentions.is_empty()
                && incoming.quote_seq.is_none()
        );
        let nudge = incoming.notification.unwrap();
        assert_eq!((nudge.initiator, nudge.target), (789, 123));
        assert_eq!(
            normalize_event_value(123, &event, 100)
                .unwrap()
                .unwrap()
                .key,
            incoming.key
        );
        assert!(normalize_event_value(124, &event, 99).is_err());
        let bad = json!({"event_type":"group_nudge","self_id":123,"time":12,"data":{"group_id":"456","sender_id":789,"receiver_id":123}});
        assert!(normalize_event_value(123, &bad, 99).is_err());
        let friend = json!({"event_type":"friend_nudge","self_id":123,"time":12,"data":{"user_id":789,"is_self_send":false,"is_self_receive":true}});
        let message = normalize_event_value(123, &friend, 99).unwrap().unwrap();
        assert_eq!(message.key.scope.scene, Scene::Friend);
        assert_eq!(message.notification.unwrap().target, 123);
    }

    #[test]
    fn member_roles_and_identity_are_strict_not_inferred_from_names() {
        let member = json!({"group_id":456,"user_id":789,"nickname":"群主\n","card":"管理员","role":"member"});
        let parsed = parse_member(&member, 456).unwrap();
        assert_eq!(parsed.nickname, "群主");
        assert_eq!(parsed.role, "member");
        assert!(parse_member(&member, 457).is_err());
        let mut bad = member.clone();
        bad["role"] = json!("superadmin");
        assert!(parse_member(&bad, 456).is_err());
        assert!(!outranks("admin", "admin") && !outranks("member", "member"));
        assert!(outranks("owner", "admin") && outranks("admin", "member"));
    }

    #[tokio::test]
    async fn structured_send_preserves_wire_segments_and_pre_cancel_is_deferred() {
        let (url, task) = server(
            "200 OK",
            r#"{"status":"ok","retcode":0,"data":{"message_seq":9}}"#,
        )
        .await;
        let milky = verified(url);
        let scope = Scope {
            bot_id: 123,
            scene: Scene::Group,
            peer_id: 456,
        };
        let message: OutgoingMessage = serde_json::from_value(json!({"segments":[
            {"type":"mention","data":{"user_id":789}},
            {"type":"image","data":{"uri":"https://example.org/a.png","sub_type":"sticker"}}
        ]}))
        .unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            milky.send_message(&scope, &message, &cancelled).await,
            SendOutcome::Deferred { .. }
        ));
        assert_eq!(
            milky
                .send_message(&scope, &message, &CancellationToken::new())
                .await,
            SendOutcome::Confirmed { message_seq: 9 }
        );
        let request = task.await.unwrap();
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["group_id"], 456);
        assert_eq!(body["message"][0]["type"], "mention");
        assert_eq!(body["message"][1]["data"]["sub_type"], "sticker");
    }

    #[tokio::test]
    async fn cancellation_after_http_submission_is_unknown() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let milky = verified(format!("http://{}", listener.local_addr().unwrap()));
        let (seen, received) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut buf = [0; 4096];
            let count = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert!(count > 0);
            seen.send(()).unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let cancel = CancellationToken::new();
        let scope = Scope {
            bot_id: 123,
            scene: Scene::Group,
            peer_id: 456,
        };
        let message = OutgoingMessage::text("one submission");
        let send = milky.send_message(&scope, &message, &cancel);
        let cancellation = async {
            received.await.unwrap();
            cancel.cancel();
        };
        let (outcome, _) = tokio::join!(send, cancellation);
        assert!(matches!(outcome, SendOutcome::Unknown { .. }));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn scoped_reads_reject_cross_scope_and_arbitrary_proxy_calls_before_http() {
        let (url, task) = server(
            "200 OK",
            r#"{"status":"ok","retcode":0,"data":{"group":{"group_id":456}}}"#,
        )
        .await;
        let milky = verified(url);
        let scope = Scope {
            bot_id: 123,
            scene: Scene::Group,
            peer_id: 456,
        };
        assert!(
            milky
                .proxy_read(Some(&scope), "get_cookies", json!({}))
                .await
                .is_err()
        );
        assert!(
            milky
                .proxy_read(Some(&scope), "get_group_info", json!({"group_id":457}))
                .await
                .is_err()
        );
        assert!(
            milky
                .get_message_raw(&MessageKey {
                    scope: scope.clone(),
                    message_seq: -123
                })
                .await
                .is_err()
        );
        let data = milky
            .proxy_read(Some(&scope), "get_group_info", json!({"group_id":456}))
            .await
            .unwrap();
        assert_eq!(data["group"]["group_id"], 456);
        let request = task.await.unwrap();
        assert!(request.starts_with("POST /api/get_group_info"));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["group_id"], 456);
    }

    #[tokio::test]
    async fn members_reject_response_from_another_group() {
        let (url, task) = server("200 OK", r#"{"status":"ok","retcode":0,"data":{"members":[{"group_id":457,"user_id":789,"nickname":"n","card":"","role":"member"}]}}"#).await;
        let milky = verified(url);
        assert!(
            milky
                .get_group_members(
                    &Scope {
                        bot_id: 123,
                        scene: Scene::Group,
                        peer_id: 456
                    },
                    false
                )
                .await
                .is_err()
        );
        task.await.unwrap();
    }

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
            json!({"groups":[{"group_id":123,"group_name":"name","remark":null}]}),
            json!({"groups":[{"group_id":123,"group_name":"name","remark":4}]}),
            json!({"groups":[{"group_id":123,"group_name":"one","remark":""},{"group_id":123,"group_name":"two","remark":""}]}),
        ] {
            assert!(parse_contacts(&data, "groups", "group_id", "group_name").is_err());
        }
    }

    #[test]
    fn absent_contact_remark_is_compatible_with_older_milky_entities() {
        for (data, field, id_key, name_key) in [
            (
                json!({"groups":[{"group_id":123,"group_name":"群"}]}),
                "groups",
                "group_id",
                "group_name",
            ),
            (
                json!({"friends":[{"user_id":456,"nickname":"好友"}]}),
                "friends",
                "user_id",
                "nickname",
            ),
        ] {
            let list = parse_contacts(&data, field, id_key, name_key).unwrap();
            assert_eq!(list.len(), 1);
            assert!(list[0].remark.is_empty());
        }
    }

    #[test]
    fn group_sender_labels_require_matching_member_and_group_and_are_bounded() {
        let mut value = event(json!([{"type":"text","data":{"text":"原文\n不改"}}]));
        value["data"]["group_member"] =
            json!({"user_id":789,"group_id":456,"nickname":"公开\n昵称","card":"群\u{202e}名片"});
        let incoming = normalize_message(123, &value["data"], 99).unwrap();
        assert_eq!(incoming.sender_nickname.as_deref(), Some("公开昵称"));
        assert_eq!(incoming.sender_card.as_deref(), Some("群名片"));
        assert_eq!(incoming.text, "原文\n不改");
        for (user, group) in [
            (json!(788), json!(456)),
            (json!(789), json!(455)),
            (json!("789"), json!(456)),
            (json!(789), json!("456")),
        ] {
            value["data"]["group_member"]["user_id"] = user;
            value["data"]["group_member"]["group_id"] = group;
            let message = normalize_message(123, &value["data"], 99).unwrap();
            assert!(message.sender_nickname.is_none() && message.sender_card.is_none());
            assert_eq!(message.sender_id, 789);
        }
        value["data"]["group_member"] =
            json!({"user_id":789,"group_id":456,"nickname":"鱼".repeat(100),"card":"\n\t"});
        let message = normalize_message(123, &value["data"], 99).unwrap();
        assert_eq!(message.sender_nickname.unwrap().chars().count(), 64);
        assert!(message.sender_card.is_none());
        value["data"]["group_member"] = json!(null);
        assert!(
            normalize_message(123, &value["data"], 99)
                .unwrap()
                .sender_nickname
                .is_none()
        );
    }

    #[test]
    fn friend_labels_never_use_private_remarks_or_mismatched_sender() {
        let mut value = json!({"message_scene":"friend","peer_id":789,"sender_id":789,"message_seq":1,"segments":[],"friend":{"user_id":789,"nickname":"公开昵称","remark":"仅本机可见的备注"}});
        let message = normalize_message(123, &value, 99).unwrap();
        assert_eq!(message.sender_nickname.as_deref(), Some("公开昵称"));
        assert!(message.sender_card.is_none());
        value["friend"]["nickname"] = json!(null);
        let missing = normalize_message(123, &value, 99).unwrap();
        assert!(missing.sender_nickname.is_none() && missing.sender_card.is_none());
        value["friend"]["nickname"] = json!("公开昵称");
        value["friend"]["user_id"] = json!(999);
        assert!(
            normalize_message(123, &value, 99)
                .unwrap()
                .sender_nickname
                .is_none()
        );
        value["friend"]["user_id"] = json!(789);
        value["sender_id"] = json!(123);
        assert!(
            normalize_message(123, &value, 99)
                .unwrap()
                .sender_nickname
                .is_none()
        );
    }

    #[test]
    fn verified_account_getter_never_returns_unverified_or_empty_identity() {
        let milky = Milky::new(MilkyConfig::default(), reqwest::Client::new()).unwrap();
        assert_eq!(milky.verified_bot_id(), None);
        milky.verification.store(1, Ordering::Release);
        assert_eq!(milky.verified_bot_id(), None);
        milky.logged_in.store(123, Ordering::Release);
        assert_eq!(milky.verified_bot_id(), Some(123));
        milky.invalidate_account();
        assert_eq!(milky.verified_bot_id(), None);
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
