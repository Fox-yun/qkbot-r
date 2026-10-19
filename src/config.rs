use crate::{
    llm::LlmConfig,
    milky::MilkyConfig,
    types::{Scene, Scope},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub app: AppConfig,
    pub milky: MilkyConfig,
    pub llm: LlmConfig,
    pub access: AccessConfig,
    pub limits: Limits,
    pub plugins: PluginsConfig,
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AccessConfig {
    pub groups: Vec<i64>,
    pub friends: Vec<i64>,
    pub admins: Vec<i64>,
}
impl AccessConfig {
    pub fn allows(&self, scope: &Scope) -> bool {
        match scope.scene {
            Scene::Group => self.groups.contains(&scope.peer_id),
            Scene::Friend => self.friends.contains(&scope.peer_id),
        }
    }
    pub fn admin(&self, user: i64) -> bool {
        self.admins.contains(&user)
    }
}

#[derive(Debug, Clone, Deserialize)]
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
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PluginsConfig {
    pub ai: AiConfig,
    pub clock: ClockConfig,
    pub reminder: ReminderConfig,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClockConfig {
    pub enabled: bool,
}
impl Default for ClockConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}
#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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
            keywords: vec!["小葵".into()],
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
                    "你是群聊助手小葵。自然、简洁地用中文回答；记清谁说了什么。不确定时直说。"
                        .into(),
                ),
                (
                    "温柔".into(),
                    "你是小葵，用温柔、简洁的中文回答，不改变已知事实和权限。".into(),
                ),
            ]),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置 {}", path.display()))?;
        let mut config: Self = toml::from_str(&data).context("TOML 配置无效")?;
        if let Ok(key) = std::env::var("QKBOT_API_KEY") {
            config.llm.api_key = key;
        }
        if let Ok(token) = std::env::var("QKBOT_MILKY_TOKEN") {
            config.milky.access_token = Some(token);
        }
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
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
