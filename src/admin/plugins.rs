//! Per-contact plugin blacklists. Missing rows are enabled, never an allowlist.
use super::{contacts::ContactsService, runtime::Runtime};
use crate::{
    catalog,
    store::Store,
    types::{Scene, Scope},
};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy)]
pub enum PluginsError {
    Invalid,
    Stale,
    Offline,
}
impl PluginsError {
    pub fn status(self) -> axum::http::StatusCode {
        match self {
            Self::Invalid => axum::http::StatusCode::BAD_REQUEST,
            Self::Stale => axum::http::StatusCode::CONFLICT,
            Self::Offline => axum::http::StatusCode::BAD_GATEWAY,
        }
    }
    pub fn code(self) -> &'static str {
        match self {
            Self::Invalid => "plugins_invalid",
            Self::Stale => "plugins_stale",
            Self::Offline => "plugins_offline",
        }
    }
}
impl std::fmt::Display for PluginsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "插件或会话范围无效",
            Self::Stale => "账号、插件配置或开关已变化，请刷新后重试",
            Self::Offline => "无法确认当前 QQ 列表，请检查登录和连接状态",
        })
    }
}
impl std::error::Error for PluginsError {}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeQuery {
    pub bot_id: String,
    pub scene: Scene,
    pub peer_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TogglePlugin {
    pub bot_id: String,
    pub scene: Scene,
    pub peer_id: String,
    pub plugin: String,
    pub enabled: bool,
    pub revision: i64,
}
#[derive(Serialize)]
pub struct PluginView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub runtime: String,
    pub commands: Vec<String>,
    pub group_only: bool,
    pub globally_enabled: bool,
    pub enabled: bool,
    pub revision: i64,
}
#[derive(Serialize)]
pub struct ScopePlugins {
    pub bot_id: String,
    pub scene: Scene,
    pub peer_id: String,
    pub plugins: Vec<PluginView>,
}
fn parse(query: &ScopeQuery) -> Result<Scope> {
    fn id(value: &str) -> Result<i64> {
        if value.is_empty() || value.len() > 19 || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(PluginsError::Invalid.into());
        }
        value
            .parse()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| PluginsError::Invalid.into())
    }
    Ok(Scope {
        bot_id: id(&query.bot_id)?,
        scene: query.scene,
        peer_id: id(&query.peer_id)?,
    })
}
async fn verify(contacts: &ContactsService, scope: &Scope, force: bool) -> Result<()> {
    let view = contacts
        .list(force)
        .await
        .map_err(|_| PluginsError::Offline)?;
    if view.bot_id != scope.bot_id.to_string() {
        return Err(PluginsError::Stale.into());
    }
    let entries = match scope.scene {
        Scene::Group => view.groups,
        Scene::Friend => view.friends,
    };
    if !entries
        .iter()
        .any(|entry| entry.id == scope.peer_id.to_string())
    {
        return Err(PluginsError::Invalid.into());
    }
    Ok(())
}
async fn read(
    store: &Store,
    scope: Scope,
    features: Vec<catalog::Feature>,
) -> Result<ScopePlugins> {
    store.call(move|db|{
        let mut plugins=Vec::with_capacity(features.len());
        for feature in features {
            let (enabled,revision)=db.query_row("SELECT enabled,revision FROM scope_plugin_access WHERE scope=?1 AND plugin=?2",params![scope.key(),feature.id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.unwrap_or((true,0));
            plugins.push(PluginView{id:feature.id,name:feature.name,description:feature.description,runtime:feature.runtime,commands:feature.commands,group_only:feature.group_only,globally_enabled:feature.enabled,enabled,revision});
        }
        Ok(ScopePlugins{bot_id:scope.bot_id.to_string(),scene:scope.scene,peer_id:scope.peer_id.to_string(),plugins})
    }).await
}
async fn running_features(runtime: &Runtime) -> Vec<catalog::Feature> {
    let config = runtime.config().await;
    let loaded = runtime.loaded_plugins().await;
    let mut features = catalog::features(&config);
    for feature in &mut features {
        feature.enabled = loaded.contains(&feature.id);
    }
    features
}
pub async fn list(
    contacts: &ContactsService,
    runtime: &Runtime,
    query: ScopeQuery,
) -> Result<ScopePlugins> {
    let scope = parse(&query)?;
    verify(contacts, &scope, false).await?;
    let features = running_features(runtime).await;
    read(&runtime.data().await?, scope, features).await
}
async fn set(
    store: &Store,
    scope: Scope,
    plugin: String,
    enabled: bool,
    expected: i64,
) -> Result<()> {
    if expected < 0 {
        return Err(PluginsError::Invalid.into());
    }
    let _gate = store.scope_gate(&scope.key())?.write_owned().await;
    store.call(move|db|{
        let tx=db.transaction()?;
        let current:(bool,i64)=tx.query_row("SELECT enabled,revision FROM scope_plugin_access WHERE scope=?1 AND plugin=?2",params![scope.key(),plugin],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.unwrap_or((true,0));
        if current.1!=expected{return Err(PluginsError::Stale.into())}
        if current.0!=enabled {
            tx.execute("INSERT INTO scope_plugin_access(scope,plugin,enabled,revision) VALUES(?1,?2,?3,1) ON CONFLICT(scope,plugin) DO UPDATE SET enabled=excluded.enabled,revision=scope_plugin_access.revision+1",params![scope.key(),plugin,enabled])?;
        }
        tx.commit()?;Ok(())
    }).await
}
pub async fn toggle(
    contacts: &ContactsService,
    runtime: &Runtime,
    request: TogglePlugin,
) -> Result<ScopePlugins> {
    let scope = parse(&ScopeQuery {
        bot_id: request.bot_id,
        scene: request.scene,
        peer_id: request.peer_id,
    })?;
    let features = running_features(runtime).await;
    if !features.iter().any(|feature| feature.id == request.plugin) {
        return Err(PluginsError::Invalid.into());
    }
    let generation = runtime.status().await.runtime_generation;
    // Narrowing an explicitly named scope remains possible during a QQ outage.
    if request.enabled {
        verify(contacts, &scope, true).await?;
        if runtime.status().await.runtime_generation != generation {
            return Err(PluginsError::Stale.into());
        }
    }
    let store = runtime.data().await?;
    set(
        &store,
        scope.clone(),
        request.plugin,
        request.enabled,
        request.revision,
    )
    .await?;
    read(&store, scope, features).await
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope(bot_id: i64) -> Scope {
        Scope {
            bot_id,
            scene: Scene::Group,
            peer_id: 20,
        }
    }
    fn features() -> Vec<catalog::Feature> {
        catalog::features(&crate::config::Config::default())
    }
    #[tokio::test]
    async fn defaults_open_cas_account_isolation_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switches.db");
        let store = Store::open(&path, 32).await.unwrap();
        let initial = read(&store, scope(1), features()).await.unwrap();
        assert_eq!(initial.plugins.len(), 18);
        assert_eq!(
            initial
                .plugins
                .iter()
                .filter(|p| p.runtime == "rust")
                .count(),
            12
        );
        assert_eq!(
            initial
                .plugins
                .iter()
                .filter(|p| p.runtime == "nonebot")
                .count(),
            6
        );
        assert!(initial.plugins.iter().all(|p| p.enabled && p.revision == 0));
        set(&store, scope(1), "wordle".into(), false, 0)
            .await
            .unwrap();
        assert!(
            set(&store, scope(1), "wordle".into(), true, 0)
                .await
                .unwrap_err()
                .is::<PluginsError>()
        );
        assert!(
            read(&store, scope(2), features())
                .await
                .unwrap()
                .plugins
                .iter()
                .all(|p| p.enabled)
        );
        store.stop().await.unwrap();
        let store = Store::open(&path, 32).await.unwrap();
        let view = read(&store, scope(1), features()).await.unwrap();
        let wordle = view.plugins.iter().find(|p| p.id == "wordle").unwrap();
        assert!(!wordle.enabled);
        assert_eq!(wordle.revision, 1);
        set(&store, scope(1), "wordle".into(), true, 1)
            .await
            .unwrap();
        let view = read(&store, scope(1), features()).await.unwrap();
        let wordle = view.plugins.iter().find(|p| p.id == "wordle").unwrap();
        assert!(wordle.enabled);
        assert_eq!(wordle.revision, 2);
        store.stop().await.unwrap();
    }
}
