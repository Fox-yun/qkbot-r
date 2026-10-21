use qkbot::{config::Config, llm::Llm, milky::Milky};

#[test]
fn external_presets_are_valid_disabled_and_explicit_lists_take_precedence() {
    let config = Config::default();
    config.validate().unwrap();
    assert_eq!(config.nonebot.plugins.len(), 6);
    assert!(!config.nonebot.enabled && config.nonebot.plugins.iter().all(|plugin| !plugin.enabled));
    let explicit: Config = toml::from_str("[nonebot]\nplugins = []").unwrap();
    assert!(explicit.nonebot.plugins.is_empty());
    let mut large_files = config;
    large_files.nonebot.media_max_bytes = 500 * 1024 * 1024;
    large_files.validate().unwrap();
}

#[test]
fn shipped_example_parses_and_constructs_clients_without_network() {
    let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
    config.validate().unwrap();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    Milky::new(config.milky, http.clone()).unwrap();
    Llm::new(config.llm, http).unwrap();
    assert_eq!(config.plugins.ai.defaults.mode, "group");
    assert!(config.plugins.ai.defaults.reply_to_bot);
}

#[test]
fn reply_trigger_defaults_and_conversation_overrides_are_backward_compatible() {
    let legacy: Config = toml::from_str("[plugins.ai.defaults]\nmode = 'user'").unwrap();
    assert!(legacy.plugins.ai.defaults.reply_to_bot);
    let defaults = legacy.plugins.ai.defaults;
    let disabled =
        qkbot::plugins::ai::resolve_settings(&defaults, r#"{"reply_to_bot":false}"#).unwrap();
    assert!(!disabled.reply_to_bot);
    let serialized = serde_json::to_string(&disabled).unwrap();
    assert!(
        !qkbot::plugins::ai::resolve_settings(&defaults, &serialized)
            .unwrap()
            .reply_to_bot
    );
    assert!(
        qkbot::plugins::ai::resolve_settings(&disabled, r#"{"reply_to_bot":true}"#)
            .unwrap()
            .reply_to_bot
    );
}

#[test]
fn legacy_access_cannot_silently_expand_permissions_during_upgrade() {
    for source in [
        "[access]\ngroups = [123]\nfriends = []\nadmins = [456]",
        "[access]",
        "[plugins.ai]\nenabled = false\n[access]\ngroups = []",
    ] {
        let error = toml::from_str::<Config>(source).unwrap_err();
        assert!(error.to_string().contains("已阻止自动切换访问策略"));
        let message = Config::from_toml(source).unwrap_err().to_string();
        assert!(message.contains("手动移除旧 [access]"));
        assert!(!message.contains("456"));
    }
    assert!(serde_json::from_value::<Config>(serde_json::json!({"access":null})).is_err());
    let config = Config::default();
    assert!(
        serde_json::to_value(&config)
            .unwrap()
            .get("access")
            .is_none()
    );
    assert!(!toml::to_string(&config).unwrap().contains("[access]"));
}

#[test]
fn unknown_ai_settings_and_unknown_top_level_keys_are_rejected() {
    let source = include_str!("../config.example.toml");
    assert!(
        toml::from_str::<Config>(&source.replace("input_budget = 6000", "imput_budget = 6000"))
            .is_err()
    );
    assert!(toml::from_str::<Config>(&format!("unexpected = true\n{source}")).is_err());
}

#[test]
fn disabling_ai_does_not_require_a_model_endpoint() {
    let config: Config =
        toml::from_str("[plugins.ai]\nenabled = false\n[plugins.clock]\nenabled = true").unwrap();
    config.validate().unwrap();
    assert!(config.llm.chat_completions_url.is_empty());
}

#[test]
fn larger_workloads_are_configurable_without_changing_lightweight_defaults() {
    let mut config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
    assert_eq!(config.plugins.ai.max_memories_per_conversation, 256);
    assert_eq!(config.plugins.ai.memory_recall_limit, 5);
    assert_eq!(config.plugins.ai.defaults.input_budget, 6000);
    config.plugins.ai.max_memories_per_conversation = 10_000;
    config.plugins.ai.memory_recall_limit = 20;
    config.plugins.ai.memory_recall_budget = 8000;
    config.plugins.ai.defaults.input_budget = 128_000;
    config.plugins.ai.defaults.output_budget = 16_000;
    config.plugins.ai.defaults.recent_messages = 512;
    config.plugins.ai.memory_input_budget = 64_000;
    config.limits.max_model_calls = 5;
    config.limits.max_tool_calls = 4;
    config.limits.max_http_attempts = 5;
    config.limits.turn_timeout_secs = 300;
    config.limits.tool_timeout_secs = 60;
    config.validate().unwrap();

    // Capacity is a finite, explicit storage policy rather than an RSS target.
    config.plugins.ai.max_memories_per_conversation = 0;
    assert!(config.validate().is_err());
}
