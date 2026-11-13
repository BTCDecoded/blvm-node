//! The `minimal-hardened` deployment profile.
//!
//! Loading a document that sets `profile = "minimal-hardened"` rejects unknown
//! keys and refuses a document that breaks the audit boundary. Historical
//! documents that omit `profile` keep the previous defaults, including ignored
//! unknown keys.

use super::{BanShareMode, NodeConfig, TransportPreferenceConfig};
use anyhow::{Result, bail};
use std::net::SocketAddr;

pub const PROFILE_MINIMAL_HARDENED: &str = "minimal-hardened";

const PROFILE_TOML: &str = include_str!("../../config/minimal-hardened.toml");

const TOP_LEVEL_KEYS: &[&str] = &[
    "profile",
    "listen_addr",
    "listen",
    "rpc_addr",
    "block_validation",
    "assumeutxo_blockhash",
    "transport_preference",
    "max_peers",
    "protocol_version",
    "signet_challenge",
    "modules",
    "stratum_v2",
    "rpc",
    "rpc_auth",
    "ban_list_sharing",
    "governance",
    "storage",
    "persistent_peers",
    "enable_self_advertisement",
    "dos_protection",
    "ibd_protection",
    "ibd",
    "spam_ban",
    "relay",
    "serve_compact_filters",
    "address_database",
    "dandelion",
    "peer_rate_limiting",
    "network_timing",
    "request_timeouts",
    "protocol_limits",
    "background_tasks",
    "replay_protection",
    "module_resource_limits",
    "logging",
    "mempool",
    "fees",
    "rbf",
    "rest_api",
    "timers",
    "module_timers",
    "module_network",
    "network",
    "services",
];

impl NodeConfig {
    /// The shipped minimal-hardened profile.
    pub fn minimal_hardened() -> Result<Self> {
        Self::from_toml_str(PROFILE_TOML)
    }
}

pub(crate) fn apply_profile_gate(config: &NodeConfig, raw: &str, is_toml: bool) -> Result<()> {
    if document_profile(raw, is_toml)?.as_deref() != Some(PROFILE_MINIMAL_HARDENED) {
        return Ok(());
    }
    let unknown = unknown_top_level_keys(raw, is_toml)?;
    if !unknown.is_empty() {
        bail!(
            "minimal-hardened profile rejects unknown config keys: {}",
            unknown.join(", ")
        );
    }
    enforce_minimal_hardened(config)
}

fn document_profile(raw: &str, is_toml: bool) -> Result<Option<String>> {
    if is_toml {
        let value: toml::Value = toml::from_str(raw)
            .map_err(|err| anyhow::anyhow!("Failed to parse TOML config: {err}"))?;
        Ok(value
            .get("profile")
            .and_then(|item| item.as_str())
            .map(str::to_string))
    } else {
        let value: serde_json::Value = serde_json::from_str(raw)
            .map_err(|err| anyhow::anyhow!("Failed to parse JSON config: {err}"))?;
        Ok(value
            .get("profile")
            .and_then(|item| item.as_str())
            .map(str::to_string))
    }
}

fn unknown_top_level_keys(raw: &str, is_toml: bool) -> Result<Vec<String>> {
    let keys = if is_toml {
        let value: toml::Value = toml::from_str(raw)
            .map_err(|err| anyhow::anyhow!("Failed to parse TOML config: {err}"))?;
        let table = value
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("config root must be a table"))?;
        table.keys().cloned().collect::<Vec<_>>()
    } else {
        let value: serde_json::Value = serde_json::from_str(raw)
            .map_err(|err| anyhow::anyhow!("Failed to parse JSON config: {err}"))?;
        let object = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("config root must be an object"))?;
        object.keys().cloned().collect::<Vec<_>>()
    };
    Ok(keys
        .into_iter()
        .filter(|key| !TOP_LEVEL_KEYS.contains(&key.as_str()))
        .collect())
}

fn require_loopback(addr: SocketAddr, key: &str) -> Result<()> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        bail!("{key} must be a loopback address in the minimal-hardened profile, got {addr}")
    }
}

fn enforce_minimal_hardened(config: &NodeConfig) -> Result<()> {
    if config.listen {
        bail!("minimal-hardened profile does not accept inbound connections (listen = true)");
    }
    let listen_addr = config.listen_addr.ok_or_else(|| {
        anyhow::anyhow!("minimal-hardened profile requires listen_addr on loopback")
    })?;
    require_loopback(listen_addr, "listen_addr")?;
    let rpc_addr = config
        .rpc_addr
        .ok_or_else(|| anyhow::anyhow!("minimal-hardened profile requires rpc_addr on loopback"))?;
    require_loopback(rpc_addr, "rpc_addr")?;

    let rpc_auth = config.rpc_auth.as_ref().ok_or_else(|| {
        anyhow::anyhow!("minimal-hardened profile requires [rpc_auth] required = true")
    })?;
    if !rpc_auth.required {
        bail!("minimal-hardened profile requires RPC authentication");
    }

    if config.enable_self_advertisement {
        bail!("minimal-hardened profile does not advertise this node's address");
    }
    if config.serve_compact_filters {
        bail!("minimal-hardened profile does not serve compact filters");
    }
    if !matches!(
        config.transport_preference,
        TransportPreferenceConfig::TcpOnly
    ) {
        bail!("minimal-hardened profile uses Bitcoin TCP only");
    }
    if config.assumeutxo_blockhash.is_some() || config.block_validation.is_some() {
        bail!("minimal-hardened profile does not skip validation with assumeutxo or assumevalid");
    }

    let modules = config.modules.as_ref().ok_or_else(|| {
        anyhow::anyhow!("minimal-hardened profile requires [modules] enabled = false")
    })?;
    if modules.enabled
        || modules.registry_url.is_some()
        || modules.marketplace_fetch_enabled
        || modules.allow_unsigned_modules
        || modules.watch_auto_load
        || !modules.enabled_modules.is_empty()
    {
        bail!("minimal-hardened profile does not load, fetch, or watch modules");
    }
    // `module_configs` still carries the crate's default ZeroMQ bind strings.
    // `with_modules_from_config` returns before reading them when `enabled` is false.

    if config
        .services
        .iter()
        .any(|(_, service)| service.listen.is_some())
    {
        bail!("minimal-hardened profile does not bind hosted service listeners");
    }
    if !config.module_network.is_empty() {
        bail!("minimal-hardened profile does not allow module HTTP");
    }
    if config
        .network
        .as_ref()
        .and_then(|network| network.http_proxy.as_ref())
        .is_some()
    {
        bail!("minimal-hardened profile does not use an HTTP proxy");
    }

    let ban = config.ban_list_sharing.as_ref().ok_or_else(|| {
        anyhow::anyhow!("minimal-hardened profile requires ban list sharing to be disabled")
    })?;
    if ban.enabled || !matches!(ban.share_mode, BanShareMode::Disabled) {
        bail!("minimal-hardened profile does not share ban lists");
    }

    if config.rest_api.as_ref().is_some_and(|rest| rest.enabled) {
        bail!("minimal-hardened profile does not enable the REST API");
    }

    let fees = config.fees.as_ref().ok_or_else(|| {
        anyhow::anyhow!("minimal-hardened profile requires the built-in fee estimator")
    })?;
    if fees.estimator != "native"
        || fees.fallback != "native"
        || !fees.allow_auto_override.is_empty()
    {
        bail!("minimal-hardened profile uses the built-in fee estimator only");
    }

    let storage = config.storage.as_ref().ok_or_else(|| {
        anyhow::anyhow!("minimal-hardened profile requires [storage] auto_migrate_core = false")
    })?;
    if storage.auto_migrate_core {
        bail!("minimal-hardened profile does not migrate a Bitcoin Core datadir on startup");
    }

    let mempool = config.mempool.as_ref().ok_or_else(|| {
        anyhow::anyhow!("minimal-hardened profile requires standard mempool policy")
    })?;
    if !mempool.require_standard {
        bail!("minimal-hardened profile keeps standard mempool policy on");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_profile_holds_the_audit_boundary() {
        let config = NodeConfig::minimal_hardened().expect("shipped profile");
        assert!(PROFILE_TOML.contains("profile = \"minimal-hardened\""));
        assert!(!config.listen);
        assert!(config.listen_addr.unwrap().ip().is_loopback());
        assert!(config.rpc_addr.unwrap().ip().is_loopback());
        assert!(config.rpc_auth.unwrap().required);
        assert!(!config.enable_self_advertisement);
        let modules = config.modules.unwrap();
        assert!(!modules.enabled);
        assert!(modules.enabled_modules.is_empty());
        assert!(modules.registry_url.is_none());
        assert_eq!(config.network_timing.unwrap().max_addresses_from_dns, 0);
        assert!(config.persistent_peers.is_empty());
        assert_eq!(config.fees.unwrap().estimator, "native");
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/config/minimal-hardened.toml"
        ))
        .unwrap();
        assert_eq!(text, PROFILE_TOML);
        for secret in ["password", "api_key", "token =", "rpcuser", "BEGIN "] {
            assert!(
                !text.to_ascii_lowercase().contains(secret),
                "profile file contains {secret}"
            );
        }
    }

    #[test]
    fn unknown_key_fails_only_inside_the_profile() {
        let historical = "listen = false\nthis_key_is_not_real = true\n";
        assert!(
            NodeConfig::from_toml_str(historical).is_ok(),
            "historical files still ignore unknown keys"
        );
        let hardened = format!("this_key_is_not_real = true\n{PROFILE_TOML}");
        let err = NodeConfig::from_toml_str(&hardened)
            .unwrap_err()
            .to_string();
        assert!(err.contains("this_key_is_not_real"), "{err}");
    }

    #[test]
    fn public_rpc_bind_fails_the_profile() {
        let hardened = PROFILE_TOML.replace("127.0.0.1:8332", "0.0.0.0:8332");
        let err = NodeConfig::from_toml_str(&hardened)
            .unwrap_err()
            .to_string();
        assert!(err.contains("rpc_addr"), "{err}");
    }

    #[test]
    fn listed_peer_is_an_allowed_override() {
        let hardened = PROFILE_TOML.replace(
            "persistent_peers = []",
            "persistent_peers = [\"127.0.0.1:8333\"]",
        );
        let config = NodeConfig::from_toml_str(&hardened).expect("peer override");
        assert_eq!(config.persistent_peers, vec!["127.0.0.1:8333".to_string()]);
        assert_eq!(config.network_timing.unwrap().max_addresses_from_dns, 0);
    }
}
