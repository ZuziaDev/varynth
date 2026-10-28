//! Offline coverage for the optional JSON provider registry.

use serde_json::json;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use tempfile::TempDir;
use varynth::config::provider_registry::{
    AccessRef, ProviderEntry, ProviderRegistry, RegistryModel, MAX_REGISTRY_BYTES,
};
use varynth::config::{Config, ProviderProfile};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn with_home<T>(f: impl FnOnce(&Path) -> T) -> T {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = TempDir::new().unwrap();
    let previous = std::env::var_os("VARYNTH_HOME");
    std::env::set_var("VARYNTH_HOME", dir.path());
    let result = f(dir.path());
    match previous {
        Some(value) => std::env::set_var("VARYNTH_HOME", value),
        None => std::env::remove_var("VARYNTH_HOME"),
    }
    result
}

fn entry(id: &str, models: &[&str]) -> ProviderEntry {
    ProviderEntry {
        id: id.into(),
        name: Some(format!("{id} gateway")),
        kind: "openai".into(),
        base_url: Some("https://gateway.example.test".into()),
        models: models
            .iter()
            .map(|model| ((*model).into(), RegistryModel::default()))
            .collect(),
        model_order: models.iter().map(|model| (*model).into()).collect(),
        ..Default::default()
    }
}

#[test]
fn json_round_trip_preserves_unknown_fields_and_model_metadata() {
    let raw = json!({
        "schemaVersion": 1,
        "providerOrder": ["zeta"],
        "providers": [{
            "id": "zeta",
            "kind": "openai",
            "baseUrl": "https://gateway.example.test",
            "access": {"env": "ZETA_API_KEY"},
            "models": {"model-a": {"displayName": "Model A", "futureModelField": {"x": 1}}},
            "modelOrder": ["model-a"],
            "metadata": {"region": "test"},
            "futureProviderField": {"enabled": true}
        }],
        "futureTopLevel": [1, 2, 3]
    });
    with_home(|home| {
        let path = home.join("config.json");
        fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
        let registry = ProviderRegistry::load(&path).unwrap();
        assert_eq!(registry.ordered_provider_ids(), ["zeta"]);
        assert_eq!(registry.qualified_model_ids(), ["zeta/model-a"]);
        let out = serde_json::to_value(&registry).unwrap();
        assert_eq!(out["futureTopLevel"], json!([1, 2, 3]));
        assert_eq!(
            out["providers"][0]["futureProviderField"],
            json!({"enabled": true})
        );
        assert_eq!(
            out["providers"][0]["models"]["model-a"]["futureModelField"],
            json!({"x": 1})
        );
    });
}

#[test]
fn provider_order_and_model_order_are_deterministic() {
    let mut registry = ProviderRegistry {
        provider_order: vec!["second".into(), "first".into()],
        providers: vec![entry("first", &["b", "a"]), entry("second", &["z", "y"])],
        ..Default::default()
    };
    registry.providers[0].model_order = vec!["a".into(), "b".into()];
    registry.validate().unwrap();
    assert_eq!(registry.ordered_provider_ids(), ["second", "first"]);
    assert_eq!(
        registry.qualified_model_ids(),
        ["second/z", "second/y", "first/a", "first/b"]
    );
}

#[test]
fn model_order_falls_back_to_remaining_model_map_entries() {
    let registry: ProviderRegistry = serde_json::from_value(json!({
        "schemaVersion": 1,
        "providers": [{
            "id": "p",
            "kind": "openai",
            "models": {"b": {}, "a": {}, "c": {}},
            "modelOrder": ["c"]
        }]
    }))
    .unwrap();
    assert_eq!(registry.providers[0].ordered_model_ids(), ["c", "a", "b"]);
}

#[test]
fn json_registry_takes_precedence_and_toml_profiles_are_fallback() {
    with_home(|_home| {
        let mut toml_cfg = Config::default();
        toml_cfg.provider = "legacy".into();
        toml_cfg.provider_profiles = vec![ProviderProfile {
            id: "legacy".into(),
            kind: "openai".into(),
            models: vec!["legacy-model".into()],
            ..Default::default()
        }];
        fs::write(Config::config_path(), toml::to_string(&toml_cfg).unwrap()).unwrap();
        let registry = ProviderRegistry {
            provider_order: vec!["json".into(), "legacy".into()],
            providers: vec![
                entry("legacy", &["json-wins"]),
                entry("json", &["json-model"]),
            ],
            ..Default::default()
        };
        registry.save(&Config::provider_registry_path()).unwrap();
        let loaded = Config::load().unwrap();
        assert_eq!(
            loaded
                .provider_profiles
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            ["json", "legacy"]
        );
        assert_eq!(loaded.provider_profiles[0].models, ["json-model"]);
        assert_eq!(loaded.provider_profiles[1].models, ["json-wins"]);
    });
}

#[test]
fn access_accepts_only_env_and_keyring_references() {
    let ok: ProviderRegistry = serde_json::from_value(json!({
        "schemaVersion": 1,
        "providers": [{
            "id": "p",
            "kind": "anthropic",
            "access": {"env": "ANTHROPIC_API_KEY"}
        }]
    }))
    .unwrap();
    ok.validate().unwrap();

    for access in [
        json!({"apiKey": "literal-secret"}),
        json!({"env": "bad-name-with-dash"}),
        json!({"keyring": "unknown-field"}),
    ] {
        let result = serde_json::from_value::<ProviderRegistry>(json!({
            "schemaVersion": 1,
            "providers": [{"id": "p", "kind": "openai", "access": access}]
        }));
        assert!(
            result.is_err(),
            "unsafe access should be rejected: {result:?}"
        );
    }

    let profile = ProviderProfile {
        id: "p".into(),
        kind: "openai".into(),
        api_key: Some("literal-secret".into()),
        ..Default::default()
    };
    let mut registry = ProviderRegistry::default();
    registry.replace_profiles(&[profile], false).unwrap();
    let serialized = serde_json::to_string(&registry).unwrap();
    assert!(!serialized.contains("literal-secret"));
    assert!(!serialized.contains("api_key"));
}

#[test]
fn registry_catalog_projection_does_not_contain_access_or_secrets() {
    let mut provider = entry("p", &["model"]);
    provider.access = Some(AccessRef {
        env: Some("P_API_KEY".into()),
        keyring: None,
        bearer_env: None,
    });
    let registry = ProviderRegistry {
        providers: vec![provider],
        ..Default::default()
    };
    let catalog = serde_json::to_string(&registry.catalog_models()).unwrap();
    assert!(catalog.contains("p/model"));
    assert!(!catalog.contains("P_API_KEY"));
    assert!(!catalog.contains("access"));
}

#[test]
fn malformed_symlink_and_oversized_registry_are_rejected() {
    with_home(|home| {
        let path = home.join("config.json");
        fs::write(&path, b"not-json").unwrap();
        assert!(ProviderRegistry::load(&path).is_err());

        fs::write(&path, vec![b' '; MAX_REGISTRY_BYTES as usize + 1]).unwrap();
        let error = ProviderRegistry::load(&path).unwrap_err().to_string();
        assert!(error.contains("exceeds"), "{error}");

        let target = home.join("target.json");
        fs::write(&target, b"{}").unwrap();
        let link = home.join("link.json");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(ProviderRegistry::load(&link).is_err());
        }
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&target, &link).is_ok() {
            assert!(ProviderRegistry::load(&link).is_err());
        }
    });
}

#[test]
fn save_is_atomic_and_round_trips_through_config_load() {
    with_home(|home| {
        let mut cfg = Config::default();
        cfg.provider_profiles = vec![ProviderProfile {
            id: "gateway".into(),
            kind: "openai".into(),
            base_url: Some("https://gateway.example.test".into()),
            api_key_env: Some("GATEWAY_API_KEY".into()),
            models: vec!["one".into(), "two".into()],
            ..Default::default()
        }];
        cfg.save().unwrap();
        let raw = fs::read_to_string(home.join("config.json")).unwrap();
        assert!(raw.contains("GATEWAY_API_KEY"));
        assert!(!raw.contains("api_key"));
        let loaded = Config::load().unwrap();
        assert_eq!(loaded.provider_profiles[0].id, "gateway");
        assert_eq!(loaded.provider_profiles[0].models, ["one", "two"]);
    });
}
