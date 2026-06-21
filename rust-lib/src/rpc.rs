//! Proxyable Ethereum JSON-RPC client core.
//!
//! Holds per-chain configuration (endpoint + proxy policy), persisted to disk,
//! and exposes chainId-keyed RPC calls. Every outbound request is built through
//! the fail-closed [`crate::proxy`] chokepoint, so a chain configured with
//! `proxy_required` and no usable proxy refuses to call rather than leaking in
//! the clear. Pure (no Logos deps) and unit-testable with `cargo test`.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::proxy::{build_client, ProxyConfig};

fn default_timeout() -> u64 {
    30
}

/// Per-chain configuration. `endpoint` is the JSON-RPC URL; `proxy` /
/// `proxy_required` drive the fail-closed client construction. JSON is
/// camelCase (`proxyRequired`, `timeoutSecs`) to match the wallet backend.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainConfig {
    pub endpoint: String,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub proxy_required: bool,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

#[derive(Debug)]
pub enum RpcError {
    UnknownChain(u64),
    Proxy(String),
    Http(String),
    Rpc { code: i64, message: String },
    Parse(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::UnknownChain(id) => write!(f, "no configuration for chain {id}"),
            RpcError::Proxy(e) => write!(f, "proxy: {e}"),
            RpcError::Http(e) => write!(f, "http: {e}"),
            RpcError::Rpc { code, message } => write!(f, "rpc error {code}: {message}"),
            RpcError::Parse(e) => write!(f, "parse: {e}"),
        }
    }
}

type Result<T> = std::result::Result<T, RpcError>;

/// The RPC client: a persisted map of chainId → [`ChainConfig`].
pub struct EthRpc {
    chains: HashMap<u64, ChainConfig>,
    store_path: Option<PathBuf>,
}

impl EthRpc {
    pub fn new() -> Self {
        Self { chains: HashMap::new(), store_path: None }
    }

    /// Open a store backed by `path` (a JSON file), loading any existing config.
    pub fn with_store(path: PathBuf) -> Self {
        let mut s = Self::new();
        s.store_path = Some(path);
        s.load();
        s
    }

    fn load(&mut self) {
        if let Some(p) = &self.store_path {
            if let Ok(txt) = std::fs::read_to_string(p) {
                if let Ok(m) = serde_json::from_str::<HashMap<String, ChainConfig>>(&txt) {
                    self.chains =
                        m.into_iter().filter_map(|(k, v)| k.parse::<u64>().ok().map(|id| (id, v))).collect();
                }
            }
        }
    }

    fn persist(&self) {
        if let Some(p) = &self.store_path {
            let m: HashMap<String, ChainConfig> =
                self.chains.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
            if let Ok(txt) = serde_json::to_string_pretty(&m) {
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(p, txt);
            }
        }
    }

    pub fn set_chain_config(&mut self, chain_id: u64, cfg: ChainConfig) {
        self.chains.insert(chain_id, cfg);
        self.persist();
    }

    pub fn get_chain_config(&self, chain_id: u64) -> Option<&ChainConfig> {
        self.chains.get(&chain_id)
    }

    pub fn remove_chain_config(&mut self, chain_id: u64) -> bool {
        let removed = self.chains.remove(&chain_id).is_some();
        self.persist();
        removed
    }

    pub fn list_chains(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.chains.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// Build a fail-closed client for `chain_id` and return it with the endpoint.
    fn client_for(&self, chain_id: u64) -> Result<(reqwest::blocking::Client, String)> {
        let c = self.chains.get(&chain_id).ok_or(RpcError::UnknownChain(chain_id))?;
        let pc = ProxyConfig::new(c.proxy.clone(), c.proxy_required, c.timeout_secs);
        let client = build_client(&pc).map_err(|e| RpcError::Proxy(e.to_string()))?;
        Ok((client, c.endpoint.clone()))
    }

    /// Issue a raw JSON-RPC call and return the `result` value (or an error).
    pub fn rpc_call(&self, chain_id: u64, method: &str, params: Value) -> Result<Value> {
        let (client, endpoint) = self.client_for(chain_id)?;
        Self::post_rpc(&client, &endpoint, method, params)
    }

    /// Like [`Self::rpc_call`] but POSTs to an explicit `url` instead of the
    /// chain's configured endpoint, while still using `chain_id`'s fail-closed
    /// proxied client. For off-chain JSON-RPC services tied to a chain — e.g. an
    /// ERC-4337 bundler (`eth_sendUserOperation`) — so they too go through
    /// net-proxy (a private send must not leak the user's IP to the bundler).
    pub fn rpc_call_url(&self, chain_id: u64, url: &str, method: &str, params: Value) -> Result<Value> {
        // Build the client from the chain's proxy config; ignore its endpoint.
        let (client, _endpoint) = self.client_for(chain_id)?;
        Self::post_rpc(&client, url, method, params)
    }

    /// POST a JSON-RPC request to `url` with `client` and unwrap the `result`.
    fn post_rpc(
        client: &reqwest::blocking::Client,
        url: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp = client.post(url).json(&body).send().map_err(|e| RpcError::Http(e.to_string()))?;
        let v: Value = resp.json().map_err(|e| RpcError::Http(e.to_string()))?;
        if let Some(err) = v.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = err.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            return Err(RpcError::Rpc { code, message });
        }
        v.get("result").cloned().ok_or_else(|| RpcError::Parse("response had no `result`".into()))
    }

    fn result_str(&self, chain_id: u64, method: &str, params: Value) -> Result<String> {
        let v = self.rpc_call(chain_id, method, params)?;
        match v {
            Value::String(s) => Ok(s),
            other => Ok(other.to_string()),
        }
    }

    // ── Typed helpers (all keyed by chain_id) ─────────────────────────────────

    /// `eth_chainId` round-trip; returns the node's chain id as a decimal.
    pub fn verify_chain_id(&self, chain_id: u64) -> Result<u64> {
        let s = self.result_str(chain_id, "eth_chainId", json!([]))?;
        parse_hex_u64(&s).ok_or_else(|| RpcError::Parse(format!("bad chainId: {s}")))
    }

    pub fn block_number(&self, chain_id: u64) -> Result<String> {
        self.result_str(chain_id, "eth_blockNumber", json!([]))
    }

    pub fn get_balance(&self, chain_id: u64, address: &str) -> Result<String> {
        self.result_str(chain_id, "eth_getBalance", json!([address, "latest"]))
    }

    /// `eth_call`; `call` is a `{to, data, ...}` object (used for ERC20 reads).
    pub fn call(&self, chain_id: u64, call: Value) -> Result<String> {
        self.result_str(chain_id, "eth_call", json!([call, "latest"]))
    }

    pub fn get_transaction_count(&self, chain_id: u64, address: &str) -> Result<String> {
        self.result_str(chain_id, "eth_getTransactionCount", json!([address, "pending"]))
    }

    pub fn gas_price(&self, chain_id: u64) -> Result<String> {
        self.result_str(chain_id, "eth_gasPrice", json!([]))
    }

    pub fn fee_history(&self, chain_id: u64, blocks: u64, reward_percentiles: Value) -> Result<Value> {
        let block_hex = format!("0x{blocks:x}");
        self.rpc_call(chain_id, "eth_feeHistory", json!([block_hex, "latest", reward_percentiles]))
    }

    pub fn estimate_gas(&self, chain_id: u64, tx: Value) -> Result<String> {
        self.result_str(chain_id, "eth_estimateGas", json!([tx]))
    }

    pub fn send_raw_transaction(&self, chain_id: u64, raw_hex: &str) -> Result<String> {
        self.result_str(chain_id, "eth_sendRawTransaction", json!([raw_hex]))
    }

    pub fn get_transaction_receipt(&self, chain_id: u64, hash: &str) -> Result<Value> {
        self.rpc_call(chain_id, "eth_getTransactionReceipt", json!([hash]))
    }

    pub fn get_transaction_by_hash(&self, chain_id: u64, hash: &str) -> Result<Value> {
        self.rpc_call(chain_id, "eth_getTransactionByHash", json!([hash]))
    }
}

impl Default for EthRpc {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_hex_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    u64::from_str_radix(h, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn cfg(endpoint: &str) -> ChainConfig {
        ChainConfig { endpoint: endpoint.into(), proxy: None, proxy_required: false, timeout_secs: 5 }
    }

    #[test]
    fn config_store_roundtrip_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chains.json");
        {
            let mut r = EthRpc::with_store(path.clone());
            r.set_chain_config(1, cfg("https://eth.example"));
            r.set_chain_config(10, cfg("https://op.example"));
            assert_eq!(r.list_chains(), vec![1, 10]);
            assert!(r.remove_chain_config(10));
            assert_eq!(r.list_chains(), vec![1]);
        }
        // Reopen: chain 1 survives, chain 10 is gone.
        let r2 = EthRpc::with_store(path);
        assert_eq!(r2.list_chains(), vec![1]);
        assert_eq!(r2.get_chain_config(1).unwrap().endpoint, "https://eth.example");
    }

    #[test]
    fn camelcase_proxy_required_is_honored() {
        // Regression: the wallet backend sends camelCase; if this field doesn't
        // map, fail-closed silently fails OPEN.
        let c: ChainConfig = serde_json::from_str(r#"{"endpoint":"x","proxyRequired":true}"#).unwrap();
        assert!(c.proxy_required);
    }

    #[test]
    fn unknown_chain_errors() {
        let r = EthRpc::new();
        assert!(matches!(r.get_balance(999, "0x0"), Err(RpcError::UnknownChain(999))));
    }

    #[test]
    fn fail_closed_when_proxy_required_but_unset() {
        let mut r = EthRpc::new();
        r.set_chain_config(
            1,
            ChainConfig {
                endpoint: "https://eth.example".into(),
                proxy: None,
                proxy_required: true, // requires a proxy, but none configured
                timeout_secs: 5,
            },
        );
        // Must refuse via the proxy chokepoint — no request is attempted.
        match r.get_balance(1, "0x0000000000000000000000000000000000000000") {
            Err(RpcError::Proxy(_)) => {}
            other => panic!("expected fail-closed Proxy error, got {other:?}"),
        }
    }

    /// Minimal one-shot HTTP server returning a canned JSON-RPC body.
    fn mock_node(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn parses_get_balance_against_mock_node() {
        let url = mock_node(r#"{"jsonrpc":"2.0","id":1,"result":"0x1234"}"#);
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(&url));
        let bal = r.get_balance(1, "0x0000000000000000000000000000000000000000").unwrap();
        assert_eq!(bal, "0x1234");
    }

    #[test]
    fn verify_chain_id_decodes_hex() {
        let url = mock_node(r#"{"jsonrpc":"2.0","id":1,"result":"0xa"}"#);
        let mut r = EthRpc::new();
        r.set_chain_config(10, cfg(&url));
        assert_eq!(r.verify_chain_id(10).unwrap(), 10);
    }

    #[test]
    fn surfaces_rpc_error() {
        let url = mock_node(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom"}}"#);
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(&url));
        match r.gas_price(1) {
            Err(RpcError::Rpc { code, message }) => {
                assert_eq!(code, -32000);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Rpc error, got {other:?}"),
        }
    }
}
