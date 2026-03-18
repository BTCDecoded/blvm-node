//! Canonical list of core RPC method names.
//!
//! Single source of truth for (1) getrpcinfo active_commands and (2) module
//! registration conflict check. Adding or renaming a method is done here only.

/// All core RPC methods that cannot be overridden by modules.
/// Used by getrpcinfo (active_commands) and by register_module_endpoint (conflict check).
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
    #[cfg(feature = "miniscript")]
    "getdescriptorinfo",
    #[cfg(feature = "miniscript")]
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
            assert!(seen.insert(*name), "duplicate core RPC method: {}", name);
        }
    }
}
