//! Configurations written before `proxy.providers` existed keep meaning what
//! they meant, and the new per-provider tables mean what they say.
//!
//! The old files are the ones this project shipped, taken from its own history
//! (`packaging/kompressor.toml` and the macOS template at 1.0.2, 1.1.0 and the
//! 1.4.0 commit before provider settings), plus the effective configuration a
//! Harbor run printed on that 1.4.0 build, where every tuned setting was
//! global. For each of them every provider must see exactly the deployment's
//! configuration: a global setting reaches Anthropic and OpenAI traffic alike,
//! as it always did.

use std::sync::Arc;

use kompressor_core::config::{CacheTtl, FrozenPrefix, RetrieveTool};
use kompressor_core::{AppCore, Config, Identity};
use kompressor_store::memory::MemoryRefStore;

const OLD: &[(&str, &str)] = &[
    (
        "packaged-1.0.2",
        include_str!("fixtures/configs/packaged-1.0.2.toml"),
    ),
    (
        "macos-1.0.2",
        include_str!("fixtures/configs/macos-1.0.2.toml"),
    ),
    (
        "packaged-1.1.0",
        include_str!("fixtures/configs/packaged-1.1.0.toml"),
    ),
    (
        "macos-1.1.0",
        include_str!("fixtures/configs/macos-1.1.0.toml"),
    ),
    (
        "packaged-1.4.0",
        include_str!("fixtures/configs/packaged-1.4.0.toml"),
    ),
    (
        "macos-1.4.0",
        include_str!("fixtures/configs/macos-1.4.0.toml"),
    ),
    (
        "effective-1.4.0-tuned-harbor",
        include_str!("fixtures/configs/effective-1.4.0-tuned-harbor.toml"),
    ),
];

const PROVIDERS: &[&str] = &["anthropic", "openai"];

fn parse(name: &str, text: &str) -> Config {
    let config: Config =
        toml::from_str(text).unwrap_or_else(|e| panic!("{name} no longer loads: {e}"));
    config
        .validate()
        .unwrap_or_else(|e| panic!("{name} no longer validates: {e}"));
    config
}

fn core(config: Config) -> AppCore {
    AppCore::new(
        config,
        Arc::new(MemoryRefStore::new()),
        kompressor_stats::writer::null_sink(),
    )
}

/// The whole configuration as `check-config` prints it, which is the one
/// comparison that covers every field.
fn printed(config: &Config) -> String {
    toml::to_string(config).expect("a loaded configuration serialises")
}

#[test]
fn every_configuration_shipped_before_provider_settings_still_loads() {
    for (name, text) in OLD {
        let config = parse(name, text);
        assert!(
            config.proxy.providers.is_empty(),
            "{name} names no provider"
        );
    }
}

#[test]
fn an_old_configuration_applies_to_every_provider_unchanged() {
    for (name, text) in OLD {
        let config = parse(name, text);
        let deployment = printed(&config);
        let core = core(config);
        let identity = Identity::anonymous();
        let options = format!("{:?}", core.options_for(&identity, None));
        for provider in PROVIDERS {
            assert!(
                core.config().for_provider(provider).is_none(),
                "{name}: nothing is scoped to {provider}"
            );
            assert_eq!(
                printed(core.config_for(Some(provider))),
                deployment,
                "{name}: {provider} traffic runs under the deployment's configuration"
            );
            assert_eq!(
                format!(
                    "{:?}",
                    core.options_for_provider(&identity, Some(provider), None)
                ),
                options,
                "{name}: and under its engine options"
            );
        }
    }
}

#[test]
fn global_tuned_settings_still_reach_both_providers() {
    // The run this file was printed for set everything globally; that has to
    // keep meaning "for all traffic", not quietly become "for nobody".
    let config = parse("effective-1.4.0-tuned-harbor", OLD[6].1);
    let core = core(config);
    for provider in PROVIDERS {
        let seen = core.config_for(Some(provider));
        assert_eq!(seen.proxy.cache_ttl, CacheTtl::Adaptive, "{provider}");
        assert_eq!(
            seen.proxy.frozen_prefix,
            FrozenPrefix::FirstSeen,
            "{provider}"
        );
        assert_eq!(seen.proxy.retrieve_tool, RetrieveTool::Always, "{provider}");
        assert!(!seen.engine.enable_code_aware, "{provider}");
    }
}

#[test]
fn printing_an_old_configuration_adds_nothing_to_it() {
    // `check-config` output is read back by tooling (the Harbor runner greps
    // it) and diffed by people. A deployment that sets no provider table must
    // print no provider table, and what it prints must load again.
    for (name, text) in OLD {
        let once = printed(&parse(name, text));
        assert!(
            !once.contains("providers"),
            "{name}: printed a providers table"
        );
        let twice = printed(&parse(name, &once));
        assert_eq!(once, twice, "{name}: the printed form round-trips");
    }
}

#[test]
fn old_environment_overrides_still_apply_to_every_provider() {
    // The variables every earlier benchmark used, applied over a shipped file.
    let mut config = parse("packaged-1.4.0", OLD[4].1);
    config.apply_env(&[
        ("KOMPRESSOR__PROXY__CACHE_TTL".into(), "1h".into()),
        ("KOMPRESSOR__PROXY__FROZEN_PREFIX".into(), "marker".into()),
        ("KOMPRESSOR__PROXY__RETRIEVE_TOOL".into(), "never".into()),
        (
            "KOMPRESSOR__ENGINE__ENABLE_CODE_AWARE".into(),
            "false".into(),
        ),
        ("KOMPRESSOR_MIN_TOKENS".into(), "2048".into()),
    ]);
    config.validate().expect("valid");
    let core = core(config);
    for provider in PROVIDERS {
        let seen = core.config_for(Some(provider));
        assert_eq!(seen.proxy.cache_ttl, CacheTtl::OneHour, "{provider}");
        assert_eq!(seen.proxy.frozen_prefix, FrozenPrefix::Marker, "{provider}");
        assert_eq!(seen.proxy.retrieve_tool, RetrieveTool::Never, "{provider}");
        assert_eq!(seen.proxy.min_tokens, 2048, "{provider}");
        assert!(!seen.engine.enable_code_aware, "{provider}");
    }
}

#[test]
fn the_shipped_configurations_load_and_scope_nothing_by_default() {
    // The example in them is commented out: installing this version changes
    // nothing for a deployment until it opts in.
    for (name, text) in [
        (
            "packaging/kompressor.toml",
            include_str!("../../../packaging/kompressor.toml"),
        ),
        (
            "packaging/macos/config.toml.in",
            include_str!("../../../packaging/macos/config.toml.in"),
        ),
    ] {
        assert!(parse(name, text).proxy.providers.is_empty(), "{name}");
    }
}

#[test]
fn a_provider_table_overrides_only_its_provider_and_inherits_the_rest() {
    let config = parse(
        "codex-and-claude",
        include_str!("fixtures/configs/codex-and-claude.toml"),
    );
    let deployment = printed(&config);
    let core = core(config);

    let anthropic = core.config_for(Some("anthropic"));
    assert_eq!(anthropic.proxy.cache_ttl, CacheTtl::Adaptive);
    assert_eq!(anthropic.proxy.frozen_prefix, FrozenPrefix::FirstSeen);
    assert_eq!(anthropic.proxy.retrieve_tool, RetrieveTool::Always);
    assert!(!anthropic.engine.enable_code_aware);
    // Not named in the table, so the deployment's.
    assert!(anthropic.proxy.websocket_relay);
    assert_eq!(anthropic.proxy.min_tokens, core.config().proxy.min_tokens);
    assert_eq!(anthropic.engine.mode, core.config().engine.mode);
    assert_eq!(
        anthropic.proxy.upstreams.len(),
        core.config().proxy.upstreams.len()
    );

    let openai = core.config_for(Some("openai"));
    assert_eq!(
        printed(openai),
        deployment,
        "openai has no table and sees the deployment"
    );
    assert_eq!(openai.proxy.cache_ttl, CacheTtl::Default);
    assert_eq!(openai.proxy.frozen_prefix, FrozenPrefix::Marker);
    assert!(openai.engine.enable_code_aware);

    assert_eq!(
        printed(core.config_for(None)),
        deployment,
        "traffic of no known family runs under the deployment"
    );
    let identity = Identity::anonymous();
    assert!(
        !core
            .options_for_provider(&identity, Some("anthropic"), None)
            .enable_code_aware
    );
    assert!(
        core.options_for_provider(&identity, Some("openai"), None)
            .enable_code_aware
    );
    assert!(core.options_for(&identity, None).enable_code_aware);
}

#[test]
fn a_new_configuration_round_trips_through_its_printed_form() {
    let config = parse(
        "codex-and-claude",
        include_str!("fixtures/configs/codex-and-claude.toml"),
    );
    let once = printed(&config);
    assert!(once.contains("[proxy.providers.anthropic]"));
    assert!(once.contains("[proxy.providers.anthropic.engine]"));
    let again = parse("printed codex-and-claude", &once);
    assert_eq!(printed(&again), once);
    assert_eq!(
        printed(&again.for_provider("anthropic").unwrap()),
        printed(&config.for_provider("anthropic").unwrap())
    );
}

#[test]
fn a_provider_can_set_a_value_back_to_the_default() {
    // `Some(default)` is an override, not an absence: a deployment that runs
    // everything at one hour can still keep one provider on the client's
    // markers.
    let config = parse(
        "global 1h, anthropic default",
        "[proxy]\ncache_ttl = \"1h\"\nfrozen_prefix = \"first_seen\"\n\
         [proxy.providers.anthropic]\ncache_ttl = \"default\"\n",
    );
    let core = core(config);
    assert_eq!(
        core.config_for(Some("anthropic")).proxy.cache_ttl,
        CacheTtl::Default
    );
    assert_eq!(
        core.config_for(Some("anthropic")).proxy.frozen_prefix,
        FrozenPrefix::FirstSeen,
        "and inherits what it does not name"
    );
    assert_eq!(
        core.config_for(Some("openai")).proxy.cache_ttl,
        CacheTtl::OneHour
    );
}

#[test]
fn global_and_provider_environment_combine_in_either_order() {
    // The environment is unordered; which variable is read first must not
    // decide which one wins. The provider's wins, for its provider only, and a
    // global variable still reaches every field the provider does not name.
    let global = ("KOMPRESSOR__PROXY__CACHE_TTL".to_string(), "1h".to_string());
    let scoped = (
        "KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__CACHE_TTL".to_string(),
        "adaptive".to_string(),
    );
    let min = (
        "KOMPRESSOR__PROXY__MIN_TOKENS".to_string(),
        "4096".to_string(),
    );
    for vars in [
        vec![global.clone(), scoped.clone(), min.clone()],
        vec![scoped.clone(), min.clone(), global.clone()],
    ] {
        let mut config = parse("packaged-1.4.0", OLD[4].1);
        config.apply_env(&vars);
        config.validate().expect("valid");
        let core = core(config);
        assert_eq!(
            core.config_for(Some("anthropic")).proxy.cache_ttl,
            CacheTtl::Adaptive
        );
        assert_eq!(
            core.config_for(Some("openai")).proxy.cache_ttl,
            CacheTtl::OneHour
        );
        assert_eq!(core.config_for(Some("anthropic")).proxy.min_tokens, 4096);
        assert_eq!(core.config_for(Some("openai")).proxy.min_tokens, 4096);
    }
}

#[test]
fn a_provider_table_in_a_file_and_the_environment_merge() {
    // The file scopes some settings, the environment adds another for the same
    // provider: both apply, neither replaces the other.
    let mut config = parse(
        "codex-and-claude",
        include_str!("fixtures/configs/codex-and-claude.toml"),
    );
    config.apply_env(&[(
        "KOMPRESSOR__PROXY__PROVIDERS__ANTHROPIC__MIN_TOKENS".into(),
        "8192".into(),
    )]);
    let core = core(config);
    let anthropic = core.config_for(Some("anthropic"));
    assert_eq!(anthropic.proxy.min_tokens, 8192);
    assert_eq!(
        anthropic.proxy.cache_ttl,
        CacheTtl::Adaptive,
        "the file's setting stands"
    );
    assert!(!anthropic.engine.enable_code_aware);
}
