//! Tests for config loading.

use pay_core::config::{Config, LogFormat};

#[test]
fn default_config() {
    let cfg = Config::default();
    assert!(!cfg.auto_pay);
    assert!(cfg.keypair.is_none());
    assert!(cfg.rpc_url.is_none());
    assert!(matches!(cfg.log_format, LogFormat::Text));
}

#[test]
fn config_from_toml_string() {
    let toml = r#"
        auto_pay = true
        rpc_url = "https://api.devnet.solana.com"
        log_format = "json"
    "#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert!(cfg.auto_pay);
    assert_eq!(
        cfg.rpc_url.as_deref(),
        Some("https://api.devnet.solana.com")
    );
    assert!(matches!(cfg.log_format, LogFormat::Json));
}

#[test]
fn config_serialization_round_trip() {
    let cfg = Config {
        auto_pay: true,
        keypair: Some("~/.config/solana/id.json".into()),
        rpc_url: Some("http://localhost:8899".into()),
        log_format: LogFormat::Json,
    };
    let toml_str = toml::to_string_pretty(&cfg).unwrap();
    let parsed: Config = toml::from_str(&toml_str).unwrap();
    assert_eq!(parsed.auto_pay, cfg.auto_pay);
    assert_eq!(parsed.keypair, cfg.keypair);
    assert_eq!(parsed.rpc_url, cfg.rpc_url);
}

#[test]
fn log_format_json_from_toml() {
    let cfg: Config = toml::from_str("log_format = \"json\"").unwrap();
    assert!(matches!(cfg.log_format, LogFormat::Json));
}

#[test]
fn log_format_text_from_toml() {
    let cfg: Config = toml::from_str("log_format = \"text\"").unwrap();
    assert!(matches!(cfg.log_format, LogFormat::Text));
}

#[test]
fn log_format_default_is_text() {
    let cfg: Config = toml::from_str("").unwrap();
    assert!(matches!(cfg.log_format, LogFormat::Text));
}
