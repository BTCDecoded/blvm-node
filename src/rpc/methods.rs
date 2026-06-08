//! Canonical list of core RPC method names.
//!
//! Single source of truth for (1) getrpcinfo active_commands and (2) module
//! registration conflict check. Adding or renaming a method is done here only.

/// RPC methods that a trusted loaded module may override via `register_core_rpc_override`.
///
/// Rules:
/// - Any method listed here may be delegated to a module at runtime.
/// - Methods that affect money, consensus, or node control must NEVER appear here.
/// - The node validates that a module's `rpc_overrides` manifest field is a subset of this list.
pub const OVERRIDABLE_CORE_RPC_METHODS: &[&str] = &["getdescriptorinfo", "analyzepsbt"];

/// All core RPC methods that cannot be overridden by modules (except those in
/// `OVERRIDABLE_CORE_RPC_METHODS`, which have a separate override path).
/// Used by getrpcinfo (active_commands) and by `register_module_endpoint` (conflict check).
pub const CORE_RPC_METHODS: &[&str] = &[
    // Blockchain
    "getblockchaininfo",
    "getblock",
    "getblockhash",
    "getblockheader",
    "getbestblockhash",
    "getblockcount",
    "getdifficulty",
    "gettxoutsetinfo",
    "loadtxoutset",
    "verifychain",
    "getchaintips",
    "getchaintxstats",
    "getblockstats",
    "pruneblockchain",
    "getpruneinfo",
    "invalidateblock",
    "reconsiderblock",
    "waitfornewblock",
    "waitforblock",
    "waitforblockheight",
    // Raw tx / mempool
    "getrawtransaction",
    "sendrawtransaction",
    "testmempoolaccept",
    "decoderawtransaction",
    "createrawtransaction",
    "gettxout",
    "gettxoutproof",
    "verifytxoutproof",
    "getmempoolinfo",
    "getrawmempool",
    "savemempool",
    "getmempoolancestors",
    "getmempooldescendants",
    "getmempoolentry",
    // Network
    "getnetworkinfo",
    "getpeerinfo",
    "getconnectioncount",
    "ping",
    "addnode",
    "disconnectnode",
    "getnettotals",
    "clearbanned",
    "setban",
    "listbanned",
    "getaddednodeinfo",
    "getnodeaddresses",
    "setnetworkactive",
    // Mining
    "getmininginfo",
    "getblocktemplate",
    "generatetoaddress",
    "submitblock",
    "estimatesmartfee",
    "prioritisetransaction",
    // Index / filter
    "getblockfilter",
    "getindexinfo",
    "getblockchainstate",
    // Address / tx details
    "validateaddress",
    "getaddressinfo",
    "gettransactiondetails",
    // Control / node
    "stop",
    "uptime",
    "getmemoryinfo",
    "getrpcinfo",
    "help",
    "logging",
    "gethealth",
    "getmetrics",
    // Modules
    "loadmodule",
    "unloadmodule",
    "reloadmodule",
    "listmodules",
    "getmoduleclispecs",
    "runmodulecli",
    "meshsendpacket",
    "meshpollreceived",
    "meshquoteroute",
    "meshrequesthopinvoice",
    "getdescriptorinfo",
    "analyzepsbt",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures no duplicate method names in the canonical list (catches copy-paste drift).
    #[test]
    fn core_rpc_methods_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for name in CORE_RPC_METHODS {
            assert!(seen.insert(*name), "duplicate core RPC method: {name}");
        }
    }

    /// OVERRIDABLE list must be a subset of CORE_RPC_METHODS (method must be advertised to be overridable).
    #[test]
    fn overridable_methods_are_subset_of_core() {
        let core: std::collections::HashSet<_> = CORE_RPC_METHODS.iter().copied().collect();
        for method in OVERRIDABLE_CORE_RPC_METHODS {
            assert!(
                core.contains(*method),
                "overridable method '{method}' is not in CORE_RPC_METHODS",
            );
        }
    }

    /// Safety: OVERRIDABLE list must never contain money/consensus/control methods.
    #[test]
    fn overridable_methods_are_not_dangerous() {
        const FORBIDDEN: &[&str] = &[
            "sendrawtransaction",
            "stop",
            "invalidateblock",
            "reconsiderblock",
            "loadmodule",
            "unloadmodule",
            "reloadmodule",
            "submitblock",
            "setban",
            "clearbanned",
        ];
        let forbidden: std::collections::HashSet<_> = FORBIDDEN.iter().copied().collect();
        for method in OVERRIDABLE_CORE_RPC_METHODS {
            assert!(
                !forbidden.contains(*method),
                "dangerous method '{method}' must not be in OVERRIDABLE_CORE_RPC_METHODS",
            );
        }
    }
}
