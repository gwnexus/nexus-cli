//! Tests for nexus_core::config module.

use nexus_core::config::{
    save_project_config, Config, ConfigSource, LocalConfigOverrides, McpSource, OutputPreference,
    ProjectConfig,
};
use std::fs;
use std::path::PathBuf;

/// Helper: create a unique temp directory for each test.
fn temp_dir(suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nexus-config-layer-test-{}-{}",
        std::process::id(),
        suffix
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_config_default_values() {
    let config = Config::default();
    assert_eq!(config.api_url, "https://nexus.gatewarden.eu");
    assert_eq!(config.default_output, OutputPreference::Table);
    assert!(!config.no_color);
    assert_eq!(config.mcp_source, McpSource::Npm);
    assert_eq!(config.run.launch_countdown_secs, 5);
}

#[test]
fn test_config_path_uses_xdg_style() {
    let path = Config::path().unwrap();
    let path_str = path.to_string_lossy();

    assert!(
        path_str.contains(".config/nexus/config.toml"),
        "expected XDG-style path, got: {}",
        path_str
    );
}

#[test]
fn test_config_dir_path() {
    let dir = Config::dir().unwrap();
    let dir_str = dir.to_string_lossy();

    assert!(dir_str.ends_with(".config/nexus"));
}

#[test]
fn test_config_toml_roundtrip() {
    let config = Config {
        api_url: "https://custom.nexus.api".to_string(),
        default_output: OutputPreference::Json,
        no_color: true,
        mcp_source: McpSource::Local,
        check_updates: false,
        run: nexus_core::config::RunConfig::default(),
    };

    let serialized = toml::to_string_pretty(&config).unwrap();
    let deserialized: Config = toml::from_str(&serialized).unwrap();

    assert_eq!(deserialized.api_url, config.api_url);
    assert_eq!(deserialized.default_output, config.default_output);
    assert_eq!(deserialized.no_color, config.no_color);
    assert_eq!(deserialized.mcp_source, config.mcp_source);
}

#[test]
fn test_config_backward_compat_minimal() {
    // Simulate an old config file with only api_url
    let toml_str = r#"api_url = "https://legacy.nexus.api""#;
    let config: Config = toml::from_str(toml_str).unwrap();

    assert_eq!(config.api_url, "https://legacy.nexus.api");
    // Defaults should fill in
    assert_eq!(config.default_output, OutputPreference::Table);
    assert!(!config.no_color);
    assert_eq!(config.mcp_source, McpSource::Npm);
    assert!(config.check_updates); // default true
}

#[test]
fn test_config_set_invalid_check_updates() {
    let mut config = Config::default();
    let result = config.set("check_updates", "maybe");
    assert!(result.is_err());
}

#[test]
fn test_output_preference_display() {
    assert_eq!(OutputPreference::Table.to_string(), "table");
    assert_eq!(OutputPreference::Json.to_string(), "json");
    assert_eq!(OutputPreference::Plain.to_string(), "plain");
}

#[test]
fn test_output_preference_from_str() {
    assert_eq!(
        "table".parse::<OutputPreference>().unwrap(),
        OutputPreference::Table
    );
    assert_eq!(
        "json".parse::<OutputPreference>().unwrap(),
        OutputPreference::Json
    );
    assert_eq!(
        "plain".parse::<OutputPreference>().unwrap(),
        OutputPreference::Plain
    );
    assert_eq!(
        "JSON".parse::<OutputPreference>().unwrap(),
        OutputPreference::Json
    );
}

#[test]
fn test_output_preference_from_str_invalid() {
    assert!("xml".parse::<OutputPreference>().is_err());
    assert!("".parse::<OutputPreference>().is_err());
}

#[test]
fn test_output_preference_serde_roundtrip() {
    let prefs = vec![
        OutputPreference::Table,
        OutputPreference::Json,
        OutputPreference::Plain,
    ];
    for pref in prefs {
        let json = serde_json::to_string(&pref).unwrap();
        let deserialized: OutputPreference = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, pref);
    }
}

#[test]
fn test_config_set_valid_keys() {
    let mut config = Config::default();

    config.set("api_url", "https://new.url").unwrap();
    assert_eq!(config.api_url, "https://new.url");

    config.set("default_output", "json").unwrap();
    assert_eq!(config.default_output, OutputPreference::Json);

    config.set("no_color", "true").unwrap();
    assert!(config.no_color);

    config.set("mcp_source", "local").unwrap();
    assert_eq!(config.mcp_source, McpSource::Local);

    config.set("mcp_source", "npm").unwrap();
    assert_eq!(config.mcp_source, McpSource::Npm);

    config.set("check_updates", "false").unwrap();
    assert!(!config.check_updates);

    config.set("check_updates", "true").unwrap();
    assert!(config.check_updates);

    config.set("run.launch_countdown_secs", "10").unwrap();
    assert_eq!(config.run.launch_countdown_secs, 10);

    config.set("run.launch_countdown_secs", "0").unwrap();
    assert_eq!(config.run.launch_countdown_secs, 0);
}

#[test]
fn test_config_set_invalid_key() {
    let mut config = Config::default();
    let result = config.set("unknown_key", "value");
    assert!(result.is_err());
}

#[test]
fn test_config_set_invalid_bool() {
    let mut config = Config::default();
    let result = config.set("no_color", "maybe");
    assert!(result.is_err());
}

#[test]
fn test_config_set_invalid_output() {
    let mut config = Config::default();
    let result = config.set("default_output", "xml");
    assert!(result.is_err());
}

#[test]
fn test_config_set_invalid_mcp_source() {
    let mut config = Config::default();
    let result = config.set("mcp_source", "docker");
    assert!(result.is_err());
}

#[test]
fn test_run_config_launch_countdown_secs_default() {
    let run = nexus_core::config::RunConfig::default();
    assert_eq!(run.launch_countdown_secs, 5);
}

#[test]
fn test_run_config_launch_countdown_secs_toml_roundtrip() {
    let mut config = Config::default();
    config.set("run.launch_countdown_secs", "3").unwrap();

    let serialized = toml::to_string_pretty(&config).unwrap();
    let deserialized: Config = toml::from_str(&serialized).unwrap();
    assert_eq!(deserialized.run.launch_countdown_secs, 3);
}

#[test]
fn test_run_config_launch_countdown_secs_zero() {
    let mut config = Config::default();
    config.set("run.launch_countdown_secs", "0").unwrap();
    assert_eq!(config.run.launch_countdown_secs, 0);
}

#[test]
fn test_config_set_invalid_countdown_secs() {
    let mut config = Config::default();
    assert!(config.set("run.launch_countdown_secs", "five").is_err());
    assert!(config.set("run.launch_countdown_secs", "-1").is_err());
    assert!(config.set("run.launch_countdown_secs", "").is_err());
}

#[test]
fn test_config_backward_compat_no_countdown() {
    // Old config without [run] section — defaults should fill in
    let toml_str = r#"api_url = "https://legacy.nexus.api""#;
    let config: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(config.run.launch_countdown_secs, 5);
}

#[test]
fn test_mcp_source_display() {
    assert_eq!(McpSource::Npm.to_string(), "npm");
    assert_eq!(McpSource::Local.to_string(), "local");
}

#[test]
fn test_mcp_source_from_str() {
    assert_eq!("npm".parse::<McpSource>().unwrap(), McpSource::Npm);
    assert_eq!("local".parse::<McpSource>().unwrap(), McpSource::Local);
    assert_eq!("NPM".parse::<McpSource>().unwrap(), McpSource::Npm);
    assert_eq!("LOCAL".parse::<McpSource>().unwrap(), McpSource::Local);
}

#[test]
fn test_mcp_source_from_str_invalid() {
    assert!("docker".parse::<McpSource>().is_err());
    assert!("".parse::<McpSource>().is_err());
}

#[test]
fn test_mcp_source_serde_roundtrip() {
    let sources = vec![McpSource::Npm, McpSource::Local];
    for src in sources {
        let json = serde_json::to_string(&src).unwrap();
        let deserialized: McpSource = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, src);
    }
}

// ---------------------------------------------------------------------------
// Layered config: project-local overrides vs global (git-style precedence)
// ---------------------------------------------------------------------------

#[test]
fn test_local_config_overrides_set_valid_keys() {
    let mut overrides = LocalConfigOverrides::default();

    overrides
        .set("api_url", "https://staging.nexus.api")
        .unwrap();
    assert_eq!(
        overrides.api_url.as_deref(),
        Some("https://staging.nexus.api")
    );

    overrides.set("default_output", "json").unwrap();
    assert_eq!(overrides.default_output, Some(OutputPreference::Json));

    overrides.set("no_color", "true").unwrap();
    assert_eq!(overrides.no_color, Some(true));
}

#[test]
fn test_local_config_overrides_set_invalid_key() {
    let mut overrides = LocalConfigOverrides::default();
    // mcp_source is a global-only key; not supported for --local scope.
    assert!(overrides.set("mcp_source", "local").is_err());
    assert!(overrides.set("unknown_key", "value").is_err());
}

#[test]
fn test_local_config_overrides_set_invalid_bool() {
    let mut overrides = LocalConfigOverrides::default();
    assert!(overrides.set("no_color", "maybe").is_err());
}

#[test]
fn test_project_config_toml_roundtrip_with_local_overrides() {
    let project_config = ProjectConfig {
        project: None,
        mcp: None,
        mcp_extra: None,
        plugins: None,
        config: Some(LocalConfigOverrides {
            api_url: Some("https://staging.nexus.api".to_string()),
            default_output: Some(OutputPreference::Json),
            no_color: Some(true),
        }),
    };

    let serialized = toml::to_string_pretty(&project_config).unwrap();
    assert!(serialized.contains("[config]"));
    assert!(serialized.contains("staging.nexus.api"));

    let deserialized: ProjectConfig = toml::from_str(&serialized).unwrap();
    let overrides = deserialized
        .config
        .expect("config section should roundtrip");
    assert_eq!(
        overrides.api_url.as_deref(),
        Some("https://staging.nexus.api")
    );
    assert_eq!(overrides.default_output, Some(OutputPreference::Json));
    assert_eq!(overrides.no_color, Some(true));
}

#[test]
fn test_project_config_without_config_section_omits_it() {
    // Zero migration cost: a ProjectConfig with no local overrides must not
    // emit an empty `[config]` section into .nexus/config.toml.
    let project_config = ProjectConfig::default();
    let serialized = toml::to_string_pretty(&project_config).unwrap();
    assert!(!serialized.contains("[config]"));
}

#[test]
fn test_load_effective_falls_back_to_default_without_any_config() {
    let dir = temp_dir("no-config-anywhere");
    // No global config file exists at HOME in this sandboxed check; we only
    // control the local layer here, so assert the local-absent case behaves
    // like plain Config::load(): default values, Default provenance or
    // Global provenance depending on whether a global file happens to exist
    // on the test machine. We only assert what local-scope guarantees: no
    // local override in effect.
    let effective = Config::load_effective_with_provenance(Some(&dir)).unwrap();
    assert!(effective.local_path.is_none());
    assert_ne!(effective.api_url_source, ConfigSource::Local);
    assert_ne!(effective.default_output_source, ConfigSource::Local);
    assert_ne!(effective.no_color_source, ConfigSource::Local);
}

#[test]
fn test_load_effective_prefers_local_api_url_over_global() {
    let dir = temp_dir("local-api-url-precedence");

    let project_config = ProjectConfig {
        project: None,
        mcp: None,
        mcp_extra: None,
        plugins: None,
        config: Some(LocalConfigOverrides {
            api_url: Some("https://staging-release--nexus.netlify.app".to_string()),
            default_output: None,
            no_color: None,
        }),
    };
    save_project_config(Some(&dir), &project_config).unwrap();

    let effective = Config::load_effective_with_provenance(Some(&dir)).unwrap();

    assert_eq!(
        effective.config.api_url,
        "https://staging-release--nexus.netlify.app"
    );
    assert_eq!(effective.api_url_source, ConfigSource::Local);
    assert!(effective.local_path.is_some());

    // Keys not set locally must not be reported as local.
    assert_ne!(effective.default_output_source, ConfigSource::Local);
    assert_ne!(effective.no_color_source, ConfigSource::Local);
}

#[test]
fn test_load_effective_partial_override_only_affects_set_keys() {
    let dir = temp_dir("partial-override");

    let project_config = ProjectConfig {
        project: None,
        mcp: None,
        mcp_extra: None,
        plugins: None,
        config: Some(LocalConfigOverrides {
            api_url: None,
            default_output: Some(OutputPreference::Json),
            no_color: None,
        }),
    };
    save_project_config(Some(&dir), &project_config).unwrap();

    let effective = Config::load_effective_with_provenance(Some(&dir)).unwrap();

    assert_eq!(effective.config.default_output, OutputPreference::Json);
    assert_eq!(effective.default_output_source, ConfigSource::Local);
    // api_url was not overridden locally.
    assert_ne!(effective.api_url_source, ConfigSource::Local);
}

#[test]
fn test_load_effective_no_local_file_uses_only_global_layer() {
    let dir = temp_dir("no-local-file");
    // No .nexus/config.toml written at all in this directory.
    let effective = Config::load_effective_with_provenance(Some(&dir)).unwrap();
    assert!(effective.local_path.is_none());
    assert_eq!(
        effective.config.api_url,
        Config::load().unwrap().api_url,
        "without a local override, the effective api_url must equal the global one"
    );
}

#[test]
fn test_config_source_display() {
    assert_eq!(ConfigSource::Local.to_string(), "local");
    assert_eq!(ConfigSource::Global.to_string(), "global");
    assert_eq!(ConfigSource::Default.to_string(), "default");
}
