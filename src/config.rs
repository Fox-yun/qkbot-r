use crate::{
    llm::LlmConfig,
    milky::MilkyConfig,
    types::{Scene, Scope},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, net::SocketAddr, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub app: AppConfig,
    pub milky: MilkyConfig,
    pub llm: LlmConfig,
    pub access: AccessConfig,
    pub limits: Limits,
    pub plugins: PluginsConfig,
    pub web: WebConfig,
    pub qq_login: QqLoginConfig,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub enabled: bool,
    pub listen: String,
    pub token: String,
    pub secure_cookie: bool,
}
impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: "127.0.0.1:3210".into(),
            token: String::new(),
            secure_cookie: false,
        }
    }
}
impl std::fmt::Debug for WebConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebConfig")
            .field("enabled", &self.enabled)
            .field("listen", &self.listen)
            .field("token", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QqLoginConfig {
    pub enabled: bool,
    pub url: String,
    pub token: String,
}
impl Default for QqLoginConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: "http://127.0.0.1:3080".into(),
            token: String::new(),
        }
    }
}
impl std::fmt::Debug for QqLoginConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QqLoginConfig")
            .field("enabled", &self.enabled)
            .field("url", &self.url)
            .field("token", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub database: String,
    pub timezone: String,
    pub log: String,
}
impl Default for AppConfig {
    fn default() -> Self {
        Self {
            database: "qkbot.sqlite3".into(),
            timezone: "Asia/Shanghai".into(),
            log: "info".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AccessConfig {
    pub groups: Vec<i64>,
    pub friends: Vec<i64>,
    #[serde(
        rename = "admins",
        skip_serializing,
        deserialize_with = "ignore_legacy_admins"
    )]
    _legacy_admins: Vec<i64>,
}
impl AccessConfig {
    pub fn allows(&self, scope: &Scope) -> bool {
        match scope.scene {
            Scene::Group => self.groups.contains(&scope.peer_id),
            Scene::Friend => self.friends.contains(&scope.peer_id),
        }
    }
}

fn ignore_legacy_admins<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<i64>, D::Error> {
    let _ = serde::de::IgnoredAny::deserialize(d)?;
    Ok(Vec::new())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub queued_global: usize,
    pub queued_per_conversation: usize,
    pub control_tasks: usize,
    pub database_queue: usize,
    pub active_conversations: usize,
    pub max_message_bytes: usize,
    pub turn_timeout_secs: u64,
    pub tool_timeout_secs: u64,
    pub max_model_calls: usize,
    pub max_http_attempts: usize,
    pub max_tool_calls: usize,
    pub retention_days: u64,
    pub send_cooldown_secs: u64,
    pub sends_per_hour: usize,
    pub model_user_per_hour: usize,
    pub model_conversation_per_hour: usize,
    pub model_global_per_hour: usize,
    pub memory_requests_per_hour: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            queued_global: 32,
            queued_per_conversation: 4,
            control_tasks: 8,
            database_queue: 128,
            active_conversations: 128,
            max_message_bytes: 16384,
            turn_timeout_secs: 120,
            tool_timeout_secs: 15,
            max_model_calls: 3,
            max_http_attempts: 4,
            max_tool_calls: 2,
            retention_days: 7,
            send_cooldown_secs: 2,
            sends_per_hour: 120,
            model_user_per_hour: 60,
            model_conversation_per_hour: 180,
            model_global_per_hour: 600,
            memory_requests_per_hour: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PluginsConfig {
    pub ai: AiConfig,
    pub clock: ClockConfig,
    pub reminder: ReminderConfig,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClockConfig {
    pub enabled: bool,
}
impl Default for ClockConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReminderConfig {
    pub enabled: bool,
    pub max_active_per_user: usize,
    pub llm_writes_require_confirmation: bool,
}
impl Default for ReminderConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_active_per_user: 10,
            llm_writes_require_confirmation: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AiSettings {
    pub mode: String,
    pub input_budget: usize,
    pub output_budget: usize,
    pub recent_messages: usize,
    pub persona: String,
    pub memory_read: bool,
    pub memory_write: bool,
    pub summary: bool,
    pub paused: bool,
    pub once: bool,
    pub allowed_tools: Vec<String>,
}
impl Default for AiSettings {
    fn default() -> Self {
        Self {
            mode: "group".into(),
            input_budget: 6000,
            output_budget: 800,
            recent_messages: 24,
            persona: "默认".into(),
            memory_read: true,
            memory_write: true,
            summary: true,
            paused: false,
            once: false,
            allowed_tools: vec!["clock_now".into()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AiConfig {
    pub enabled: bool,
    pub defaults: AiSettings,
    pub keywords: Vec<String>,
    pub read_group_background: bool,
    pub learn_all_group_messages: bool,
    pub compact_after_messages: usize,
    pub compact_idle_secs: u64,
    pub memory_input_budget: usize,
    pub memory_output_budget: usize,
    pub max_memories_per_conversation: usize,
    pub memory_recall_limit: usize,
    pub memory_recall_budget: usize,
    pub personas: BTreeMap<String, String>,
}
impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            defaults: AiSettings::default(),
            keywords: vec!["小鱼".into()],
            read_group_background: true,
            learn_all_group_messages: false,
            compact_after_messages: 40,
            compact_idle_secs: 600,
            memory_input_budget: 6000,
            memory_output_budget: 1600,
            max_memories_per_conversation: 256,
            memory_recall_limit: 5,
            memory_recall_budget: 1000,
            personas: BTreeMap::from([
                (
                    "默认".into(),
                    "你是群聊助手小鱼。自然、简洁地用中文回答；记清谁说了什么。不确定时直说。"
                        .into(),
                ),
                (
                    "温柔".into(),
                    "你是小鱼，用温柔、简洁的中文回答，不改变已知事实和权限。".into(),
                ),
            ]),
        }
    }
}

impl Config {
    /// Parse only file values; callers can inspect/edit them without persisting env secrets.
    pub fn load_raw(path: &Path) -> Result<Self> {
        let data = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置 {}", path.display()))?;
        Self::from_toml(&data)
    }
    pub fn from_toml(data: &str) -> Result<Self> {
        // TOML errors can quote the offending source line, including a credential.
        toml::from_str(data).map_err(|_| anyhow::anyhow!("TOML 配置无效，请检查语法和字段类型"))
    }
    pub fn apply_environment(&mut self) {
        if let Ok(key) = std::env::var("QKBOT_API_KEY") {
            self.llm.api_key = key;
        }
        if let Ok(token) = std::env::var("QKBOT_MILKY_TOKEN") {
            self.milky.access_token = Some(token);
        }
        if let Ok(token) = std::env::var("QKBOT_ADMIN_TOKEN") {
            self.web.token = token;
        }
        if let Ok(token) = std::env::var("QKBOT_LLBOT_TOKEN") {
            self.qq_login.token = token;
        }
    }
    pub fn load(path: &Path) -> Result<Self> {
        let mut config = Self::load_raw(path)?;
        config.apply_environment();
        config.validate()?;
        Ok(config)
    }
    /// Validate the same local constructors used at startup, without network or database I/O.
    pub fn validate_runtime(&self) -> Result<()> {
        self.validate()?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| anyhow::anyhow!("无法创建HTTP客户端"))?;
        crate::milky::Milky::new(self.milky.clone(), http.clone())
            .map_err(|_| anyhow::anyhow!("Milky配置无效，请检查URL、鉴权格式、响应大小和超时"))?;
        if self.plugins.ai.enabled {
            crate::llm::Llm::new(self.llm.clone(), http).map_err(|_| {
                anyhow::anyhow!("模型配置无效，请检查URL、模型名称、鉴权格式、并发和超时")
            })?;
        }
        crate::plugins::assemble(self)
            .map_err(|_| anyhow::anyhow!("插件或工具配置无效，请检查启用项和allowed_tools"))?;
        Ok(())
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            [
                self.limits.model_user_per_hour,
                self.limits.model_conversation_per_hour,
                self.limits.model_global_per_hour,
                self.limits.memory_requests_per_hour
            ]
            .iter()
            .all(|n| (1..=100_000).contains(n)),
            "model hourly request limits must be 1..100000"
        );
        ensure!(!self.app.database.trim().is_empty(), "app.database不能为空");
        tracing_subscriber::EnvFilter::try_new(&self.app.log)
            .map_err(|_| anyhow::anyhow!("app.log不是有效日志过滤器"))?;
        self.web
            .listen
            .parse::<SocketAddr>()
            .map_err(|_| anyhow::anyhow!("web.listen 必须是 IP:端口（IPv6 使用 [地址]:端口）"))?;
        ensure!(
            self.web.token.is_empty() || self.web.token.len() >= 32,
            "web.token 非空时必须至少32字节"
        );
        reqwest::header::HeaderValue::from_str(&self.web.token)
            .map_err(|_| anyhow::anyhow!("web.token 包含无效请求头字符"))?;
        let login_url = reqwest::Url::parse(&self.qq_login.url)
            .map_err(|_| anyhow::anyhow!("qq_login.url 不是有效URL"))?;
        ensure!(
            matches!(login_url.scheme(), "http" | "https") && login_url.host_str().is_some(),
            "qq_login.url 必须使用http或https"
        );
        ensure!(
            login_url.username().is_empty()
                && login_url.password().is_none()
                && login_url.query().is_none()
                && login_url.fragment().is_none(),
            "qq_login.url 不能包含用户名、密码、查询参数或片段"
        );
        ensure!(self.qq_login.token.len() <= 4096, "LLBot WebUI 密码过长");
        self.app
            .timezone
            .parse::<chrono_tz::Tz>()
            .context("无效时区")?;
        ensure!(
            self.limits.queued_global > 0 && self.limits.queued_global <= 1024,
            "queued_global must be 1..1024"
        );
        ensure!(
            self.limits.queued_per_conversation > 0
                && self.limits.queued_per_conversation <= self.limits.queued_global,
            "invalid conversation queue limit"
        );
        ensure!(
            (1..=64).contains(&self.limits.control_tasks)
                && (8..=1024).contains(&self.limits.database_queue)
                && (1..=1024).contains(&self.limits.active_conversations),
            "invalid worker limits"
        );
        ensure!(
            (1024..=32768).contains(&self.limits.max_message_bytes),
            "invalid message byte cap"
        );
        ensure!(
            (1..=900).contains(&self.limits.turn_timeout_secs)
                && (1..=120).contains(&self.limits.tool_timeout_secs),
            "invalid turn/tool timeout"
        );
        ensure!(
            (1..=16).contains(&self.limits.max_model_calls)
                && (1..=32).contains(&self.limits.max_http_attempts)
                && self.limits.max_tool_calls <= 15,
            "model/tool limits exceed supported bounds"
        );
        ensure!(
            (1..=100_000).contains(&self.limits.sends_per_hour)
                && (1..=365).contains(&self.limits.retention_days)
                && self.limits.send_cooldown_secs <= 60,
            "invalid send/retention limit"
        );
        ensure!(
            self.plugins.reminder.llm_writes_require_confirmation,
            "LLM write confirmation cannot be disabled in v1"
        );
        ensure!(
            (1..=10).contains(&self.plugins.reminder.max_active_per_user),
            "reminder max_active_per_user must be 1..10"
        );
        if self.plugins.ai.enabled {
            let ai = &self.plugins.ai;
            validate_settings(&ai.defaults)?;
            ensure!(
                ai.personas.contains_key(&ai.defaults.persona),
                "default persona not defined"
            );
            ensure!(
                ai.personas
                    .values()
                    .all(|s| !s.is_empty() && s.len() <= 8192),
                "persona must be 1..8192 bytes"
            );
            ensure!(
                ai.keywords
                    .iter()
                    .all(|s| !s.trim().is_empty() && s.len() <= 100),
                "invalid keyword"
            );
            ensure!(
                (1..=8).contains(&self.llm.max_concurrent_requests),
                "LLM concurrency must be 1..8"
            );
            ensure!(
                ai.compact_after_messages >= 4 && ai.compact_idle_secs >= 30,
                "invalid compaction threshold"
            );
            ensure!(
                (3072..=262144).contains(&ai.memory_input_budget)
                    && (256..=8192).contains(&ai.memory_output_budget),
                "memory input must be 3072..262144; output 256..8192"
            );
            ensure!(
                (1..=1_000_000).contains(&ai.max_memories_per_conversation)
                    && (1..=128).contains(&ai.memory_recall_limit)
                    && (64..=262144).contains(&ai.memory_recall_budget),
                "memory capacity must be 1..1000000; recall count 1..128; recall budget 64..262144"
            );
        }
        Ok(())
    }
}
pub fn validate_settings(s: &AiSettings) -> Result<()> {
    ensure!(
        s.mode == "group" || s.mode == "user",
        "mode must be group or user"
    );
    ensure!(
        (1024..=262144).contains(&s.input_budget),
        "input budget must be 1024..262144"
    );
    ensure!(
        (64..=32768).contains(&s.output_budget),
        "output budget must be 64..32768"
    );
    ensure!(s.recent_messages <= 4096, "history limit must be <=4096");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn web_and_login_defaults_and_address_validation() {
        let mut config = Config::default();
        assert_eq!(config.web.listen, "127.0.0.1:3210");
        assert_eq!(config.qq_login.url, "http://127.0.0.1:3080");
        assert!(config.web.enabled && config.qq_login.enabled);
        config.validate().unwrap();
        config.web.listen = "localhost:3210".into();
        assert!(config.validate().is_err());
        config.web.listen = "[::1]:3210".into();
        config.web.token = "short".into();
        assert!(config.validate().is_err());
        config.web.token = "a".repeat(32);
        config.validate().unwrap();
        config.web.token.push('\n');
        assert!(config.validate().is_err());
        config.web.token.clear();
        for url in [
            "file:///tmp/qq",
            "https://user:pass@example.invalid",
            "https://example.invalid/?token=fixture",
            "https://example.invalid/#fragment",
        ] {
            config.qq_login.url = url.into();
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn debug_and_toml_errors_do_not_disclose_credentials() {
        let mut config = Config::default();
        config.web.token = "admin-sensitive-fixture".into();
        config.qq_login.token = "login-sensitive-fixture".into();
        config.llm.api_key = "model-sensitive-fixture".into();
        config.milky.access_token = Some("milky-sensitive-fixture".into());
        assert!(!format!("{config:?}").contains("sensitive-fixture"));
        let error = Config::from_toml("[web]\ntoken = \"sensitive-fixture-unclosed").unwrap_err();
        assert!(!format!("{error:#}").contains("sensitive-fixture"));
    }

    #[test]
    fn raw_loading_keeps_stored_values_and_serialization_roundtrips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = Config::default();
        config.llm.api_key = "raw-model-fixture".into();
        config.web.token = "raw-admin-fixture-at-least-32-bytes".into();
        fs::write(&path, toml::to_string_pretty(&config).unwrap()).unwrap();
        let loaded = Config::load_raw(&path).unwrap();
        assert_eq!(loaded.llm.api_key, config.llm.api_key);
        assert_eq!(loaded.web.token, config.web.token);
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&config).unwrap()
        );
    }
}
