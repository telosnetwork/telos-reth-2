//! Telos RPC surface policy.

use alloy_eips::BlockId;
use reth_node_core::args::RpcServerArgs;
use reth_rpc_builder::{RethRpcModule, TransportRpcModules};
use std::collections::BTreeSet;

/// Complete authenticated API exposed to the trusted companion.
pub const TELOS_AUTH_RPC_ALLOWLIST: [&str; 5] = [
    "engine_exchangeCapabilities",
    "engine_forkchoiceUpdatedV1",
    "engine_newPayloadV1",
    "eth_chainId",
    "eth_getBlockByNumber",
];

/// Complete namespace surface qualified for the Telos follower.
const TELOS_RPC_NAMESPACE_ALLOWLIST: [&str; 3] = ["eth", "net", "web3"];

/// Exact public Ethereum methods qualified for this release.
///
/// Upstream additions must be reviewed and added explicitly; otherwise startup fails closed.
pub const TELOS_PUBLIC_ETH_RPC_ALLOWLIST: &[&str] = &[
    "eth_accounts",
    "eth_baseFee",
    "eth_blockNumber",
    "eth_call",
    "eth_chainId",
    "eth_estimateGas",
    "eth_gasPrice",
    "eth_getBalance",
    "eth_getBlockByHash",
    "eth_getBlockByNumber",
    "eth_getBlockReceipts",
    "eth_getBlockTransactionCountByHash",
    "eth_getBlockTransactionCountByNumber",
    "eth_getCode",
    "eth_getFilterChanges",
    "eth_getFilterLogs",
    "eth_getHeaderByHash",
    "eth_getHeaderByNumber",
    "eth_getLogs",
    "eth_getRawTransactionByBlockHashAndIndex",
    "eth_getRawTransactionByBlockNumberAndIndex",
    "eth_getRawTransactionByHash",
    "eth_getStorageAt",
    "eth_getStorageValues",
    "eth_getTransactionByBlockHashAndIndex",
    "eth_getTransactionByBlockNumberAndIndex",
    "eth_getTransactionByHash",
    "eth_getTransactionCount",
    "eth_getTransactionReceipt",
    "eth_getUncleByBlockHashAndIndex",
    "eth_getUncleByBlockNumberAndIndex",
    "eth_getUncleCountByBlockHash",
    "eth_getUncleCountByBlockNumber",
    "eth_maxPriorityFeePerGas",
    "eth_newBlockFilter",
    "eth_newFilter",
    "eth_sendRawTransaction",
    "eth_uninstallFilter",
];

/// Public Ethereum methods qualified only for `WebSocket` transports.
pub const TELOS_WS_ONLY_ETH_RPC_ALLOWLIST: &[&str] = &["eth_subscribe", "eth_unsubscribe"];

/// Exact public network methods qualified for the follower's no-peer network implementation.
pub const TELOS_PUBLIC_NET_RPC_ALLOWLIST: &[&str] = &["net_peerCount", "net_version"];

/// Exact public Web3 methods qualified for this release.
pub const TELOS_PUBLIC_WEB3_RPC_ALLOWLIST: &[&str] = &["web3_sha3"];

/// Stock methods that cannot currently produce truthful Telos results.
///
/// Telos commits the historical empty-trie placeholder in `stateRoot`. A generated EIP-1186 proof
/// would therefore not verify against the canonical header even when the returned account values
/// are correct. Fee history and transaction filling use Ethereum fee-market semantics rather than
/// the native gas-price schedule, and local transaction submission bypasses the native forwarder.
pub const TELOS_UNSUPPORTED_RPC_METHODS: [&str; 26] = [
    "eth_blobBaseFee",
    "eth_capabilities",
    "eth_coinbase",
    "eth_createAccessList",
    "eth_feeHistory",
    "eth_fillTransaction",
    "eth_getAccount",
    "eth_getAccountInfo",
    "eth_getProof",
    "eth_getTransactionBySenderAndNonce",
    "eth_getWork",
    "eth_hashrate",
    "eth_mining",
    "eth_newPendingTransactionFilter",
    "eth_pendingTransactions",
    "eth_protocolVersion",
    "eth_sendRawTransactionSync",
    "eth_sendTransaction",
    "eth_sign",
    "eth_signTransaction",
    "eth_signTypedData",
    "eth_submitHashrate",
    "eth_submitWork",
    "eth_syncing",
    "net_listening",
    "web3_clientVersion",
];

/// Validates the block selector for Telos account nonces.
///
/// Exact canonical state and the latest state have authoritative native reconciliation applied.
/// Reth's local pool does not represent nodeos pending transactions, so serving the pending tag
/// would manufacture a nonce from an incomplete mempool view.
pub fn validate_telos_transaction_count_block(block: Option<BlockId>) -> Result<(), &'static str> {
    if block == Some(BlockId::pending()) {
        return Err(
            "eth_getTransactionCount pending is unavailable: the local Reth pool does not mirror nodeos; use latest or an exact canonical block",
        )
    }
    Ok(())
}

/// Authenticated methods outside the minimal Telos companion protocol.
pub const TELOS_UNSUPPORTED_AUTH_METHODS: [&str; 17] = [
    "engine_getClientVersionV1",
    "eth_blockNumber",
    "eth_call",
    "eth_getBlockAccessList",
    "eth_getBlockAccessListByBlockHash",
    "eth_getBlockAccessListByBlockNumber",
    "eth_getBlockAccessListRaw",
    "eth_getBlockByHash",
    "eth_getBlockReceipts",
    "eth_getCode",
    "eth_getLogs",
    "eth_getProof",
    "eth_getTransactionReceipt",
    "eth_sendRawTransaction",
    "eth_syncing",
    "reth_forkchoiceUpdated",
    "reth_newPayload",
];

/// Methods that are truthful only when the native Telos forwarder is configured.
pub const TELOS_FORWARDER_REQUIRED_RPC_METHODS: [&str; 3] =
    ["eth_gasPrice", "eth_maxPriorityFeePerGas", "eth_sendRawTransaction"];

/// Replay or synthetic-block methods embedded in otherwise supported RPC namespaces.
///
/// These are removed from both regular transports and the authenticated module while the replay
/// gate is closed. `eth_call`, `eth_createAccessList`, and `eth_estimateGas` are not in this list
/// because their historical block context is attached through `ConfigureEvm`.
pub const REPLAY_UNSAFE_RPC_METHODS: [&str; 8] = [
    "eth_callBundle",
    "eth_callMany",
    "eth_getBlockAccessList",
    "eth_getBlockAccessListByBlockHash",
    "eth_getBlockAccessListByBlockNumber",
    "eth_getBlockAccessListRaw",
    "eth_simulateV1",
    "mev_simBundle",
];

/// Applies the fail-closed Telos RPC policy.
///
/// Regular Reth IPC exposes every configured namespace and cannot currently be restricted to a
/// per-namespace allowlist, so it is disabled while either execution or replay is unqualified.
/// HTTP and `WebSocket` transports remain available, but reject namespaces with known replay
/// paths.
/// The authenticated Engine API and authenticated Engine IPC are configured independently and are
/// not changed by this policy.
///
/// Returns `true` when this policy disabled regular IPC.
pub fn enforce_telos_rpc_policy(
    rpc: &mut RpcServerArgs,
    execution_ready: bool,
    replay_ready: bool,
) -> eyre::Result<bool> {
    let _ = (execution_ready, replay_ready);
    let ipc_disabled = !rpc.ipcdisable;
    // Regular IPC cannot be restricted to the exact Telos namespace/method policy.
    rpc.ipcdisable = true;

    validate_transport("HTTP", rpc.http, rpc.http_api.as_ref())?;
    validate_transport("WebSocket", rpc.ws, rpc.ws_api.as_ref())?;

    Ok(ipc_disabled)
}

/// Requires the exact authenticated companion protocol after known stock methods are removed.
/// Upstream additions fail startup instead of being silently exposed.
pub fn enforce_exact_auth_rpc_surface(module: &mut jsonrpsee::RpcModule<()>) -> eyre::Result<()> {
    let actual = module.method_names().collect::<BTreeSet<_>>();
    let expected = TELOS_AUTH_RPC_ALLOWLIST.into_iter().collect::<BTreeSet<_>>();
    if actual != expected {
        eyre::bail!(
            "Telos authenticated RPC surface mismatch: expected {expected:?}, got {actual:?}"
        );
    }
    Ok(())
}

/// Verifies every configured public transport against the exact reviewed method inventory.
///
/// This is an assertion rather than a prefix filter: a new method introduced by an upstream Reth
/// rebase stops startup until its Telos semantics have been reviewed. Forwarding methods are
/// required on every `eth` transport when a signer is configured and forbidden otherwise.
pub fn enforce_exact_public_rpc_surface(
    modules: &TransportRpcModules,
    forwarder_enabled: bool,
) -> eyre::Result<()> {
    let config = modules.module_config();
    let transports = [
        (
            "HTTP",
            modules.http_methods(|_| true),
            config.contains_http(&RethRpcModule::Eth),
            config.contains_http(&RethRpcModule::Net),
            config.contains_http(&RethRpcModule::Web3),
        ),
        (
            "WebSocket",
            modules.ws_methods(|_| true),
            config.contains_ws(&RethRpcModule::Eth),
            config.contains_ws(&RethRpcModule::Net),
            config.contains_ws(&RethRpcModule::Web3),
        ),
        (
            "IPC",
            modules.ipc_methods(|_| true),
            config.contains_ipc(&RethRpcModule::Eth),
            config.contains_ipc(&RethRpcModule::Net),
            config.contains_ipc(&RethRpcModule::Web3),
        ),
    ];

    for (transport, methods, eth, net, web3) in transports {
        let mut expected = BTreeSet::new();
        if eth {
            expected.extend(TELOS_PUBLIC_ETH_RPC_ALLOWLIST.iter().copied().filter(|method| {
                forwarder_enabled || !TELOS_FORWARDER_REQUIRED_RPC_METHODS.contains(method)
            }));
            if transport == "WebSocket" {
                expected.extend(TELOS_WS_ONLY_ETH_RPC_ALLOWLIST.iter().copied());
            }
        }
        if net {
            expected.extend(TELOS_PUBLIC_NET_RPC_ALLOWLIST.iter().copied());
        }
        if web3 {
            expected.extend(TELOS_PUBLIC_WEB3_RPC_ALLOWLIST.iter().copied());
        }
        let actual = methods
            .as_ref()
            .map(|methods| methods.method_names().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        if actual != expected {
            let unexpected = actual.difference(&expected).copied().collect::<Vec<_>>();
            let missing = expected.difference(&actual).copied().collect::<Vec<_>>();
            eyre::bail!(
                "Telos {transport} RPC surface mismatch: unexpected {unexpected:?}, missing {missing:?}"
            );
        }
    }
    Ok(())
}

/// Removes WebSocket-only methods from transports that cannot carry subscriptions.
pub fn restrict_telos_ws_only_methods(modules: &mut TransportRpcModules) {
    modules.remove_http_methods(TELOS_WS_ONLY_ETH_RPC_ALLOWLIST.iter().copied());
    modules.remove_ipc_methods(TELOS_WS_ONLY_ETH_RPC_ALLOWLIST.iter().copied());
}

fn validate_transport(
    transport: &str,
    enabled: bool,
    selection: Option<&reth_rpc_server_types::RpcModuleSelection>,
) -> eyre::Result<()> {
    if !enabled {
        return Ok(())
    }

    let Some(selection) = selection else {
        // Reth's implicit HTTP/WS selection is eth,net,web3.
        return Ok(())
    };

    let mut unsupported_namespaces = selection
        .iter_selection()
        .filter_map(|module| {
            let name = module.as_str();
            (!TELOS_RPC_NAMESPACE_ALLOWLIST.contains(&name)).then(|| name.to_string())
        })
        .collect::<Vec<_>>();
    unsupported_namespaces.sort_unstable();
    unsupported_namespaces.dedup();

    if !unsupported_namespaces.is_empty() {
        eyre::bail!(
            "Telos {transport} RPC configuration enables unsupported namespace(s): {}; the exact \
             production allowlist is eth,net,web3",
            unsupported_namespaces.join(", ")
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::RpcModule;
    use reth_rpc_builder::TransportRpcModuleConfig;
    use reth_rpc_server_types::RpcModuleSelection;

    #[test]
    fn transaction_count_rejects_only_the_unrepresented_pending_state() {
        assert!(validate_telos_transaction_count_block(None).is_ok());
        assert!(validate_telos_transaction_count_block(Some(BlockId::latest())).is_ok());
        assert!(validate_telos_transaction_count_block(Some(42u64.into())).is_ok());
        assert!(validate_telos_transaction_count_block(Some(BlockId::pending())).is_err());
    }

    #[test]
    fn closed_gate_disables_regular_ipc_but_not_auth_ipc() {
        let mut rpc = RpcServerArgs { auth_ipc: true, ..Default::default() };

        assert!(enforce_telos_rpc_policy(&mut rpc, true, false).unwrap());
        assert!(rpc.ipcdisable);
        assert!(rpc.auth_ipc);
    }

    #[test]
    fn closed_gate_keeps_standard_http_and_websocket_modules() {
        let mut rpc = RpcServerArgs {
            http: true,
            ws: true,
            http_api: Some(RpcModuleSelection::Standard),
            ws_api: Some(RpcModuleSelection::try_from_selection(["eth", "net", "web3"]).unwrap()),
            ..Default::default()
        };

        enforce_telos_rpc_policy(&mut rpc, false, false).unwrap();
        assert!(rpc.http);
        assert!(rpc.ws);
    }

    #[test]
    fn closed_gate_rejects_every_known_replay_namespace() {
        for (transport, selection) in [
            ("HTTP", RpcModuleSelection::try_from_selection(["eth", "debug"]).unwrap()),
            ("HTTP", RpcModuleSelection::try_from_selection(["ots"]).unwrap()),
            ("WebSocket", RpcModuleSelection::try_from_selection(["trace", "web3"]).unwrap()),
            ("HTTP", RpcModuleSelection::All),
        ] {
            let mut rpc = RpcServerArgs { ipcdisable: true, ..Default::default() };
            if transport == "HTTP" {
                rpc.http = true;
                rpc.http_api = Some(selection);
            } else {
                rpc.ws = true;
                rpc.ws_api = Some(selection);
            }

            let error = enforce_telos_rpc_policy(&mut rpc, true, false).unwrap_err().to_string();
            assert!(error.contains(transport));
            assert!(error.contains("unsupported namespace"));
        }
    }

    #[test]
    fn disabled_transport_does_not_expose_its_requested_modules() {
        let mut rpc = RpcServerArgs {
            http: false,
            http_api: Some(RpcModuleSelection::All),
            ..Default::default()
        };

        enforce_telos_rpc_policy(&mut rpc, true, false).unwrap();
    }

    #[test]
    fn open_gates_still_disable_unrestricted_ipc() {
        let mut rpc = RpcServerArgs {
            http: true,
            http_api: Some(RpcModuleSelection::Standard),
            ipcdisable: false,
            ..Default::default()
        };

        assert!(enforce_telos_rpc_policy(&mut rpc, true, true).unwrap());
        assert!(rpc.ipcdisable);
    }

    #[test]
    fn every_nonqualified_namespace_is_rejected_even_when_gates_are_open() {
        for selection in [
            RpcModuleSelection::try_from_selection(["eth", "admin"]).unwrap(),
            RpcModuleSelection::try_from_selection(["txpool"]).unwrap(),
            RpcModuleSelection::try_from_selection(["custom"]).unwrap(),
            RpcModuleSelection::All,
        ] {
            let mut rpc = RpcServerArgs {
                http: true,
                http_api: Some(selection),
                ipcdisable: true,
                ..Default::default()
            };
            let error = enforce_telos_rpc_policy(&mut rpc, true, true).unwrap_err().to_string();
            assert!(error.contains("unsupported namespace"));
        }
    }

    #[test]
    fn authenticated_surface_rejects_unknown_or_missing_methods() {
        let mut module = jsonrpsee::RpcModule::new(());
        for method in TELOS_AUTH_RPC_ALLOWLIST {
            module.register_method(method, |_, _, _| "ok").unwrap();
        }

        enforce_exact_auth_rpc_surface(&mut module).unwrap();
        assert_eq!(
            module.method_names().collect::<BTreeSet<_>>(),
            TELOS_AUTH_RPC_ALLOWLIST.into_iter().collect()
        );

        module.register_method("engine_futureMethodV9", |_, _, _| "unsafe").unwrap();
        assert!(enforce_exact_auth_rpc_surface(&mut module).is_err());
        module.remove_method("engine_futureMethodV9");
        module.remove_method("engine_newPayloadV1");
        assert!(enforce_exact_auth_rpc_surface(&mut module).is_err());
    }

    #[test]
    fn public_surface_is_exact_and_namespace_aware() {
        let config = TransportRpcModuleConfig::default()
            .with_http([RethRpcModule::Eth, RethRpcModule::Net])
            .with_ws([RethRpcModule::Web3]);
        let mut http = RpcModule::new(());
        for method in TELOS_PUBLIC_ETH_RPC_ALLOWLIST
            .iter()
            .copied()
            .chain(TELOS_PUBLIC_NET_RPC_ALLOWLIST.iter().copied())
        {
            http.register_method(method, |_, _, _| "ok").unwrap();
        }
        let mut ws = RpcModule::new(());
        for method in TELOS_PUBLIC_WEB3_RPC_ALLOWLIST.iter().copied() {
            ws.register_method(method, |_, _, _| "ok").unwrap();
        }
        let modules =
            TransportRpcModules::default().with_config(config).with_http(http).with_ws(ws);

        enforce_exact_public_rpc_surface(&modules, true).unwrap();
    }

    #[test]
    fn public_surface_rejects_upstream_additions_and_forwarder_leaks() {
        let config = TransportRpcModuleConfig::default().with_http([RethRpcModule::Net]);
        let mut http = RpcModule::new(());
        for method in TELOS_PUBLIC_NET_RPC_ALLOWLIST.iter().copied() {
            http.register_method(method, |_, _, _| "ok").unwrap();
        }
        http.register_method("eth_futureMethodV9", |_, _, _| "unsafe").unwrap();
        let modules = TransportRpcModules::default().with_config(config).with_http(http);
        assert!(enforce_exact_public_rpc_surface(&modules, false).is_err());

        let config = TransportRpcModuleConfig::default().with_http([RethRpcModule::Eth]);
        let mut http = RpcModule::new(());
        for method in TELOS_PUBLIC_ETH_RPC_ALLOWLIST
            .iter()
            .copied()
            .filter(|method| !TELOS_FORWARDER_REQUIRED_RPC_METHODS.contains(method))
        {
            http.register_method(method, |_, _, _| "ok").unwrap();
        }
        let safe = TransportRpcModules::default().with_config(config).with_http(http);
        enforce_exact_public_rpc_surface(&safe, false).unwrap();
    }

    #[test]
    fn subscriptions_are_exposed_only_on_websocket() {
        let config = TransportRpcModuleConfig::default()
            .with_http([RethRpcModule::Eth])
            .with_ws([RethRpcModule::Eth])
            .with_ipc([RethRpcModule::Eth]);
        let eth_module = || {
            let mut module = RpcModule::new(());
            for method in TELOS_PUBLIC_ETH_RPC_ALLOWLIST
                .iter()
                .copied()
                .chain(TELOS_WS_ONLY_ETH_RPC_ALLOWLIST.iter().copied())
            {
                module.register_method(method, |_, _, _| "ok").unwrap();
            }
            module
        };
        let mut modules = TransportRpcModules::default()
            .with_config(config)
            .with_http(eth_module())
            .with_ws(eth_module())
            .with_ipc(eth_module());

        restrict_telos_ws_only_methods(&mut modules);

        for method in TELOS_WS_ONLY_ETH_RPC_ALLOWLIST {
            assert!(modules
                .http_methods(|name| name == *method)
                .unwrap()
                .method_names()
                .next()
                .is_none());
            assert!(modules
                .ws_methods(|name| name == *method)
                .unwrap()
                .method_names()
                .next()
                .is_some());
            assert!(modules
                .ipc_methods(|name| name == *method)
                .unwrap()
                .method_names()
                .next()
                .is_none());
        }
        enforce_exact_public_rpc_surface(&modules, true).unwrap();
    }

    #[test]
    fn method_denylist_covers_eth_replay_and_synthetic_block_entrypoints() {
        assert_eq!(
            REPLAY_UNSAFE_RPC_METHODS,
            [
                "eth_callBundle",
                "eth_callMany",
                "eth_getBlockAccessList",
                "eth_getBlockAccessListByBlockHash",
                "eth_getBlockAccessListByBlockNumber",
                "eth_getBlockAccessListRaw",
                "eth_simulateV1",
                "mev_simBundle",
            ]
        );
    }
}
