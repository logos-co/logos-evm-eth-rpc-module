//! eth_rpc_module — a proxyable, fail-closed Ethereum JSON-RPC client.
//!
//! Stores per-chain configuration (endpoint + proxy policy) keyed by chainId, so
//! callers route by chainId alone. Every outbound request is built through the
//! single [`proxy`] chokepoint. The crypto-free RPC core (`rpc`) and the proxy
//! chokepoint (`proxy`) are plain Rust, unit-tested with
//! `cargo test --no-default-features`; the Logos glue is behind the default
//! `logos_module` feature.

mod proxy;
mod rpc;

pub use rpc::{ChainConfig, EthRpc, RpcError};

#[cfg(feature = "logos_module")]
mod glue;
