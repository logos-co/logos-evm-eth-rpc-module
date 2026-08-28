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
    // Well under the 20s Logos RPC deadline. At 30 a single dead endpoint outlived the
    // protocol timeout, so the caller saw a transport failure rather than "this chain is
    // unreachable" — and behind a single-dispatch coordinator it took every other call with it.
    8
}

fn default_verified_timeout() -> u64 {
    // The verified path is a second hop (us -> proxy -> its provider) and a light client may
    // be walking headers, so it gets more room — still under the deadline.
    15
}

/// How JSON-RPC for a chain should be routed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VerifiedProxyMode {
    /// Talk to the configured endpoint directly.
    #[default]
    Off,
    /// Route through the light-client proxy and REFUSE rather than fall back. There is no
    /// `preferred` mode on purpose: quietly answering from an unverified source when the user
    /// asked for verification is the failure this feature exists to prevent.
    Required,
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
    /// Whether to route through the light-client verified proxy. Distinct from `proxy`
    /// above, which is a NETWORK proxy (SOCKS5h/Tor) answering a different question: that one
    /// hides who is asking, this one proves the answer.
    #[serde(default)]
    pub verified_proxy_mode: VerifiedProxyMode,
    #[serde(default = "default_verified_timeout")]
    pub verified_timeout_secs: u64,
}

impl ChainConfig {
    /// `proxyRequired` together with verified routing is a contradiction we refuse rather
    /// than silently resolve: the verified proxy makes its own outbound connections and knows
    /// nothing about our SOCKS config, so honouring both is impossible and honouring either
    /// silently breaks the guarantee the other was asked for.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.proxy_required && self.verified_proxy_mode == VerifiedProxyMode::Required {
            return Err("proxyRequired and verifiedProxyMode=required cannot both be set: the \
                        verified proxy makes its own connections and cannot honour a SOCKS \
                        proxy configured here"
                .into());
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum RpcError {
    UnknownChain(u64),
    Proxy(String),
    Http(String),
    Rpc { code: i64, message: String },
    Parse(String),
    /// The verified path could not answer. NEVER downgraded to a direct call: a caller who
    /// asked for verification gets an error, not an unverified number.
    VerifiedProxy(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::UnknownChain(id) => write!(f, "no configuration for chain {id}"),
            RpcError::Proxy(e) => write!(f, "proxy: {e}"),
            RpcError::Http(e) => write!(f, "http: {e}"),
            RpcError::Rpc { code, message } => write!(f, "rpc error {code}: {message}"),
            RpcError::VerifiedProxy(e) => write!(f, "verified proxy: {e}"),
            RpcError::Parse(e) => write!(f, "parse: {e}"),
        }
    }
}

type Result<T> = std::result::Result<T, RpcError>;

/// What the verified proxy can actually say about an answer.
///
/// A light client proves state against a header's stateRoot. It cannot prove a fee oracle's
/// opinion or that a broadcast was accepted — those are forwarded to its own execution
/// provider and come back on trust. Badging them "verified" would be a false claim on exactly
/// the numbers that decide what a transaction costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifiedClass {
    Verified,
    Proxied,
}

pub fn verified_class(method: &str) -> VerifiedClass {
    match method {
        "eth_getBalance" | "eth_getTransactionCount" | "eth_getCode" | "eth_getStorageAt"
        | "eth_call" => VerifiedClass::Verified,
        _ => VerifiedClass::Proxied,
    }
}

/// Rewrite params for the verified leg ONLY.
///
/// What eth_rpc sends to real nodes must not change: `eth_feeHistory`'s blockCount is a hex
/// QUANTITY string per ethereum/execution-apis, and the proxy declaring it `u64` is the
/// non-conformant side. Coercing at the source would make a shared module violate the spec to
/// suit one consumer, and be wrong against any strict node. So the translation lives here,
/// where it is the proxy's dialect being accommodated.
///
/// Every rule below was MEASURED against a live sepolia light client, not inferred.
pub fn verified_params(method: &str, params: &Value) -> Value {
    let mut p = params.as_array().cloned().unwrap_or_default();
    match method {
        // A hex blockCount returns success:true with EMPTY arrays — no error, just nothing.
        "eth_feeHistory" => {
            if let Some(Value::String(h)) = p.first().cloned() {
                if let Some(n) = h.strip_prefix("0x").and_then(|x| u64::from_str_radix(x, 16).ok()) {
                    p[0] = json!(n);
                }
            }
        }
        // "pending" is REFUSED and unverifiable by construction: a light client proves against
        // a header's stateRoot and pending has no canonical header. "latest" is the only tag
        // that works — which is what makes nonce reservation load-bearing in verified mode.
        "eth_getTransactionCount" => {
            if p.len() >= 2 && p[1] == json!("pending") {
                p[1] = json!("latest");
            }
        }
        // Upstream extends these with a third positional `optimisticStateFetch`; a one-arg
        // eth_estimateGas is refused outright with "parameters missing".
        "eth_call" | "eth_estimateGas" | "eth_createAccessList" => {
            while p.len() < 2 {
                p.push(json!("latest"));
            }
            if p.len() == 2 {
                p.push(json!(false));
            }
        }
        _ => {}
    }
    Value::Array(p)
}

/// Normalise a verified-leg RESULT back to what a JSON-RPC node would have returned.
///
/// The proxy's encoding is not uniform with the wire format, and the differences are silent:
/// `eth_call` answers a BYTE ARRAY where a node answers a hex string, and `eth_blockNumber`
/// answers a JSON number where a node answers a hex quantity. A consumer decoding hex gets
/// garbage from the first and a type error from the second — with no error anywhere, because
/// both are perfectly valid JSON. Measured against mainnet, both directions.
///
/// Only shapes that are unambiguously the proxy's dialect are rewritten; anything already in
/// wire form passes through untouched.
pub fn normalize_verified_result(v: Value) -> Value {
    match &v {
        // A byte array — every element a small integer — is `bytes` the node would have hex'd.
        Value::Array(items)
            if !items.is_empty()
                && items.iter().all(|i| i.as_u64().is_some_and(|n| n <= 255)) =>
        {
            let hex: String =
                items.iter().map(|i| format!("{:02x}", i.as_u64().unwrap_or(0))).collect();
            Value::String(format!("0x{hex}"))
        }
        // A bare number where the wire carries a QUANTITY.
        Value::Number(n) if n.is_u64() => Value::String(format!("0x{:x}", n.as_u64().unwrap_or(0))),
        _ => v,
    }
}

/// Supplies the verified leg. Implemented in the Logos glue, which owns the module call, so
/// `rpc.rs` stays free of Logos dependencies and the routing DECISION stays unit-testable.
pub trait VerifiedRouter: Send + Sync {
    /// Dispatch through the proxy. `Err` is a refusal, never a licence to fall back.
    fn call(&self, chain_id: u64, method: &str, params: &Value) -> std::result::Result<Value, String>;
}

/// The RPC client: a persisted map of chainId → [`ChainConfig`].
pub struct EthRpc {
    chains: HashMap<u64, ChainConfig>,
    store_path: Option<PathBuf>,
    verified: Option<std::sync::Arc<dyn VerifiedRouter>>,
}

impl EthRpc {
    pub fn new() -> Self {
        Self { chains: HashMap::new(), store_path: None, verified: None }
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

    pub fn set_verified_router(&mut self, r: std::sync::Arc<dyn VerifiedRouter>) {
        self.verified = Some(r);
    }

    pub fn set_chain_config(&mut self, chain_id: u64, cfg: ChainConfig) -> std::result::Result<(), String> {
        cfg.validate()?;
        self.chains.insert(chain_id, cfg);
        self.persist();
        Ok(())
    }

    /// Seed a chain only where it is ABSENT, per field. `chains.json` is shared with other
    /// wallets on this device: a blanket overwrite silently retunes theirs, and a blanket skip
    /// leaves a stale value we own. Returns the fields actually written.
    pub fn ensure_chain_config(&mut self, chain_id: u64, cfg: &ChainConfig) -> Vec<String> {
        let mut seeded = Vec::new();
        match self.chains.get_mut(&chain_id) {
            None => {
                self.chains.insert(chain_id, cfg.clone());
                seeded.push("*".to_string());
            }
            Some(existing) => {
                if existing.endpoint.trim().is_empty() && !cfg.endpoint.trim().is_empty() {
                    existing.endpoint = cfg.endpoint.clone();
                    seeded.push("endpoint".into());
                }
            }
        }
        if !seeded.is_empty() {
            self.persist();
        }
        seeded
    }

    /// Overwrite only the transport timeouts this module owns. Lowering a DEFAULT is useless
    /// without this: an existing chains.json already carries the old value and `load` prefers
    /// what is on disk.
    pub fn patch_chain_transport(&mut self, chain_id: u64, timeout_secs: Option<u64>, verified_timeout_secs: Option<u64>) -> bool {
        let Some(c) = self.chains.get_mut(&chain_id) else { return false };
        if let Some(t) = timeout_secs { c.timeout_secs = t; }
        if let Some(t) = verified_timeout_secs { c.verified_timeout_secs = t; }
        self.persist();
        true
    }

    pub fn set_verified_proxy_mode(&mut self, chain_id: u64, mode: VerifiedProxyMode) -> std::result::Result<(), String> {
        let Some(c) = self.chains.get_mut(&chain_id) else {
            return Err(format!("no configuration for chain {chain_id}"));
        };
        let previous = c.verified_proxy_mode;
        c.verified_proxy_mode = mode;
        if let Err(e) = c.validate() {
            c.verified_proxy_mode = previous;
            return Err(e);
        }
        self.persist();
        Ok(())
    }

    pub fn verified_timeout(&self, chain_id: u64) -> Option<u64> {
        self.chains.get(&chain_id).map(|c| c.verified_timeout_secs)
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
        self.rpc_call_routed(chain_id, method, params).map(|(v, _)| v)
    }

    /// The routing decision, in the one place every typed method already funnels through.
    /// Returns the value and whether it is proof-backed, so a caller can report which it got
    /// rather than inferring it from the mode.
    pub fn rpc_call_routed(&self, chain_id: u64, method: &str, params: Value) -> Result<(Value, Option<VerifiedClass>)> {
        let cfg = self.chains.get(&chain_id).ok_or(RpcError::UnknownChain(chain_id))?;
        if cfg.verified_proxy_mode == VerifiedProxyMode::Required {
            let router = self
                .verified
                .as_ref()
                .ok_or_else(|| RpcError::VerifiedProxy("no verified proxy is wired up".into()))?;
            let coerced = verified_params(method, &params);
            // REFUSE on failure. Falling back would answer a request for a verified number
            // with an unverified one, which is worse than no answer at all.
            let v = router.call(chain_id, method, &coerced).map_err(RpcError::VerifiedProxy)?;
            return Ok((normalize_verified_result(v), Some(verified_class(method))));
        }
        let (client, endpoint) = self.client_for(chain_id)?;
        Self::post_rpc(&client, &endpoint, method, params).map(|v| (v, None))
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
        ChainConfig { endpoint: endpoint.into(), proxy: None, proxy_required: false, timeout_secs: 5,
            verified_proxy_mode: VerifiedProxyMode::Off, verified_timeout_secs: 15 }
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
                timeout_secs: 5, verified_proxy_mode: VerifiedProxyMode::Off, verified_timeout_secs: 15,
            },
        )
        .unwrap();
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

#[cfg(test)]
mod verified_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Records what the verified leg was asked for, so the coercion is asserted ON THE WIRE
    /// rather than by re-reading the function that performs it.
    struct SpyRouter {
        seen: Mutex<Vec<(String, Value)>>,
        answer: std::result::Result<Value, String>,
    }
    impl SpyRouter {
        fn ok(v: Value) -> Arc<Self> { Arc::new(Self { seen: Mutex::new(vec![]), answer: Ok(v) }) }
        fn failing() -> Arc<Self> {
            Arc::new(Self { seen: Mutex::new(vec![]), answer: Err("proxy is not running".into()) })
        }
        fn last(&self) -> (String, Value) { self.seen.lock().unwrap().last().cloned().unwrap() }
        fn count(&self) -> usize { self.seen.lock().unwrap().len() }
    }
    impl VerifiedRouter for SpyRouter {
        fn call(&self, _c: u64, m: &str, p: &Value) -> std::result::Result<Value, String> {
            self.seen.lock().unwrap().push((m.to_string(), p.clone()));
            self.answer.clone()
        }
    }

    fn cfg(mode: VerifiedProxyMode) -> ChainConfig {
        ChainConfig {
            // Deliberately unreachable: if routing ever falls through to the direct path the
            // test fails, rather than quietly passing against a real node.
            endpoint: "http://127.0.0.1:1/never".into(),
            proxy: None, proxy_required: false, timeout_secs: 1,
            verified_proxy_mode: mode, verified_timeout_secs: 1,
        }
    }
    fn verified_rpc(router: Arc<SpyRouter>) -> EthRpc {
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(VerifiedProxyMode::Required)).unwrap();
        r.set_verified_router(router);
        r
    }

    #[test]
    fn fee_history_block_count_becomes_a_number_on_the_verified_leg_only() {
        let spy = SpyRouter::ok(json!({ "baseFeePerGas": ["0x1"] }));
        let r = verified_rpc(spy.clone());
        r.fee_history(1, 4, json!([25])).unwrap();
        let (m, p) = spy.last();
        assert_eq!(m, "eth_feeHistory");
        assert_eq!(p[0], json!(4), "a hex blockCount returns success with EMPTY arrays");
        // The spec-conformant hex is untouched off the verified leg: verified_params is the
        // ONLY place this differs, so a real node still gets what the spec says.
        assert_eq!(verified_params("eth_getBalance", &json!(["0xa", "latest"]))[1], json!("latest"));
    }

    #[test]
    fn pending_becomes_latest_because_pending_is_unverifiable() {
        let spy = SpyRouter::ok(json!("0x7"));
        let r = verified_rpc(spy.clone());
        r.get_transaction_count(1, "0xabc").unwrap();
        assert_eq!(spy.last().1[1], json!("latest"), "a light client has no header for `pending`");
    }

    #[test]
    fn estimate_gas_and_call_gain_the_upstream_third_parameter() {
        let spy = SpyRouter::ok(json!("0x5208"));
        let r = verified_rpc(spy.clone());
        r.estimate_gas(1, json!({ "to": "0xabc" })).unwrap();
        let (_, p) = spy.last();
        assert_eq!(p.as_array().unwrap().len(), 3, "a one-arg estimate_gas is refused outright");
        assert_eq!(p[2], json!(false));
        r.call(1, json!({ "to": "0xabc" })).unwrap();
        assert_eq!(spy.last().1.as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_failing_verified_leg_refuses_and_never_falls_back() {
        let spy = SpyRouter::failing();
        let r = verified_rpc(spy.clone());
        let e = r.get_balance(1, "0xabc").unwrap_err();
        assert!(matches!(e, RpcError::VerifiedProxy(_)), "got {e}");
        assert!(e.to_string().contains("proxy is not running"));
        assert_eq!(spy.count(), 1, "one attempt, and no retry against the endpoint");
    }

    #[test]
    fn a_byte_array_result_is_normalised_back_to_a_hex_string() {
        // eth_call through the proxy answers a BYTE ARRAY where a node answers "0x...".
        // Measured on mainnet: symbol() on WETH came back as [0,0,...,87,69,84,72,...].
        let spy = SpyRouter::ok(json!([0, 32, 87, 69, 84, 72]));
        let r = verified_rpc(spy);
        assert_eq!(r.call(1, json!({ "to": "0xabc" })).unwrap(), "0x00205745544 8".replace(" ", ""));
    }

    #[test]
    fn a_bare_number_result_is_normalised_back_to_a_hex_quantity() {
        // eth_blockNumber answers a JSON number through the proxy and a hex string directly.
        let spy = SpyRouter::ok(json!(11579774u64));
        let r = verified_rpc(spy);
        assert_eq!(r.block_number(1).unwrap(), "0xb0b17e");
    }

    #[test]
    fn a_result_already_in_wire_form_is_left_alone() {
        let spy = SpyRouter::ok(json!("0x1afd6402e6259dc342259"));
        let r = verified_rpc(spy);
        assert_eq!(r.get_balance(1, "0xabc").unwrap(), "0x1afd6402e6259dc342259");
        // …and a structured reply keeps its shape: feeHistory's arrays hold hex STRINGS, so
        // the byte-array rule must not fire on them.
        let spy2 = SpyRouter::ok(json!({ "baseFeePerGas": ["0x59bceec"], "oldestBlock": "0x1" }));
        let r2 = verified_rpc(spy2);
        let v = r2.fee_history(1, 4, json!([50])).unwrap();
        assert_eq!(v["baseFeePerGas"][0], json!("0x59bceec"));
    }

    #[test]
    fn only_proof_backed_reads_are_classed_verified() {
        for m in ["eth_getBalance", "eth_getTransactionCount", "eth_getCode",
                  "eth_getStorageAt", "eth_call"] {
            assert_eq!(verified_class(m), VerifiedClass::Verified, "{m}");
        }
        // A light client cannot prove a fee oracle's opinion or that a broadcast landed.
        for m in ["eth_gasPrice", "eth_maxPriorityFeePerGas", "eth_feeHistory",
                  "eth_estimateGas", "eth_sendRawTransaction", "eth_blockNumber"] {
            assert_eq!(verified_class(m), VerifiedClass::Proxied, "{m}");
        }
    }

    #[test]
    fn off_mode_never_touches_the_router() {
        let spy = SpyRouter::ok(json!("0x1"));
        let mut r = EthRpc::new();
        r.set_chain_config(1, cfg(VerifiedProxyMode::Off)).unwrap();
        r.set_verified_router(spy.clone());
        let _ = r.get_balance(1, "0xabc"); // fails on the dead endpoint, which is the point
        assert_eq!(spy.count(), 0, "off mode must not route");
    }

    #[test]
    fn a_socks_proxy_and_verified_routing_cannot_both_be_required() {
        let mut r = EthRpc::new();
        let bad = ChainConfig {
            endpoint: "https://x".into(),
            proxy: Some("socks5h://127.0.0.1:9050".into()),
            proxy_required: true, timeout_secs: 8,
            verified_proxy_mode: VerifiedProxyMode::Required, verified_timeout_secs: 15,
        };
        assert!(bad.validate().is_err());
        assert!(r.set_chain_config(1, bad).is_err(), "the store must refuse the contradiction");
        assert!(r.get_chain_config(1).is_none());
    }

    #[test]
    fn ensure_seeds_an_absent_chain_but_never_clobbers_a_configured_endpoint() {
        let mut r = EthRpc::new();
        let mine = cfg(VerifiedProxyMode::Off);
        assert_eq!(r.ensure_chain_config(1, &mine), vec!["*".to_string()]);
        let theirs = ChainConfig { endpoint: "https://theirs".into(), ..mine.clone() };
        assert!(r.ensure_chain_config(1, &theirs).is_empty(), "an existing endpoint is the user's");
        assert_eq!(r.get_chain_config(1).unwrap().endpoint, "http://127.0.0.1:1/never");
    }

    #[test]
    fn patch_rewrites_the_timeouts_we_own_and_nothing_else() {
        let mut r = EthRpc::new();
        let mut c = cfg(VerifiedProxyMode::Off);
        c.timeout_secs = 30;
        c.endpoint = "https://mine".into();
        r.set_chain_config(1, c).unwrap();
        assert!(r.patch_chain_transport(1, Some(8), Some(15)));
        let got = r.get_chain_config(1).unwrap();
        assert_eq!((got.timeout_secs, got.verified_timeout_secs), (8, 15));
        assert_eq!(got.endpoint, "https://mine", "the user's endpoint is untouched");
        assert!(!r.patch_chain_transport(999, Some(8), None));
    }

    #[test]
    fn switching_mode_on_a_socks_required_chain_is_refused_and_rolled_back() {
        let mut r = EthRpc::new();
        let mut c = cfg(VerifiedProxyMode::Off);
        c.proxy = Some("socks5h://1".into());
        c.proxy_required = true;
        r.set_chain_config(1, c).unwrap();
        assert!(r.set_verified_proxy_mode(1, VerifiedProxyMode::Required).is_err());
        assert_eq!(r.get_chain_config(1).unwrap().verified_proxy_mode, VerifiedProxyMode::Off,
                   "a refused switch must not leave the mode changed");
    }
}
