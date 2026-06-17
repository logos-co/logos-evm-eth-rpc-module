# logos-evm-eth-rpc-module

A Logos `core` module (Rust, rust-first cdylib): a **proxyable, fail-closed
Ethereum JSON-RPC client** for the Logos multi-chain EVM wallet.

It stores configuration **per chain** (endpoint + proxy policy), so callers route
by `chainId` alone. Every outbound request is built through a single fail-closed
chokepoint (`src/proxy.rs`): if a chain is configured with `proxyRequired` and no
usable proxy, the request is **refused** rather than sent in the clear. Uses
`reqwest` with `rustls-tls` + `socks` (`socks5h://` resolves DNS through the
proxy — Tor-ready).

## Contract (`EthRpcModule`)

Config: `set_chain_config(chainId, {endpoint, proxy?, proxyRequired?, timeoutSecs?})`,
`get_chain_config`, `remove_chain_config`, `list_chains`. Calls keyed by chainId:
`verify_chain_id`, `block_number`, `get_balance`, `call`, `get_transaction_count`,
`gas_price`, `fee_history`, `estimate_gas`, `send_raw_transaction`,
`get_transaction_receipt`, `get_transaction_by_hash`, `raw_rpc`.

## Build & test

```bash
cd rust-lib && cargo test --no-default-features   # rpc + proxy cores (mock node + fail-closed)
nix build .#install                                # -> result/modules/eth_rpc_module/
```

> `src/proxy.rs` is an inlined copy of the canonical `logos-evm-net-proxy` crate
> (the module builder only stages a module's `rust-lib`, so a sibling path dep
> isn't visible in the nix sandbox). Keep the two in sync.
