//! File-backed administrator settings. Secrets never leave through snapshots.
use std::{
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::config::Config;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Deserialize, Default)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretChange {
    #[default]
    Keep,
    Set {
        value: String,
    },
    Clear,
}
impl fmt::Debug for SecretChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Keep => "Keep",
            Self::Set { .. } => "Set([redacted])",
            Self::Clear => "Clear",
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecretUpdates {
    pub llm_api_key: SecretChange,
    pub milky_access_token: SecretChange,
    pub llbot_token: SecretChange,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveRequest {
    pub revision: String,
    #[serde(deserialize_with = "complete_config")]
    pub config: Config,
    #[serde(default)]
    pub secrets: SecretUpdates,
}

fn complete_config<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Config, D::Error> {
    fn complete(value: &Value, shape: &Value, path: &str) -> bool {
        if path == "plugins.ai.personas" {
            return value.is_object();
        }
        let Some(fields) = shape.as_object() else {
            return true;
        };
        let Some(object) = value.as_object() else {
            return false;
        };
        fields.iter().all(|(key, expected)| {
            if path == "web" && key == "token" && !object.contains_key(key) {
                return true;
            }
            object.get(key).is_some_and(|actual| {
                complete(
                    actual,
                    expected,
                    &if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    },
                )
            })
        })
    }
    let value = Value::deserialize(deserializer)?;
    let shape = serde_json::to_value(Config::default())
        .map_err(|_| serde::de::Error::custom("无法读取配置结构"))?;
    if !complete(&value, &shape, "") {
        return Err(serde::de::Error::custom(
            "请提交GET返回的完整配置，不能省略字段",
        ));
    }
    serde_json::from_value(value).map_err(|_| serde::de::Error::custom("配置字段或类型无效"))
}

#[derive(Debug)]
pub struct RevisionConflict;
impl fmt::Display for RevisionConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("配置已被其他操作修改，请刷新后重试")
    }
}
impl std::error::Error for RevisionConflict {}

#[derive(Clone, Default)]
struct Environment {
    llm_api_key: Option<String>,
    milky_access_token: Option<String>,
    admin_token: Option<String>,
    llbot_token: Option<String>,
}
impl Environment {
    fn read() -> Self {
        Self {
            llm_api_key: std::env::var("QKBOT_API_KEY").ok(),
            milky_access_token: std::env::var("QKBOT_MILKY_TOKEN").ok(),
            admin_token: std::env::var("QKBOT_ADMIN_TOKEN").ok(),
            llbot_token: std::env::var("QKBOT_LLBOT_TOKEN").ok(),
        }
    }
    fn apply(&self, config: &mut Config) {
        if let Some(value) = &self.llm_api_key {
            config.llm.api_key = value.clone();
        }
        if let Some(value) = &self.milky_access_token {
            config.milky.access_token = Some(value.clone());
        }
        if let Some(value) = &self.admin_token {
            config.web.token = value.clone();
        }
        if let Some(value) = &self.llbot_token {
            config.qq_login.token = value.clone();
        }
    }
}

pub struct ConfigStore {
    path: PathBuf,
    serial: Arc<Mutex<()>>,
    cached: Arc<RwLock<Cached>>,
}

struct Cached {
    effective: Config,
    revision: String,
    snapshot: Value,
    qq_version: u64,
    milky_version: u64,
}
impl Cached {
    fn read(path: &Path, environment: &Environment) -> Result<Self> {
        let bytes = read_file(path)?;
        Self::from_bytes(path, &bytes, environment)
    }
    fn from_bytes(path: &Path, bytes: &[u8], environment: &Environment) -> Result<Self> {
        Ok(Self {
            effective: effective(path, bytes, environment)?,
            revision: revision(bytes),
            snapshot: snapshot(bytes, environment)?,
            qq_version: 0,
            milky_version: 0,
        })
    }
}

fn publish_cache(cache: &RwLock<Cached>, mut next: Cached) -> Result<Value> {
    let mut previous = cache
        .write()
        .map_err(|_| anyhow::anyhow!("配置缓存不可用"))?;
    let changed = serde_json::to_vec(&previous.effective.qq_login)?
        != serde_json::to_vec(&next.effective.qq_login)?;
    next.qq_version = previous.qq_version.wrapping_add(u64::from(changed));
    let milky_changed = serde_json::to_vec(&previous.effective.milky)?
        != serde_json::to_vec(&next.effective.milky)?;
    next.milky_version = previous
        .milky_version
        .wrapping_add(u64::from(milky_changed));
    let snapshot = next.snapshot.clone();
    *previous = next;
    Ok(snapshot)
}
impl ConfigStore {
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let (path, cached) = tokio::task::spawn_blocking(move || {
            let environment = Environment::read();
            let path = initialize(path, &environment)?;
            let cached = Cached::read(&path, &environment)?;
            Ok::<_, anyhow::Error>((path, cached))
        })
        .await
        .context("配置任务未完成")??;
        Ok(Self {
            path,
            serial: Arc::new(Mutex::new(())),
            cached: Arc::new(RwLock::new(cached)),
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn snapshot(&self) -> Result<Value> {
        self.reload().await
    }

    /// Explicit administrator reload. Polling paths must use the cached accessors.
    pub async fn reload(&self) -> Result<Value> {
        let guard = self.serial.clone().lock_owned().await;
        let path = self.path.clone();
        let cache = self.cached.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let next = Cached::read(&path, &Environment::read())?;
            publish_cache(&cache, next)
        })
        .await
        .context("配置任务未完成")?
    }

    pub async fn save(&self, request: SaveRequest) -> Result<Value> {
        let guard = self.serial.clone().lock_owned().await;
        let path = self.path.clone();
        let cache = self.cached.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let environment = Environment::read();
            let next = save_file_cached(&path, request, &environment)?;
            // Commit the cache in the same blocking job as the file. A canceled
            // HTTP request cannot leave a successful save invisible to runtime.
            publish_cache(&cache, next)
        })
        .await
        .context("配置任务未完成")?
    }

    pub async fn effective_with_revision(&self) -> Result<(Config, String)> {
        let cache = self
            .cached
            .read()
            .map_err(|_| anyhow::anyhow!("配置缓存不可用"))?;
        Ok((cache.effective.clone(), cache.revision.clone()))
    }

    pub async fn effective(&self) -> Result<Config> {
        self.effective_with_revision()
            .await
            .map(|(config, _)| config)
    }

    pub async fn cached_snapshot(&self) -> Result<Value> {
        Ok(self
            .cached
            .read()
            .map_err(|_| anyhow::anyhow!("配置缓存不可用"))?
            .snapshot
            .clone())
    }

    pub async fn qq_config(&self) -> Result<crate::config::QqLoginConfig> {
        Ok(self
            .cached
            .read()
            .map_err(|_| anyhow::anyhow!("配置缓存不可用"))?
            .effective
            .qq_login
            .clone())
    }

    pub async fn qq_config_version(&self) -> Result<(crate::config::QqLoginConfig, u64)> {
        let cache = self
            .cached
            .read()
            .map_err(|_| anyhow::anyhow!("配置缓存不可用"))?;
        Ok((cache.effective.qq_login.clone(), cache.qq_version))
    }

    pub async fn milky_config_version(&self) -> Result<(crate::milky::MilkyConfig, u64)> {
        let cache = self
            .cached
            .read()
            .map_err(|_| anyhow::anyhow!("配置缓存不可用"))?;
        Ok((cache.effective.milky.clone(), cache.milky_version))
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path).context("无法读取配置文件")?;
    ensure!(
        file.metadata()?.len() <= MAX_CONFIG_BYTES,
        "配置文件超过1MiB限制"
    );
    let mut bytes = Vec::new();
    use std::io::Read;
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("无法读取配置文件")?;
    ensure!(
        bytes.len() as u64 <= MAX_CONFIG_BYTES,
        "配置文件超过1MiB限制"
    );
    Ok(bytes)
}
fn parse(bytes: &[u8]) -> Result<Config> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| anyhow::anyhow!("配置文件必须是UTF-8文本"))?;
    Config::from_toml(text)
}
fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn initialize(path: PathBuf, environment: &Environment) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = path.parent().context("配置路径缺少父目录")?;
    fs::create_dir_all(parent).context("无法创建配置目录")?;
    if !path.exists() {
        let mut temporary = tempfile::Builder::new()
            .prefix(".qkbot-config-")
            .suffix(".tmp")
            .tempfile_in(parent)
            .context("无法创建配置临时文件")?;
        temporary
            .write_all(include_str!("../../config.example.toml").as_bytes())
            .context("无法写入初始配置")?;
        temporary.as_file().sync_all().context("无法同步初始配置")?;
        if let Err(error) = temporary.persist_noclobber(&path)
            && error.error.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(anyhow::anyhow!("无法创建初始配置文件"));
        }
    }
    let path = fs::canonicalize(path).context("无法定位配置文件")?;
    let bytes = read_file(&path)?;
    validate_stored(&parse(&bytes)?)?;
    effective(&path, &bytes, environment)?;
    Ok(path)
}

fn effective(path: &Path, bytes: &[u8], environment: &Environment) -> Result<Config> {
    let mut config = parse(bytes)?;
    environment.apply(&mut config);
    ensure!(
        !config.app.database.trim().is_empty(),
        "app.database不能为空"
    );
    let database = Path::new(&config.app.database);
    if database.is_relative() {
        config.app.database = path
            .parent()
            .context("配置路径缺少父目录")?
            .join(database)
            .to_str()
            .context("数据库路径不是有效UTF-8")?
            .to_owned();
    }
    validate_stored(&config)?;
    Ok(config)
}

fn secret_status(file: &str, environment: &Option<String>) -> Value {
    json!({
        "file_set": !file.is_empty(),
        "env_set": environment.is_some(),
        "effective_set": !environment.as_deref().unwrap_or(file).is_empty(),
        "source": if environment.is_some() { "environment" } else if !file.is_empty() { "file" } else { "none" },
    })
}
fn snapshot(bytes: &[u8], environment: &Environment) -> Result<Value> {
    let mut config = parse(bytes)?;
    // External editors can bypass save validation. Reject invalid endpoints before
    // serializing them, including URLs that accidentally contain userinfo secrets.
    validate_stored(&config)?;
    let secrets = json!({
        "llm_api_key": secret_status(&config.llm.api_key, &environment.llm_api_key),
        "milky_access_token": secret_status(config.milky.access_token.as_deref().unwrap_or(""), &environment.milky_access_token),
        "llbot_token": secret_status(&config.qq_login.token, &environment.llbot_token),
    });
    config.llm.api_key.clear();
    config.milky.access_token = None;
    config.web.token.clear();
    config.qq_login.token.clear();
    let mut config = serde_json::to_value(config)?;
    config["web"].as_object_mut().unwrap().remove("token");
    Ok(json!({ "config": config, "revision": revision(bytes), "secrets": secrets }))
}

fn change_secret(original: &str, change: &SecretChange) -> Result<String> {
    match change {
        SecretChange::Keep => Ok(original.to_owned()),
        SecretChange::Clear => Ok(String::new()),
        SecretChange::Set { value } => {
            ensure!(!value.is_empty(), "设置凭据不能为空；删除请明确选择clear");
            Ok(value.clone())
        }
    }
}

#[cfg(test)]
fn save_file(path: &Path, request: SaveRequest, environment: &Environment) -> Result<Value> {
    Ok(save_file_cached(path, request, environment)?.snapshot)
}

fn save_file_cached(
    path: &Path,
    request: SaveRequest,
    environment: &Environment,
) -> Result<Cached> {
    let previous_bytes = read_file(path)?;
    if revision(&previous_bytes) != request.revision {
        return Err(RevisionConflict.into());
    }
    let previous = parse(&previous_bytes)?;
    let mut config = request.config;
    ensure!(
        config.web.enabled == previous.web.enabled
            && config.web.listen == previous.web.listen
            && config.web.secure_cookie == previous.web.secure_cookie,
        "web.enabled、web.listen、web.secure_cookie只能修改文件并重启管理服务，不能通过API修改"
    );
    ensure!(
        config.llm.api_key.is_empty()
            && config
                .milky
                .access_token
                .as_deref()
                .is_none_or(str::is_empty)
            && config.web.token.is_empty()
            && config.qq_login.token.is_empty(),
        "配置正文中的凭据必须为空，请使用secrets的keep/set/clear操作"
    );
    config.llm.api_key = change_secret(&previous.llm.api_key, &request.secrets.llm_api_key)?;
    config.qq_login.token = change_secret(&previous.qq_login.token, &request.secrets.llbot_token)?;
    config.web.token = previous.web.token;
    config.milky.access_token = match &request.secrets.milky_access_token {
        SecretChange::Keep => previous.milky.access_token,
        SecretChange::Clear => None,
        change => Some(change_secret("", change)?),
    };
    // Validate both stored and effective values, so an env override cannot conceal
    // an invalid file credential that would fail after a future restart.
    validate_stored(&config)?;
    let encoded = toml::to_string_pretty(&config).map_err(|_| anyhow::anyhow!("无法序列化配置"))?;
    ensure!(
        encoded.len() as u64 <= MAX_CONFIG_BYTES,
        "配置文件超过1MiB限制"
    );
    let cached = Cached::from_bytes(path, encoded.as_bytes(), environment)?;
    let parent = path.parent().context("配置路径缺少父目录")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".qkbot-config-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .context("无法创建配置临时文件")?;
    temporary
        .as_file()
        .set_permissions(
            fs::metadata(path)
                .context("无法读取配置权限")?
                .permissions(),
        )
        .context("无法保留配置权限")?;
    temporary
        .write_all(encoded.as_bytes())
        .context("无法写入配置临时文件")?;
    temporary
        .as_file()
        .sync_all()
        .context("无法同步配置临时文件")?;
    // Also catch an editor's modification while validation/temp-file writing ran.
    if revision(&read_file(path)?) != request.revision {
        return Err(RevisionConflict.into());
    }
    temporary
        .persist(path)
        .map_err(|_| anyhow::anyhow!("无法原子替换配置文件；原文件未更改"))?;
    Ok(cached)
}

/// Local shape and safety checks only. An unfinished model setup must not make
/// its own administration page unavailable or create new HTTP clients on reads.
fn validate_stored(config: &Config) -> Result<()> {
    config.validate()?;
    crate::plugins::assemble(config)?;
    for value in [&config.llm.chat_completions_url, &config.milky.url] {
        if value.trim().is_empty() {
            continue;
        }
        let url = reqwest::Url::parse(value).map_err(|_| anyhow::anyhow!("接口地址格式无效"))?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "接口地址必须使用HTTP(S)，不能包含用户名、密码、查询参数或片段"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn milky_generation_changes_only_for_transport_changes_and_detects_a_b_a() {
        let (_directory, path, _) = fixture();
        let store = ConfigStore::open(&path).await.unwrap();
        let (original, first) = store.milky_config_version().await.unwrap();
        let mut config = store.effective().await.unwrap();
        config.app.timezone = "UTC".into();
        fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        store.reload().await.unwrap();
        assert_eq!(store.milky_config_version().await.unwrap().1, first);
        config.milky.url = "http://127.0.0.1:3999".into();
        fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        store.reload().await.unwrap();
        assert_eq!(store.milky_config_version().await.unwrap().1, first + 1);
        config.milky = original;
        fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        store.reload().await.unwrap();
        assert_eq!(store.milky_config_version().await.unwrap().1, first + 2);
    }

    #[tokio::test]
    async fn cached_reads_do_not_reopen_files_and_explicit_reload_preserves_last_valid_state() {
        let (_directory, path, _) = fixture();
        let store = ConfigStore::open(&path).await.unwrap();
        let (_, revision) = store.effective_with_revision().await.unwrap();
        let original = fs::read(&path).unwrap();
        fs::write(&path, b"invalid = [").unwrap();
        assert_eq!(store.effective_with_revision().await.unwrap().1, revision);
        assert!(store.qq_config().await.is_ok());
        assert!(store.snapshot().await.is_err());
        assert_eq!(store.cached_snapshot().await.unwrap()["revision"], revision);
        fs::remove_file(&path).unwrap();
        assert!(store.effective().await.is_ok());
        fs::write(&path, [&original[..], b"\n# explicit reload\n"].concat()).unwrap();
        let next = store.snapshot().await.unwrap();
        assert_ne!(next["revision"], revision);
        assert_eq!(
            store.effective_with_revision().await.unwrap().1,
            next["revision"].as_str().unwrap()
        );
    }

    #[tokio::test]
    async fn incomplete_model_settings_can_be_saved_and_cache_changes_after_successful_commit_only()
    {
        let (_directory, path, _) = fixture();
        let store = ConfigStore::open(&path).await.unwrap();
        let view = store.snapshot().await.unwrap();
        let mut request: SaveRequest =
            serde_json::from_value(json!({"revision":view["revision"],"config":view["config"]}))
                .unwrap();
        request.config.llm.chat_completions_url.clear();
        request.config.llm.model.clear();
        let saved = store.save(request).await.unwrap();
        assert_eq!(saved["config"]["llm"]["model"], "");
        let (config, revision) = store.effective_with_revision().await.unwrap();
        assert!(config.llm.model.is_empty());
        assert_eq!(revision, saved["revision"].as_str().unwrap());
        let mut request: SaveRequest =
            serde_json::from_value(json!({"revision":saved["revision"],"config":saved["config"]}))
                .unwrap();
        request.config.app.timezone = "UTC".into();
        let mut external = fs::read(&path).unwrap();
        external.extend_from_slice(b"\n# external change\n");
        fs::write(&path, external).unwrap();
        assert!(store.save(request).await.is_err());
        assert_eq!(store.effective_with_revision().await.unwrap().1, revision);
        assert_eq!(
            store.effective().await.unwrap().app.timezone,
            config.app.timezone
        );
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, Environment) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = Config::from_toml(include_str!("../../config.example.toml")).unwrap();
        config.llm.api_key = "file-model-fixture".into();
        config.milky.access_token = Some("file-milky-fixture".into());
        config.qq_login.token = "file-login-fixture".into();
        config.web.token = "file-admin-fixture-at-least-32-bytes".into();
        fs::write(&path, toml::to_string_pretty(&config).unwrap()).unwrap();
        (directory, path, Environment::default())
    }
    fn request(path: &Path, environment: &Environment) -> SaveRequest {
        let view = snapshot(&read_file(path).unwrap(), environment).unwrap();
        serde_json::from_value(json!({"revision":view["revision"],"config":view["config"]}))
            .unwrap()
    }

    #[test]
    fn secrets_are_redacted_and_explicit_keep_set_clear_preserve_file_values() {
        let (_directory, path, environment) = fixture();
        let mut update = request(&path, &environment);
        update.config.app.timezone = "UTC".into();
        let view = save_file(&path, update, &environment).unwrap();
        assert!(!view.to_string().contains("file-model-fixture"));
        assert!(!view.to_string().contains("file-milky-fixture"));
        assert!(!view.to_string().contains("file-login-fixture"));
        assert!(!view.to_string().contains("file-admin-fixture"));
        assert!(view["config"]["web"].get("token").is_none());
        assert!(view["secrets"].get("admin_token").is_none());
        let kept = Config::load_raw(&path).unwrap();
        assert_eq!(kept.llm.api_key, "file-model-fixture");
        assert_eq!(
            kept.milky.access_token.as_deref(),
            Some("file-milky-fixture")
        );
        assert_eq!(kept.qq_login.token, "file-login-fixture");
        assert_eq!(kept.app.timezone, "UTC");

        let mut update = request(&path, &environment);
        update.secrets.llm_api_key = SecretChange::Set {
            value: "updated-model-fixture".into(),
        };
        update.secrets.milky_access_token = SecretChange::Clear;
        update.secrets.llbot_token = SecretChange::Set {
            value: "updated-login-fixture".into(),
        };
        let view = save_file(&path, update, &environment).unwrap();
        assert_eq!(view["secrets"]["milky_access_token"]["file_set"], false);
        assert_eq!(view["secrets"]["llm_api_key"]["source"], "file");
        let stored = Config::load_raw(&path).unwrap();
        assert_eq!(stored.llm.api_key, "updated-model-fixture");
        assert_eq!(stored.milky.access_token, None);
        assert_eq!(stored.qq_login.token, "updated-login-fixture");
        assert_eq!(stored.web.token, kept.web.token);

        let mut update = request(&path, &environment);
        update.secrets.llm_api_key = SecretChange::Clear;
        update.secrets.llbot_token = SecretChange::Clear;
        update.secrets.milky_access_token = SecretChange::Set {
            value: "updated-milky-fixture".into(),
        };
        save_file(&path, update, &environment).unwrap();
        let stored = Config::load_raw(&path).unwrap();
        assert!(stored.llm.api_key.is_empty());
        assert!(stored.qq_login.token.is_empty());
        assert_eq!(
            stored.milky.access_token.as_deref(),
            Some("updated-milky-fixture")
        );
    }

    #[test]
    fn environment_overrides_are_visible_as_status_and_never_written_to_disk() {
        let (_directory, path, _) = fixture();
        let environment = Environment {
            llm_api_key: Some("env-model-fixture".into()),
            milky_access_token: Some("env-milky-fixture".into()),
            admin_token: Some("env-admin-fixture-at-least-32-bytes".into()),
            llbot_token: Some("env-login-fixture".into()),
        };
        let mut update = request(&path, &environment);
        update.secrets.llbot_token = SecretChange::Clear;
        let view = save_file(&path, update, &environment).unwrap();
        for name in ["llm_api_key", "milky_access_token", "llbot_token"] {
            assert_eq!(view["secrets"][name]["env_set"], true);
            assert_eq!(view["secrets"][name]["effective_set"], true);
            assert_eq!(view["secrets"][name]["source"], "environment");
        }
        assert_eq!(view["secrets"]["llbot_token"]["file_set"], false);
        assert!(view["config"]["web"].get("token").is_none());
        assert!(view["secrets"].get("admin_token").is_none());
        let bytes = read_file(&path).unwrap();
        assert!(!String::from_utf8(bytes.clone()).unwrap().contains("env-"));
        let active = effective(&path, &bytes, &environment).unwrap();
        assert_eq!(active.llm.api_key, "env-model-fixture");
        assert_eq!(
            active.milky.access_token.as_deref(),
            Some("env-milky-fixture")
        );
        assert_eq!(active.qq_login.token, "env-login-fixture");
        assert_eq!(active.web.token, "env-admin-fixture-at-least-32-bytes");
        assert_eq!(
            Path::new(&active.app.database),
            path.parent().unwrap().join("qkbot.sqlite3")
        );
        assert_eq!(
            Config::load_raw(&path).unwrap().app.database,
            "qkbot.sqlite3"
        );
    }

    #[test]
    fn external_edits_cause_revision_conflict_without_overwrite() {
        let (_directory, path, environment) = fixture();
        let update = request(&path, &environment);
        let mut external = read_file(&path).unwrap();
        external.extend_from_slice(b"\n# externally edited\n");
        fs::write(&path, &external).unwrap();
        let error = save_file(&path, update, &environment).unwrap_err();
        assert!(error.downcast_ref::<RevisionConflict>().is_some());
        assert_eq!(read_file(&path).unwrap(), external);
    }

    #[test]
    fn invalid_values_and_empty_set_do_not_replace_the_file() {
        let (_directory, path, environment) = fixture();
        let original = read_file(&path).unwrap();
        let mut invalid = request(&path, &environment);
        invalid.config.llm.chat_completions_url =
            "https://sensitive-fixture:password@example.invalid/chat".into();
        let error = save_file(&path, invalid, &environment).unwrap_err();
        assert!(!format!("{error:#}").contains("sensitive-fixture"));
        assert_eq!(read_file(&path).unwrap(), original);
        let mut invalid = request(&path, &environment);
        invalid
            .config
            .plugins
            .ai
            .defaults
            .allowed_tools
            .push("does-not-exist".into());
        assert!(save_file(&path, invalid, &environment).is_err());
        assert_eq!(read_file(&path).unwrap(), original);
        let mut invalid = request(&path, &environment);
        invalid.secrets.llm_api_key = SecretChange::Set {
            value: String::new(),
        };
        assert!(save_file(&path, invalid, &environment).is_err());
        assert_eq!(read_file(&path).unwrap(), original);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn administrator_settings_and_plaintext_secret_fields_are_immutable_via_api() {
        let (_directory, path, environment) = fixture();
        let original = read_file(&path).unwrap();
        for index in 0..5 {
            let mut invalid = request(&path, &environment);
            match index {
                0 => invalid.config.web.enabled = false,
                1 => invalid.config.web.listen = "127.0.0.1:9999".into(),
                2 => invalid.config.web.token = "replacement-admin-token-over-32-bytes".into(),
                3 => invalid.config.web.secure_cookie = !invalid.config.web.secure_cookie,
                _ => invalid.config.llm.api_key = "direct-plaintext-fixture".into(),
            }
            assert!(save_file(&path, invalid, &environment).is_err());
            assert_eq!(read_file(&path).unwrap(), original);
        }
    }

    #[test]
    fn configuration_shape_omits_web_token_and_rejects_removed_secret_updates() {
        let (_directory, path, environment) = fixture();
        let original = read_file(&path).unwrap();
        let view = snapshot(&original, &environment).unwrap();
        assert!(view["config"]["web"].get("token").is_none());
        assert!(view["secrets"].get("admin_token").is_none());
        let request: SaveRequest = serde_json::from_value(json!({
            "revision":view["revision"],"config":view["config"]
        }))
        .unwrap();
        assert!(request.config.web.token.is_empty());
        save_file(&path, request, &environment).unwrap();
        assert_eq!(
            Config::load_raw(&path).unwrap().web.token,
            parse(&original).unwrap().web.token
        );
        for action in [
            json!({"action":"keep"}),
            json!({"action":"clear"}),
            json!({"action":"set","value":"unaccepted-field"}),
        ] {
            assert!(
                serde_json::from_value::<SaveRequest>(json!({
                    "revision":view["revision"],"config":view["config"],
                    "secrets":{"admin_token":action}
                }))
                .is_err()
            );
        }
    }

    #[test]
    fn partial_config_requests_are_rejected_and_secret_debug_is_redacted() {
        let (_directory, path, environment) = fixture();
        let mut view = snapshot(&read_file(&path).unwrap(), &environment).unwrap();
        view["config"]["limits"]
            .as_object_mut()
            .unwrap()
            .remove("retention_days");
        assert!(
            serde_json::from_value::<SaveRequest>(
                json!({"revision":view["revision"],"config":view["config"]})
            )
            .is_err()
        );
        let change = SecretChange::Set {
            value: "sensitive-fixture-value".into(),
        };
        assert!(!format!("{change:?}").contains("sensitive-fixture-value"));
        let empty = secret_status("file-value", &Some(String::new()));
        assert_eq!(empty["env_set"], true);
        assert_eq!(empty["effective_set"], false);
        assert_eq!(empty["source"], "environment");
    }
    #[test]
    fn first_start_creates_example_once_and_resolves_database_relative_to_config() {
        let directory = tempfile::tempdir().unwrap();
        let requested = directory.path().join("nested").join("config.toml");
        let path = initialize(requested, &Environment::default()).unwrap();
        let bytes = read_file(&path).unwrap();
        assert_eq!(bytes, include_str!("../../config.example.toml").as_bytes());
        let config = effective(&path, &bytes, &Environment::default()).unwrap();
        assert_eq!(
            Path::new(&config.app.database),
            path.parent().unwrap().join("qkbot.sqlite3")
        );
        fs::write(
            &path,
            [bytes.as_slice(), b"\n# keep external comment\n"].concat(),
        )
        .unwrap();
        let edited = read_file(&path).unwrap();
        initialize(path.clone(), &Environment::default()).unwrap();
        assert_eq!(read_file(&path).unwrap(), edited);
    }
}
