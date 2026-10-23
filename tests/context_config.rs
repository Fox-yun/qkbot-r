use qkbot::config::{Config, ContextConfig};

fn context_value(config: &Config) -> serde_json::Value {
    serde_json::to_value(&config.plugins.ai.context).unwrap()
}

#[test]
fn continuity_defaults_and_shipped_example_use_ten_before_and_five_after() {
    let config = Config::default();
    config.validate().unwrap();
    let context = &config.plugins.ai.context;
    assert!(context.enabled);
    assert_eq!(context.recent_seconds, 7200);
    assert_eq!(context.max_turns, 3);
    assert_eq!(context.neighbors_before, 10);
    assert_eq!(context.neighbors_after, 5);
    assert_eq!(context.neighbor_seconds, 120);
    assert_eq!(context.recall_budget, 3000);
    assert_eq!(context.neighbor_budget, 1500);
    let example = Config::from_toml(include_str!("../config.example.toml")).unwrap();
    example.validate().unwrap();
    assert_eq!(context_value(&example), context_value(&config));
}

#[test]
fn legacy_and_partial_configurations_fill_missing_context_defaults() {
    let default = serde_json::to_value(ContextConfig::default()).unwrap();
    for source in [
        "",
        "[plugins.ai]\nkeywords = ['小鱼']\nglobal_memory_shared = false",
        "[plugins.ai.context]",
    ] {
        let legacy = Config::from_toml(source).unwrap();
        legacy.validate().unwrap();
        assert_eq!(context_value(&legacy), default);
        let restored = Config::from_toml(&toml::to_string(&legacy).unwrap()).unwrap();
        assert_eq!(context_value(&restored), default);
    }
    let partial = Config::from_toml("[plugins.ai.context]\nneighbors_before = 0").unwrap();
    assert_eq!(partial.plugins.ai.context.neighbors_before, 0);
    assert_eq!(partial.plugins.ai.context.neighbors_after, 5);
    assert_eq!(partial.plugins.ai.context.recall_budget, 3000);
    partial.validate().unwrap();
}

#[test]
fn explicit_context_configuration_roundtrips_through_toml_and_admin_json() {
    let config = Config::from_toml(
        "[plugins.ai.context]\n\
         enabled = false\n\
         recent_seconds = 3600\n\
         max_turns = 7\n\
         neighbors_before = 0\n\
         neighbors_after = 12\n\
         neighbor_seconds = 45\n\
         recall_budget = 2048\n\
         neighbor_budget = 512",
    )
    .unwrap();
    config.validate().unwrap();
    let toml = Config::from_toml(&toml::to_string(&config).unwrap()).unwrap();
    let json: Config = serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
    assert_eq!(context_value(&toml), context_value(&config));
    assert_eq!(context_value(&json), context_value(&config));
    assert!(!toml.plugins.ai.context.enabled);
    assert_eq!(json.plugins.ai.context.neighbors_after, 12);
}

#[test]
fn context_rejects_out_of_range_limits_even_when_disabled() {
    for enabled in [false, true] {
        for fields in [
            "recent_seconds = 59",
            "recent_seconds = 604801",
            "max_turns = 0",
            "max_turns = 33",
            "neighbors_before = 51",
            "neighbors_after = 51",
            "neighbor_seconds = 0",
            "neighbor_seconds = 3601",
            "recall_budget = 255\nneighbor_budget = 0",
            "recall_budget = 64001",
            "recall_budget = 3000\nneighbor_budget = 3001",
        ] {
            let config = Config::from_toml(&format!(
                "[plugins.ai]\nenabled = {enabled}\n[plugins.ai.context]\nenabled = {enabled}\n{fields}"
            ))
            .unwrap();
            assert!(
                config.validate().is_err(),
                "accepted {fields}, enabled={enabled}"
            );
        }
    }
    let mut overflow = Config::default();
    overflow.plugins.ai.context.recent_seconds = u64::MAX;
    assert!(overflow.validate().is_err());
}

#[test]
fn context_accepts_both_limit_boundaries_and_zero_neighbor_context() {
    for fields in [
        "recent_seconds = 60\nmax_turns = 1\nneighbors_before = 0\nneighbors_after = 0\nneighbor_seconds = 1\nrecall_budget = 256\nneighbor_budget = 0",
        "recent_seconds = 604800\nmax_turns = 32\nneighbors_before = 50\nneighbors_after = 50\nneighbor_seconds = 3600\nrecall_budget = 64000\nneighbor_budget = 64000",
    ] {
        let config = Config::from_toml(&format!("[plugins.ai.context]\n{fields}")).unwrap();
        config.validate().unwrap();
    }
}

#[test]
fn neighbor_allowance_is_a_sub_budget_of_continuity_recall() {
    for (recall, neighbor, valid) in [
        (256, 0, true),
        (256, 256, true),
        (256, 257, false),
        (3000, 1500, true),
        (64000, 64000, true),
        (64000, 64001, false),
    ] {
        let config = Config::from_toml(&format!(
            "[plugins.ai.context]\nrecall_budget = {recall}\nneighbor_budget = {neighbor}"
        ))
        .unwrap();
        assert_eq!(
            config.validate().is_ok(),
            valid,
            "recall={recall}, neighbor={neighbor}"
        );
    }
}

#[test]
fn unknown_context_fields_and_negative_unsigned_values_fail_parsing() {
    for fields in [
        "recall_budegt = 3000",
        "recent_seconds = -1",
        "max_turns = -1",
        "neighbors_before = -1",
        "neighbors_after = -1",
        "neighbor_seconds = -1",
        "recall_budget = -1",
        "neighbor_budget = -1",
    ] {
        assert!(
            Config::from_toml(&format!("[plugins.ai.context]\n{fields}")).is_err(),
            "accepted {fields}"
        );
    }
}
