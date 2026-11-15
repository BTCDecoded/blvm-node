//! Named deployment profiles.
//!
//! `local` is an overlay: the embedded document is the base and the user's
//! file replaces its keys. `minimal-hardened` is a lock: the file itself must
//! satisfy the checker. The other names are stored for blvm-ui and do not
//! change settings. A file that omits `profile` keeps the historical defaults.

use super::NodeConfig;
use anyhow::Result;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

/// The audit profile. A file that names it is checked and not overlaid.
pub const PROFILE_MINIMAL_HARDENED: &str = "minimal-hardened";

const LOCAL_PROFILE_TOML: &str = include_str!("../../config/local.toml");

/// Whether selecting a profile changes the running settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileApplication {
    /// The embedded document is the base. The user's file replaces its keys.
    Overlay,
    /// The file must contain the strict values. It is not filled in.
    Lock,
    /// The name is stored. The file's own settings are what runs.
    NamedOnly,
}

/// One entry in the catalog blvm-ui can list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeploymentProfile {
    pub id: &'static str,
    pub application: ProfileApplication,
    /// The binary must not plant an assume-valid height for this profile.
    pub skips_assume_valid_seed: bool,
    pub summary: &'static str,
}

/// Profiles a config file may name. Order is the catalog order.
pub fn deployment_profiles() -> &'static [DeploymentProfile] {
    &[
        DeploymentProfile {
            id: "local",
            application: ProfileApplication::Overlay,
            skips_assume_valid_seed: true,
            summary: "Loopback listen, modules off, native fees, mempool saved. Network and ports come from the CLI.",
        },
        DeploymentProfile {
            id: "minimal-hardened",
            application: ProfileApplication::Lock,
            skips_assume_valid_seed: true,
            summary: "No inbound, no modules, no assume-valid, native fees. Unknown keys are errors.",
        },
        DeploymentProfile {
            id: "public-peer",
            application: ProfileApplication::NamedOnly,
            skips_assume_valid_seed: false,
            summary: "Not applied yet. Public listen of the address in the file, with RPC on loopback.",
        },
        DeploymentProfile {
            id: "desktop",
            application: ProfileApplication::NamedOnly,
            skips_assume_valid_seed: false,
            summary: "Not applied yet. Local node plus the index and loopback ZeroMQ.",
        },
        DeploymentProfile {
            id: "pool",
            application: ProfileApplication::NamedOnly,
            skips_assume_valid_seed: false,
            summary: "Not applied yet. Local RPC and Stratum, without the index or a fee website.",
        },
    ]
}

pub fn require_known_profile(name: &str) -> Result<DeploymentProfile> {
    deployment_profiles()
        .iter()
        .copied()
        .find(|profile| profile.id == name)
        .ok_or_else(|| {
            let known = deployment_profiles()
                .iter()
                .map(|profile| profile.id)
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!("unknown config profile {name:?}; known profiles: {known}")
        })
}

pub fn skips_assume_valid_seed(name: &str) -> bool {
    require_known_profile(name)
        .ok()
        .is_some_and(|profile| profile.skips_assume_valid_seed)
}

pub(crate) fn profile_is_applied(application: ProfileApplication) -> bool {
    matches!(
        application,
        ProfileApplication::Overlay | ProfileApplication::Lock
    )
}

impl NodeConfig {
    /// The embedded `local` profile.
    pub fn local() -> Result<Self> {
        Self::from_toml_str(LOCAL_PROFILE_TOML)
    }
}

/// When `profile = "local"`, merge the embedded document under the user's file.
/// Any other document is returned unchanged.
pub(crate) fn with_overlay_document(content: &str, is_toml: bool) -> Result<String> {
    let Some(name) = raw_profile_name(content, is_toml)? else {
        return Ok(content.to_string());
    };
    let profile = require_known_profile(&name)?;
    if profile.application != ProfileApplication::Overlay {
        return Ok(content.to_string());
    }
    if profile.id != "local" {
        anyhow::bail!(
            "profile {} is marked overlay and has no document",
            profile.id
        );
    }
    let base: toml::Value = toml::from_str(LOCAL_PROFILE_TOML)
        .map_err(|err| anyhow::anyhow!("embedded local profile: {err}"))?;
    if is_toml {
        let overlay: toml::Value = toml::from_str(content)
            .map_err(|err| anyhow::anyhow!("Failed to parse TOML config: {err}"))?;
        let merged = merge_toml(base, overlay);
        toml::to_string(&merged).map_err(|err| anyhow::anyhow!("profile overlay: {err}"))
    } else {
        let overlay: serde_json::Value = serde_json::from_str(content)
            .map_err(|err| anyhow::anyhow!("Failed to parse JSON config: {err}"))?;
        let merged = merge_json(toml_to_json(base), overlay);
        serde_json::to_string(&merged).map_err(|err| anyhow::anyhow!("profile overlay: {err}"))
    }
}

fn raw_profile_name(raw: &str, is_toml: bool) -> Result<Option<String>> {
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

fn merge_toml(base: toml::Value, overlay: toml::Value) -> toml::Value {
    match (base, overlay) {
        (toml::Value::Table(mut base_table), toml::Value::Table(overlay_table)) => {
            for (key, value) in overlay_table {
                let merged = match base_table.remove(&key) {
                    Some(existing) => merge_toml(existing, value),
                    None => value,
                };
                base_table.insert(key, merged);
            }
            toml::Value::Table(base_table)
        }
        (_base, overlay) => overlay,
    }
}

fn merge_json(base: serde_json::Value, overlay: serde_json::Value) -> serde_json::Value {
    match (base, overlay) {
        (serde_json::Value::Object(mut base_object), serde_json::Value::Object(overlay_object)) => {
            for (key, value) in overlay_object {
                let merged = match base_object.remove(&key) {
                    Some(existing) => merge_json(existing, value),
                    None => value,
                };
                base_object.insert(key, merged);
            }
            serde_json::Value::Object(base_object)
        }
        (_base, overlay) => overlay,
    }
}

fn toml_to_json(value: toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(text) => serde_json::Value::String(text),
        toml::Value::Integer(number) => serde_json::Value::Number(number.into()),
        toml::Value::Float(number) => serde_json::Number::from_f64(number)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::Boolean(bit) => serde_json::Value::Bool(bit),
        toml::Value::Datetime(stamp) => serde_json::Value::String(stamp.to_string()),
        toml::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(toml_to_json).collect())
        }
        toml::Value::Table(table) => {
            let mut object = serde_json::Map::new();
            for (key, item) in table {
                object.insert(key, toml_to_json(item));
            }
            serde_json::Value::Object(object)
        }
    }
}

/// A loopback or unspecified listen address is not sent to peers.
pub fn listen_address_can_be_advertised(addr: SocketAddr) -> bool {
    !ip_is_loopback_or_unspecified(addr.ip())
}

fn ip_is_loopback_or_unspecified(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_unspecified(),
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return v4.is_loopback() || v4.is_unspecified();
            }
            ip.is_loopback() || ip.is_unspecified()
        }
    }
}

/// Lines printed once when the node process starts.
pub fn operator_start_lines(config: &NodeConfig, data_dir: &str) -> Vec<String> {
    let mut lines = Vec::with_capacity(8);
    lines.push(profile_line(config));
    lines.push(format!(
        "network={}",
        config.protocol_version.as_deref().unwrap_or("unset")
    ));
    lines.push(format!("data={}", describe_data_dir(data_dir)));
    lines.push(listen_line(config));
    lines.push(format!(
        "rpc={}",
        config
            .rpc_addr
            .map(|addr| addr.to_string())
            .unwrap_or_else(|| "unset".to_string())
    ));
    lines.push(module_line(config));
    lines.push(assume_valid_line(config));
    lines.push(fee_line(config));
    lines.push(mempool_line(config));
    lines.push(peer_line(config));
    lines
}

fn profile_line(config: &NodeConfig) -> String {
    match config.profile.as_deref() {
        None => "profile=none (open defaults)".to_string(),
        Some(id) => match require_known_profile(id) {
            Ok(profile) if profile_is_applied(profile.application) => {
                format!("profile={id} (applied)")
            }
            Ok(_) => format!("profile={id} (named only; this file's settings are what runs)"),
            Err(_) => format!("profile={id}"),
        },
    }
}

pub fn describe_data_dir(data_dir: &str) -> String {
    let path = Path::new(data_dir);
    if path.is_absolute() {
        return data_dir.to_string();
    }
    match std::env::current_dir() {
        Ok(cwd) => format!("{data_dir} (absolute {})", cwd.join(path).display()),
        Err(_) => format!("{data_dir} (relative to the working directory)"),
    }
}

fn listen_line(config: &NodeConfig) -> String {
    let listen = if config.listen { "on" } else { "off" };
    let advertise = if config.enable_self_advertisement {
        "on"
    } else {
        "off"
    };
    let addr = config
        .listen_addr
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| "unset".to_string());
    let withheld = config.listen_addr.is_some_and(|addr| {
        config.enable_self_advertisement && !listen_address_can_be_advertised(addr)
    });
    let note = if withheld {
        "; loopback is not advertised to peers"
    } else {
        ""
    };
    format!("p2p=listen {listen} {addr}; self-advertisement {advertise}{note}")
}

fn module_line(config: &NodeConfig) -> String {
    let Some(modules) = config.modules.as_ref().filter(|modules| modules.enabled) else {
        return "modules=off".to_string();
    };
    let mut names: Vec<&str> = modules.enabled_modules.keys().map(String::as_str).collect();
    names.sort_unstable();
    let registry = modules.registry_url.as_deref().unwrap_or("none");
    let zmq = modules
        .module_configs
        .get("blvm-zmq")
        .filter(|_| names.contains(&"blvm-zmq"))
        .map(|entries| {
            let mut binds: Vec<String> = entries
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect();
            binds.sort();
            format!("; zmq {}", binds.join(", "))
        })
        .unwrap_or_default();
    format!("modules=on [{}] registry={registry}{zmq}", names.join(", "))
}

fn assume_valid_line(config: &NodeConfig) -> String {
    let Some(validation) = config.block_validation.as_ref() else {
        return "assume-valid=off".to_string();
    };
    if validation.assume_valid_height == 0 && validation.assume_valid_hash.is_none() {
        return "assume-valid=off".to_string();
    }
    match validation.assume_valid_hash {
        Some(hash) => format!(
            "assume-valid=height {} hash {}",
            validation.assume_valid_height,
            hash_hex(&hash)
        ),
        None => format!(
            "assume-valid=height {} (script checks still run until a hash is set)",
            validation.assume_valid_height
        ),
    }
}

fn fee_line(config: &NodeConfig) -> String {
    let fees = config
        .fees
        .clone()
        .unwrap_or_else(super::FeeEstimatorConfig::default);
    if fees.estimator == "auto" && fees.allow_auto_override.is_empty() {
        "fees=built-in (estimator auto has no module allowed to replace it)".to_string()
    } else {
        format!(
            "fees=estimator {} fallback {}",
            fees.estimator, fees.fallback
        )
    }
}

fn mempool_line(config: &NodeConfig) -> String {
    let persist = config
        .mempool
        .as_ref()
        .is_some_and(|mempool| mempool.persist_mempool);
    if persist {
        "mempool=saved across restarts".to_string()
    } else {
        "mempool=dropped on shutdown".to_string()
    }
}

fn peer_line(config: &NodeConfig) -> String {
    let cap = config
        .max_outbound_peers
        .map(|count| count.to_string())
        .unwrap_or_else(|| "unset".to_string());
    let target = config
        .network_timing
        .as_ref()
        .map(|timing| timing.target_outbound_peers.to_string())
        .unwrap_or_else(|| "8 once network_timing exists".to_string());
    format!("peers=outbound cap {cap}; desired {target}")
}

fn hash_hex(hash: &[u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn known_profile_or_absent(name: Option<&str>) -> Result<Option<DeploymentProfile>> {
    match name {
        None => Ok(None),
        Some(name) => require_known_profile(name).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_profile_name_is_rejected() {
        let err = require_known_profile("laptop").unwrap_err().to_string();
        assert!(err.contains("laptop"), "{err}");
        assert!(err.contains("local"), "{err}");
        assert!(err.contains("minimal-hardened"), "{err}");
    }

    #[test]
    fn a_named_profile_does_not_change_settings() {
        let config = NodeConfig::from_toml_str("profile = \"desktop\"\n").expect("desktop");
        assert_eq!(config.profile.as_deref(), Some("desktop"));
        assert!(config.listen);
        assert!(config.modules.as_ref().unwrap().enabled);
        let lines = operator_start_lines(&config, "./data");
        assert!(
            lines.iter().any(|line| line.contains("named only")),
            "{lines:?}"
        );
    }

    #[test]
    fn local_turns_modules_off_and_saves_the_mempool() {
        let config = NodeConfig::from_toml_str("profile = \"local\"\n").expect("local");
        assert!(config.listen);
        assert!(config.listen_addr.is_none());
        assert!(config.rpc_addr.is_none());
        assert!(config.protocol_version.is_none());
        assert!(!config.enable_self_advertisement);
        let modules = config.modules.as_ref().unwrap();
        assert!(!modules.enabled);
        assert!(modules.enabled_modules.is_empty());
        assert!(modules.registry_url.is_none());
        assert!(!modules.watch_enabled);
        let fees = config.fees.as_ref().unwrap();
        assert_eq!(fees.estimator, "native");
        assert_eq!(fees.fallback, "native");
        assert!(fees.allow_auto_override.is_empty());
        let mempool = config.mempool.as_ref().unwrap();
        assert!(mempool.persist_mempool);
        assert_eq!(mempool.mempool_persistence_path, "mempool.dat");
        assert!(skips_assume_valid_seed("local"));
        let lines = operator_start_lines(&config, "./data");
        assert!(
            lines
                .iter()
                .any(|line| line.contains("profile=local (applied)")),
            "{lines:?}"
        );
    }

    #[test]
    fn an_explicit_module_pin_survives_the_local_base() {
        let config = NodeConfig::from_toml_str(
            "profile = \"local\"\n[modules]\nenabled = true\nblvm-index = \"0.1.*\"\n",
        )
        .expect("pin");
        let modules = config.modules.unwrap();
        assert!(modules.enabled);
        assert_eq!(
            modules
                .enabled_modules
                .get("blvm-index")
                .map(String::as_str),
            Some("0.1.*")
        );
        assert!(!modules.enabled_modules.contains_key("blvm-miniscript"));
        assert!(!modules.enabled_modules.contains_key("blvm-zmq"));
        assert!(modules.registry_url.is_none());
    }

    #[test]
    fn local_rejects_an_unknown_top_level_key() {
        let err = NodeConfig::from_toml_str("profile = \"local\"\nnot_a_real_key = true\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not_a_real_key"), "{err}");
    }

    #[test]
    fn a_one_line_hardened_profile_fails_closed() {
        let err = NodeConfig::from_toml_str("profile = \"minimal-hardened\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("minimal-hardened"), "{err}");
    }

    #[test]
    fn a_file_without_a_profile_keeps_the_module_defaults() {
        let config = NodeConfig::from_toml_str("listen = true\n").expect("historical");
        assert!(config.profile.is_none());
        assert!(
            config
                .modules
                .unwrap()
                .enabled_modules
                .contains_key("blvm-zmq")
        );
        assert!(!skips_assume_valid_seed("desktop"));
        assert!(skips_assume_valid_seed("minimal-hardened"));
    }

    #[test]
    fn json_local_uses_the_same_base() {
        let config = NodeConfig::from_json_str(r#"{"profile":"local"}"#).expect("json");
        assert!(!config.modules.unwrap().enabled);
        assert_eq!(
            config.mempool.unwrap().mempool_persistence_path,
            "mempool.dat"
        );
    }

    #[test]
    fn open_defaults_name_the_fee_estimator_and_the_mempool() {
        let config = NodeConfig::default();
        let lines = operator_start_lines(&config, "./data");
        let text = lines.join("\n");
        assert!(text.contains("profile=none"), "{text}");
        assert!(text.contains("fees=built-in"), "{text}");
        assert!(text.contains("mempool=dropped on shutdown"), "{text}");
        assert!(text.contains("modules=on"), "{text}");
        assert!(text.contains("blvm-zmq"), "{text}");
        assert!(text.contains("peers=outbound cap 100"), "{text}");
        assert!(
            text.contains("desired 8 once network_timing exists"),
            "{text}"
        );
        assert!(text.contains("absolute "), "{text}");
    }

    #[test]
    fn loopback_and_unspecified_addresses_are_not_advertised() {
        let loopback: SocketAddr = "127.0.0.1:8333".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:8333".parse().unwrap();
        let unspecified: SocketAddr = "0.0.0.0:8333".parse().unwrap();
        let public: SocketAddr = "203.0.113.5:8333".parse().unwrap();
        assert!(!listen_address_can_be_advertised(loopback));
        assert!(!listen_address_can_be_advertised(mapped));
        assert!(!listen_address_can_be_advertised(unspecified));
        assert!(listen_address_can_be_advertised(public));
    }

    #[test]
    fn applied_profile_is_the_hardened_one() {
        let profile = require_known_profile(PROFILE_MINIMAL_HARDENED).unwrap();
        assert_eq!(profile.application, ProfileApplication::Lock);
        assert!(profile.skips_assume_valid_seed);
        let named = deployment_profiles()
            .iter()
            .filter(|profile| profile.application == ProfileApplication::NamedOnly)
            .count();
        assert_eq!(named, 3);
        assert_eq!(
            require_known_profile("local").unwrap().application,
            ProfileApplication::Overlay
        );
    }

    #[test]
    fn empty_profile_name_is_rejected() {
        let err = require_known_profile("").unwrap_err().to_string();
        assert!(err.contains("unknown config profile"), "{err}");
    }

    #[test]
    fn a_file_with_an_unknown_profile_does_not_load() {
        let err = NodeConfig::from_toml_str("profile = \"laptop\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("laptop"), "{err}");
    }
}
