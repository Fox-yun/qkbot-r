pub mod ai;
pub mod clock;
pub mod reminder;

use crate::{
    config::Config,
    plugin::{PluginRegistry, PluginSpec},
    tools::{ToolCatalogEntry, ToolRegistry},
};
use anyhow::Result;
use std::collections::BTreeSet;

/// Show every registered tool, even when its provider is disabled or an old
/// allow-list names a removed tool. This clone never grants runtime permission.
pub fn tool_catalog(config: &Config) -> Result<Vec<ToolCatalogEntry>> {
    let mut display = config.clone();
    display.plugins.ai.defaults.allowed_tools.clear();
    let (_, tools, _) = assemble(&display)?;
    Ok(tools.catalog())
}

pub fn assemble(config: &Config) -> Result<(PluginRegistry, ToolRegistry, BTreeSet<String>)> {
    let mut plugins = PluginRegistry::new();
    plugins.register(PluginSpec {
        id: "host",
        commands: &["/bot"],
        tools: &[],
        dependencies: &[],
    })?;
    plugins.register(PluginSpec {
        id: "ai",
        commands: &[],
        tools: &[],
        dependencies: &["host"],
    })?;
    plugins.register_handler(std::sync::Arc::new(clock::ClockPlugin))?;
    plugins.register_handler(std::sync::Arc::new(reminder::ReminderPlugin))?;
    let mut enabled = BTreeSet::from(["host".to_owned()]);
    if config.plugins.ai.enabled {
        enabled.insert("ai".into());
    }
    if config.plugins.clock.enabled {
        enabled.insert("clock".into());
    }
    if config.plugins.reminder.enabled {
        enabled.insert("reminder".into());
    }
    plugins.validate(&enabled)?;
    let mut tools = ToolRegistry::new();
    clock::register(&mut tools)?;
    reminder::register(&mut tools, config.plugins.reminder.max_active_per_user)?;
    for name in &config.plugins.ai.defaults.allowed_tools {
        anyhow::ensure!(
            tools.names().any(|registered| registered == name),
            "unknown configured tool: {name}"
        );
    }
    Ok((plugins, tools, enabled))
}
