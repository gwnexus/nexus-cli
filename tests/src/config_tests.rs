//! Tests for nexus_core::config module.

use nexus_core::config::{Config, McpSource, OutputPreference};

#[test]
fn test_config_default_values() {
    let config = Config::default();
    assert_eq!(config.api_url, "https://nexus.gatewarden.eu");
    assert_eq!(config.default_output, OutputPreference::Table);
    assert!(!config.no_color);
    assert_eq!(config.mcp_source, McpSource::Npm);
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
