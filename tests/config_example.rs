use qkbot::{config::Config, llm::Llm, milky::Milky};

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
    assert!(config.access.groups.is_empty());
    assert_eq!(config.plugins.ai.defaults.mode, "group");
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
