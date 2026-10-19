//! Statically linked, trusted plugins. This registry is not a security sandbox.
use crate::{host::Host, types::Incoming};
use anyhow::{Result, bail};
use async_trait::async_trait;
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::Arc,
    time::Duration,
};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct PluginSpec {
    pub id: &'static str,
    pub commands: &'static [&'static str],
    pub tools: &'static [&'static str],
    pub dependencies: &'static [&'static str],
}

/// Source plugins are trusted code. Host whitelist, queues and send ledger still
/// apply; this small interface does not load scripts or dynamic libraries.
#[async_trait]
pub trait Plugin: Send + Sync {
    fn spec(&self) -> PluginSpec;
    async fn init(&self, _host: &Host) -> Result<()> {
        Ok(())
    }
    async fn stop(&self, _host: &Host) -> Result<()> {
        Ok(())
    }
    fn accepts(&self, _message: &Incoming) -> bool {
        true
    }
    async fn command(&self, host: &Host, message: &Incoming) -> Result<String>;
    fn observe_messages(&self) -> bool {
        false
    }
    async fn on_message(&self, _host: &Host, _message: &Incoming) -> Result<()> {
        Ok(())
    }
}

#[derive(Default, Clone)]
pub struct PluginRegistry {
    specs: BTreeMap<&'static str, PluginSpec>,
    handlers: BTreeMap<&'static str, Arc<dyn Plugin>>,
    enabled: BTreeSet<String>,
}
impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, spec: PluginSpec) -> Result<()> {
        if self.specs.contains_key(spec.id) {
            bail!("duplicate plugin: {}", spec.id);
        }
        self.specs.insert(spec.id, spec);
        Ok(())
    }
    pub fn specs(&self) -> impl Iterator<Item = &PluginSpec> {
        self.specs.values()
    }
    pub fn register_handler(&mut self, handler: Arc<dyn Plugin>) -> Result<()> {
        let spec = handler.spec();
        let id = spec.id;
        self.register(spec)?;
        self.handlers.insert(id, handler);
        Ok(())
    }
    pub fn handler_for(&self, command: &str) -> Option<Arc<dyn Plugin>> {
        self.handlers
            .iter()
            .find(|(id, handler)| {
                self.enabled.contains(**id) && handler.spec().commands.contains(&command)
            })
            .map(|(_, handler)| handler.clone())
    }
    pub fn observers(&self) -> Vec<Arc<dyn Plugin>> {
        self.handlers
            .iter()
            .filter(|(id, handler)| self.enabled.contains(**id) && handler.observe_messages())
            .map(|(_, handler)| handler.clone())
            .collect()
    }
    /// Dependencies initialize first. Shutdown should traverse the result in reverse.
    pub fn enabled_handlers(&self) -> Vec<Arc<dyn Plugin>> {
        fn append(
            registry: &PluginRegistry,
            id: &str,
            visited: &mut BTreeSet<String>,
            out: &mut Vec<Arc<dyn Plugin>>,
        ) {
            if !registry.enabled.contains(id) || !visited.insert(id.to_owned()) {
                return;
            }
            if let Some(spec) = registry.specs.get(id) {
                for dependency in spec.dependencies {
                    append(registry, dependency, visited, out);
                }
            }
            if let Some(handler) = registry.handlers.get(id) {
                out.push(handler.clone());
            }
        }
        let mut handlers = Vec::new();
        let mut visited = BTreeSet::new();
        for id in &self.enabled {
            append(self, id, &mut visited, &mut handlers);
        }
        handlers
    }
    pub fn validate(&mut self, enabled: &BTreeSet<String>) -> Result<()> {
        let mut commands = BTreeSet::new();
        let mut tools = BTreeSet::new();
        for id in enabled {
            let spec = self
                .specs
                .get(id.as_str())
                .ok_or_else(|| anyhow::anyhow!("unknown plugin: {id}"))?;
            for cmd in spec.commands {
                if !commands.insert(*cmd) {
                    bail!("duplicate command: {cmd}");
                }
            }
            for tool in spec.tools {
                if !tools.insert(*tool) {
                    bail!("duplicate tool: {tool}");
                }
            }
            for dep in spec.dependencies {
                if !enabled.contains(*dep) {
                    bail!("plugin {} requires enabled plugin {dep}", spec.id);
                }
            }
        }
        fn visit<'a>(
            id: &'a str,
            registry: &'a PluginRegistry,
            visiting: &mut BTreeSet<&'a str>,
            done: &mut BTreeSet<&'a str>,
        ) -> Result<()> {
            if done.contains(id) {
                return Ok(());
            }
            if !visiting.insert(id) {
                bail!("plugin dependency cycle at {id}");
            }
            let spec = registry
                .specs
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("missing plugin {id}"))?;
            for dep in spec.dependencies {
                visit(dep, registry, visiting, done)?;
            }
            visiting.remove(id);
            done.insert(id);
            Ok(())
        }
        let mut done = BTreeSet::new();
        for id in enabled {
            visit(id, self, &mut BTreeSet::new(), &mut done)?;
        }
        self.enabled = enabled.clone();
        Ok(())
    }
}

/// Owns all asynchronous tasks belonging to a plugin. Dropping also aborts tasks.
pub struct PluginRuntime {
    pub id: &'static str,
    pub epoch: i64,
    pub cancel: CancellationToken,
    tasks: JoinSet<()>,
}
impl PluginRuntime {
    pub fn new(id: &'static str, epoch: i64, parent: &CancellationToken) -> Self {
        Self {
            id,
            epoch,
            cancel: parent.child_token(),
            tasks: JoinSet::new(),
        }
    }
    pub fn spawn(&mut self, task: impl Future<Output = ()> + Send + 'static) {
        let token = self.cancel.clone();
        self.tasks.spawn(async move {
            tokio::select! { _ = token.cancelled() => {}, _ = task => {} }
        });
    }
    pub async fn shutdown(mut self, grace: Duration) {
        self.cancel.cancel();
        if tokio::time::timeout(grace, async {
            while self.tasks.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            self.tasks.abort_all();
            while self.tasks.join_next().await.is_some() {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_duplicate_commands_and_cycles() {
        let mut registry = PluginRegistry::new();
        registry
            .register(PluginSpec {
                id: "a",
                commands: &["/same"],
                tools: &[],
                dependencies: &[],
            })
            .unwrap();
        registry
            .register(PluginSpec {
                id: "b",
                commands: &["/same"],
                tools: &[],
                dependencies: &[],
            })
            .unwrap();
        assert!(registry.validate(&["a".into(), "b".into()].into()).is_err());
        let mut registry = PluginRegistry::new();
        registry
            .register(PluginSpec {
                id: "a",
                commands: &[],
                tools: &[],
                dependencies: &["b"],
            })
            .unwrap();
        registry
            .register(PluginSpec {
                id: "b",
                commands: &[],
                tools: &[],
                dependencies: &["a"],
            })
            .unwrap();
        assert!(registry.validate(&["a".into(), "b".into()].into()).is_err());
    }

    #[test]
    fn only_enabled_command_handlers_are_exposed() {
        let mut registry = PluginRegistry::new();
        registry
            .register(PluginSpec {
                id: "host",
                commands: &["/bot"],
                tools: &[],
                dependencies: &[],
            })
            .unwrap();
        registry
            .register_handler(Arc::new(crate::plugins::clock::ClockPlugin))
            .unwrap();
        registry.validate(&["host".into()].into()).unwrap();
        assert!(registry.handler_for("/clock").is_none());
        registry
            .validate(&["host".into(), "clock".into()].into())
            .unwrap();
        assert_eq!(registry.handler_for("/clock").unwrap().spec().id, "clock");
        assert!(registry.handler_for("/other").is_none());
        assert!(registry.observers().is_empty());
    }
}
