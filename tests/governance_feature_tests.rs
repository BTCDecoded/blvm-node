//! Tests for governance-related configuration (`GovernanceConfig`) and ban-list sharing.
//!
//! `GovernanceConfig` exists under `blvm_node::config` when the `governance` feature is enabled.

#[cfg(feature = "governance")]
mod governance_on {
    use blvm_node::config::GovernanceConfig;

    #[test]
    fn governance_config_default_disables_relay() {
        let c = GovernanceConfig::default();
        assert!(!c.enabled);
        assert!(c.commons_url.is_none());
        assert!(c.api_key.is_none());
    }

    #[test]
    fn governance_config_toml_parses_optional_fields() {
        let src = r#"
enabled = true
commons_url = "http://10.0.0.2:8080"
api_key = "test-key"
"#;
        let c: GovernanceConfig = toml::from_str(src).expect("parse governance TOML");
        assert!(c.enabled);
        assert_eq!(c.commons_url.as_deref(), Some("http://10.0.0.2:8080"));
        assert_eq!(c.api_key.as_deref(), Some("test-key"));
    }

    #[test]
    fn governance_config_toml_omitted_optionals_use_defaults() {
        let c: GovernanceConfig = toml::from_str("").expect("empty table defaults");
        assert!(!c.enabled);
        assert!(c.commons_url.is_none());
        assert!(c.api_key.is_none());
    }
}

#[cfg(feature = "governance")]
#[test]
fn ban_list_sharing_defaults_match_config() {
    use blvm_node::config::{BanListSharingConfig, BanShareMode};

    let c = BanListSharingConfig::default();
    assert!(c.enabled);
    assert!(matches!(c.share_mode, BanShareMode::Periodic));
    assert_eq!(c.periodic_interval_seconds, 300);
    assert_eq!(c.min_ban_duration_to_share, 3600);
}

#[cfg(feature = "governance")]
#[test]
fn ban_list_sharing_toml_share_mode_lowercase() {
    use blvm_node::config::{BanListSharingConfig, BanShareMode};

    let c: BanListSharingConfig = toml::from_str(
        r#"
enabled = false
share_mode = "immediate"
periodic_interval_seconds = 60
min_ban_duration_to_share = 0
"#,
    )
    .expect("parse ban list TOML");
    assert!(!c.enabled);
    assert!(matches!(c.share_mode, BanShareMode::Immediate));
    assert_eq!(c.periodic_interval_seconds, 60);
    assert_eq!(c.min_ban_duration_to_share, 0);
}

#[cfg(not(feature = "governance"))]
#[test]
fn governance_feature_disabled_build_has_ban_list_config() {
    use blvm_node::config::BanListSharingConfig;

    let c = BanListSharingConfig::default();
    assert!(c.enabled);
}
