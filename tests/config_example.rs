use qkbot::{
    config::{AutoReplyConfig, Config},
    llm::Llm,
    milky::Milky,
};

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
    assert!(config.llm.vision);
    assert!(config.plugins.ai.global_memory_shared);
    assert_eq!(config.plugins.ai.memory_input_budget, 8000);
    assert_eq!(config.plugins.ai.auto_reply, AutoReplyConfig::default());
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    Milky::new(config.milky, http.clone()).unwrap();
    Llm::new(config.llm, http).unwrap();
    assert_eq!(config.plugins.ai.defaults.mode, "group");
    assert!(config.plugins.ai.defaults.reply_to_bot);
    assert_eq!(
        config.plugins.ai.defaults.allowed_tools,
        [
            "clock_now",
            "reply_message",
            "search_stickers",
            "reminder_list",
            "reminder_create",
            "reminder_cancel"
        ]
    );
    assert!(!config.plugins.reminder.llm_writes_require_confirmation);
}

#[test]
fn reminder_confirmation_can_be_disabled_or_explicitly_kept() {
    for required in [false, true] {
        let config: Config = toml::from_str(&format!(
            "[plugins.reminder]\nenabled = true\nllm_writes_require_confirmation = {required}"
        ))
        .unwrap();
        config.validate().unwrap();
        let restored: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(
            restored.plugins.reminder.llm_writes_require_confirmation,
            required
        );
        let (_, registry, _) = qkbot::plugins::assemble(&restored).unwrap();
        for name in ["reminder_create", "reminder_cancel"] {
            let entry = registry
                .catalog()
                .into_iter()
                .find(|entry| entry.name == name)
                .unwrap();
            assert_eq!(entry.description.contains("需本人确认"), required);
            assert!(
                restored
                    .plugins
                    .ai
                    .defaults
                    .allowed_tools
                    .iter()
                    .any(|tool| tool == name)
            );
        }
    }
}

#[test]
fn vision_defaults_on_for_legacy_configs_and_explicit_off_roundtrips() {
    let legacy: Config = toml::from_str("[llm]\nmodel = 'legacy-model'").unwrap();
    assert!(legacy.llm.vision);
    let disabled: Config = toml::from_str("[llm]\nvision = false").unwrap();
    assert!(!disabled.llm.vision);
    let roundtrip: Config = toml::from_str(&toml::to_string(&disabled).unwrap()).unwrap();
    assert!(!roundtrip.llm.vision);
    assert!(format!("{:?}", disabled.llm).contains("vision: false"));
}

#[test]
fn global_memory_sharing_defaults_on_and_explicit_off_roundtrips() {
    assert!(Config::default().plugins.ai.global_memory_shared);
    let legacy: Config = toml::from_str("[plugins.ai]\nkeywords = ['小鱼']").unwrap();
    assert!(legacy.plugins.ai.global_memory_shared);
    for shared in [false, true] {
        let config: Config =
            toml::from_str(&format!("[plugins.ai]\nglobal_memory_shared = {shared}")).unwrap();
        config.validate().unwrap();
        assert_eq!(config.plugins.ai.global_memory_shared, shared);
        let roundtrip: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(roundtrip.plugins.ai.global_memory_shared, shared);
        let json_roundtrip: Config =
            serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
        assert_eq!(json_roundtrip.plugins.ai.global_memory_shared, shared);
    }
}

#[test]
fn auto_reply_policy_defaults_are_backward_compatible_and_partial_values_merge() {
    let legacy = Config::from_toml("[plugins.ai]\nkeywords = ['小鱼']").unwrap();
    legacy.validate().unwrap();
    let policy = &legacy.plugins.ai.auto_reply;
    assert_eq!(policy, &AutoReplyConfig::default());
    assert_eq!(policy.cooldown_seconds, 30);
    assert_eq!(policy.debounce_ms, 1500);
    assert_eq!(policy.max_wait_ms, 5000);
    assert_eq!(policy.max_messages, 8);
    assert_eq!(policy.max_age_seconds, 120);
    assert!(!policy.prompt.contains("NO_REPLY"));

    let partial = Config::from_toml("[plugins.ai.auto_reply]\ncooldown_seconds = 0").unwrap();
    partial.validate().unwrap();
    assert_eq!(
        partial.plugins.ai.auto_reply,
        AutoReplyConfig {
            cooldown_seconds: 0,
            ..AutoReplyConfig::default()
        }
    );
    // Policy configuration has no global activation flag; opt-in is stored per chat.
    assert!(Config::from_toml("[plugins.ai.auto_reply]\nenabled = true").is_err());
}

#[test]
fn auto_reply_rejects_invalid_limits_even_when_ai_is_disabled() {
    for enabled in [false, true] {
        for fields in [
            "cooldown_seconds = 3601",
            "debounce_ms = 99",
            "debounce_ms = 10001",
            "max_wait_ms = 99",
            "max_wait_ms = 30001",
            "max_wait_ms = 1499",
            "max_messages = 0",
            "max_messages = 33",
            "max_age_seconds = 4",
            "max_age_seconds = 601",
            "max_age_seconds = 5\nmax_wait_ms = 5001",
            "prompt = '   '",
        ] {
            let config = Config::from_toml(&format!(
                "[plugins.ai]\nenabled = {enabled}\n[plugins.ai.auto_reply]\n{fields}"
            ))
            .unwrap();
            assert!(
                config.validate().is_err(),
                "accepted {fields} with AI enabled={enabled}"
            );
        }
        let mut config = Config::default();
        config.plugins.ai.enabled = enabled;
        config.plugins.ai.auto_reply.max_age_seconds = u64::MAX;
        assert!(config.validate().is_err());
    }
    assert!(Config::from_toml("[plugins.ai.auto_reply]\ncooldown_seconds = -1").is_err());
}

#[test]
fn auto_reply_valid_boundaries_and_trimmed_unicode_prompt_are_supported() {
    for fields in [
        "cooldown_seconds = 0\ndebounce_ms = 100\nmax_wait_ms = 100\nmax_messages = 1\nmax_age_seconds = 5",
        "cooldown_seconds = 3600\ndebounce_ms = 10000\nmax_wait_ms = 30000\nmax_messages = 32\nmax_age_seconds = 600",
        "debounce_ms = 10000\nmax_wait_ms = 10000\nmax_age_seconds = 10",
    ] {
        let config = Config::from_toml(&format!("[plugins.ai.auto_reply]\n{fields}")).unwrap();
        config.validate().unwrap();
    }
    let mut config = Config::default();
    config.plugins.ai.auto_reply.prompt = format!(" \n{}\t ", "聊".repeat(2000));
    config.validate().unwrap();
    config.plugins.ai.auto_reply.prompt = "聊".repeat(2001);
    assert!(config.validate().is_err());
    config.plugins.ai.enabled = false;
    assert!(config.validate().is_err());
    config.plugins.ai.auto_reply.prompt = " \n\t\u{3000} ".into();
    assert!(config.validate().is_err());
}

#[test]
fn auto_reply_custom_policy_roundtrips_through_toml_and_admin_json() {
    let mut config = Config::default();
    config.plugins.ai.auto_reply = AutoReplyConfig {
        prompt: "  只在有新信息时接话。\n看到闲聊时保持克制。  ".into(),
        cooldown_seconds: 90,
        debounce_ms: 2500,
        max_wait_ms: 12_000,
        max_messages: 16,
        max_age_seconds: 45,
    };
    config.validate().unwrap();
    let toml_roundtrip = Config::from_toml(&toml::to_string(&config).unwrap()).unwrap();
    let json_roundtrip: Config =
        serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
    for restored in [toml_roundtrip, json_roundtrip] {
        restored.validate().unwrap();
        assert_eq!(restored.plugins.ai.auto_reply, config.plugins.ai.auto_reply);
    }
}

#[test]
fn final_reply_tool_is_default_but_explicit_permissions_remain_unchanged() {
    let defaults = Config::default().plugins.ai.defaults;
    assert!(
        defaults
            .allowed_tools
            .iter()
            .any(|name| name == "reply_message")
    );
    let legacy: Config =
        toml::from_str("[plugins.ai.defaults]\nallowed_tools = ['clock_now']").unwrap();
    assert_eq!(legacy.plugins.ai.defaults.allowed_tools, ["clock_now"]);
    let empty: Config = toml::from_str("[plugins.ai.defaults]\nallowed_tools = []").unwrap();
    assert!(empty.plugins.ai.defaults.allowed_tools.is_empty());
    let conversation =
        qkbot::plugins::ai::resolve_settings(&defaults, r#"{"allowed_tools":["clock_now"]}"#)
            .unwrap();
    assert_eq!(conversation.allowed_tools, ["clock_now"]);
    let (plugins, tools, _) = qkbot::plugins::assemble(&Config::default()).unwrap();
    assert!(
        plugins
            .specs()
            .find(|plugin| plugin.id == "ai")
            .unwrap()
            .tools
            .contains(&"reply_message")
    );
    assert!(tools.names().any(|name| name == "reply_message"));
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
    assert_eq!(Config::default().plugins.ai.memory_input_budget, 8000);
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
