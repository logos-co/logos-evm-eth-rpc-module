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

## Working with no configuration at all

The module ships defaults, so a consumer needs no UI app installed to work. Ask
`config_status()` — `{ ok, state, source, chains }`, where `state` is `unready`
(context not ready — ask again), `unconfigured`, or `configured` — then call
`init_defaults()`, which seeds these per chain and per **field**, only where absent:

| Chain | id | Endpoint |
|---|---|---|
| Ethereum | `1` | `https://ethereum-rpc.publicnode.com` |
| Sepolia | `11155111` | `https://ethereum-sepolia-rpc.publicnode.com` |
| Hoodi | `560048` | `https://ethereum-hoodi-rpc.publicnode.com` |

`init_defaults` is idempotent — including across restarts — so it may be called
unconditionally; `applied: false` is not an error. It writes over nothing: an endpoint,
verified mode or proxy policy already stored is left alone, and a record's `source` moves
one way only, `default` → `external`, as soon as any caller writes to it.

> **All three defaults are one operator.** publicnode sees the traffic of every
> default-configured wallet. Set your own endpoint (or a SOCKS proxy) in `eth_rpc_ui` if
> that matters to you — the defaults exist so the wallet works, not because they are private.

## Build & test

```bash
cd rust-lib && cargo test --no-default-features   # rpc + proxy cores (mock node + fail-closed)
nix build .#install                                # -> result/modules/eth_rpc_module/
```

> `src/proxy.rs` is an inlined copy of the canonical `logos-evm-net-proxy` crate
> (the module builder only stages a module's `rust-lib`, so a sibling path dep
> isn't visible in the nix sandbox). Keep the two in sync.
