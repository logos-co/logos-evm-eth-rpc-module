//! Logos module glue for `eth_rpc_module` (rust-first authoring).
//!
//! The builder derives the `.lidl` from the `EthRpcModule` trait below
//! (`codegen.rust = { trait, source: "src/glue.rs" }`). Compiled only with the
//! default `logos_module` feature; `cargo test --no-default-features` exercises
//! the `rpc` + `proxy` cores without the Logos runtime.
//!
//! Config is keyed + persisted per chain (`set_chain_config`); every RPC method
//! takes only a `chain_id`. Structured values cross as JSON strings;
//! `{ "ok": true, ... }` / `{ "ok": false, "error": "..." }`.
//!
//! `concurrency: "multi"` (metadata.json): every RPC method is a blocking
//! network round-trip (up to `timeoutSecs`, default 30s), so the module opts into
//! concurrent dispatch — one slow call no longer stalls the others. The multi
//! contract makes the generated trait take `&self` + `Send + Sync`, so the state
//! lives behind a `RwLock`: the 14 RPC handlers only read it (and run
//! concurrently — many readers), while the two rare config mutators take the
//! write lock.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use logos_rust_sdk::LogosModuleSDK;

use serde_json::{json, Value};

use crate::rpc::{ChainConfig, EthRpc, VerifiedProxyMode, VerifiedRouter};

pub trait EthRpcModule: Send + Sync + 'static {
    /// Store config for a chain. `config_json`: `{ endpoint, proxy?, proxyRequired?, timeoutSecs? }`.
    fn set_chain_config(&self, chain_id: i64, config_json: String) -> bool;
    fn get_chain_config(&self, chain_id: i64) -> String;
    fn remove_chain_config(&self, chain_id: i64) -> bool;
    /// `{ ok, chains: [chainId, ...] }`.
    fn list_chains(&self) -> String;

    /// `eth_chainId` round-trip → `{ ok, chainId }`.
    fn verify_chain_id(&self, chain_id: i64) -> String;
    fn block_number(&self, chain_id: i64) -> String;
    fn get_balance(&self, chain_id: i64, address: String) -> String;
    /// `eth_call` — `call_json` is a `{ to, data }` object (ERC20 reads).
    fn call(&self, chain_id: i64, call_json: String) -> String;
    fn get_transaction_count(&self, chain_id: i64, address: String) -> String;
    fn gas_price(&self, chain_id: i64) -> String;
    fn fee_history(&self, chain_id: i64, blocks: i64, reward_percentiles_json: String) -> String;
    fn estimate_gas(&self, chain_id: i64, tx_json: String) -> String;
    fn send_raw_transaction(&self, chain_id: i64, raw_hex: String) -> String;
    fn get_transaction_receipt(&self, chain_id: i64, hash_hex: String) -> String;
    fn get_transaction_by_hash(&self, chain_id: i64, hash_hex: String) -> String;
    /// Escape hatch for any standard JSON-RPC method. `params_json` is a JSON array.
    fn raw_rpc(&self, chain_id: i64, method: String, params_json: String) -> String;
    /// Like [`Self::raw_rpc`] but POSTs to an explicit `url` (not the chain's
    /// configured endpoint), reusing `chain_id`'s fail-closed proxied client. For
    /// off-chain JSON-RPC tied to a chain — e.g. an ERC-4337 bundler
    /// (`eth_sendUserOperation`) — so it too goes through net-proxy. `params_json`
    /// is a JSON array.
    fn raw_rpc_url(&self, chain_id: i64, url: String, method: String, params_json: String) -> String;

    /// Seed a chain only where it is ABSENT, per field, returning which fields were written.
    /// `chains.json` is shared with other wallets on this device: a blanket overwrite silently
    /// retunes theirs, a blanket skip leaves a stale value we own.
    fn ensure_chain_config(&self, chain_id: i64, config_json: String) -> String;
    /// Overwrite only the transport timeouts this module owns. Lowering a default is useless
    /// without this — an existing chains.json already carries the old value. 0 leaves a field.
    fn patch_chain_transport(&self, chain_id: i64, timeout_secs: i64, verified_timeout_secs: i64) -> String;
    /// `"off"` talks to the endpoint; `"required"` routes through the light-client proxy and
    /// REFUSES rather than falling back. There is no `preferred`: answering from an unverified
    /// source when verification was asked for is the failure this prevents.
    fn set_verified_proxy_mode(&self, chain_id: i64, mode: String) -> String;
    /// `{ ok, usable, reason? }` — whether this chain could route right now, and why not.
    fn verified_proxy_status(&self, chain_id: i64) -> String;

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

// ── the verified leg ──────────────────────────────────────────────────────────────────
//
// Reached module-to-module but UNTYPED, and with no metadata.json dependency — deliberately.
// Declaring verified_proxy_module as a real dependency makes collectAllModuleDeps resolve it
// for every consumer of THIS module, dragging libverifproxy and the whole nimbus closure into
// the multi-chain wallet, its doctests and ours, and taking the EVM stack off Windows until
// nimbus' Nim requirement lands. An untyped call is the same hop without any of that, and
// nothing typed is lost because this contract is JSON either way.

/// Bounded so an ABSENT proxy costs a second, not the 20s protocol deadline. An untyped call
/// to an unloaded module blocks for the full default timeout — the async variant included,
/// which only moves the stall to another thread.
const PROBE_BUDGET: Duration = Duration::from_millis(1500);

/// How long a health verdict is reused: long enough that a burst of reads costs one probe,
/// short enough that a proxy falling over is noticed within seconds.
const HEALTH_TTL: Duration = Duration::from_secs(5);

struct VerifiedProxyRouter {
    health: std::sync::Mutex<HashMap<u64, (Instant, std::result::Result<(), String>)>>,
    budget: std::sync::Mutex<HashMap<u64, Duration>>,
}

impl VerifiedProxyRouter {
    fn new() -> Self {
        Self {
            health: std::sync::Mutex::new(HashMap::new()),
            budget: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn set_budget(&self, chain_id: u64, secs: u64) {
        if let Ok(mut b) = self.budget.lock() {
            b.insert(chain_id, Duration::from_secs(secs.clamp(1, 60)));
        }
    }

    fn budget_for(&self, chain_id: u64) -> Duration {
        self.budget.lock().ok().and_then(|b| b.get(&chain_id).copied())
            .unwrap_or(Duration::from_secs(15))
    }

    /// `status` is declared `-> {tstr: any}`, NOT `-> result`, so it answers the object
    /// itself rather than a {success,value,error} envelope. Reading `value.state` here would
    /// silently see nothing and call every healthy proxy unusable.
    fn probe(&self, chain_id: u64) -> std::result::Result<(), String> {
        let raw = LogosModuleSDK::new()
            .plugin("verified_proxy_module")
            .call_json_with_timeout("status", &json!([]), PROBE_BUDGET)
            .map_err(|e| format!("not reachable ({e:?})"))?;

        match raw.get("state").and_then(Value::as_str) {
            Some("running") => {}
            // `degraded` means the light client's heartbeat is failing. ProxyRuntime::call
            // would still ACCEPT it; a verified path should not — a degraded head is exactly
            // when an answer stops meaning what it claims.
            Some(other) => {
                let why = raw.get("lastError").and_then(Value::as_str).unwrap_or("");
                return Err(if why.is_empty() {
                    format!("state is '{other}', not running")
                } else {
                    format!("state is '{other}': {why}")
                });
            }
            None => return Err("status carried no state".into()),
        }

        // A proxy synced to a DIFFERENT chain would answer confidently and wrongly.
        let theirs = raw.get("chainId").and_then(Value::as_i64).unwrap_or(0);
        if theirs != chain_id as i64 {
            return Err(format!("proxy is on chain {theirs}, this request is for {chain_id}"));
        }

        // A head that has stopped advancing is the documented failure: without a heartbeat the
        // verified head goes BACKWARDS, long after start() reported success.
        if raw.get("head").and_then(|h| h.get("blockNumber")).and_then(Value::as_str)
            .unwrap_or("").is_empty()
        {
            return Err("proxy has no verified head yet".into());
        }
        Ok(())
    }

    fn healthy(&self, chain_id: u64) -> std::result::Result<(), String> {
        if let Ok(cache) = self.health.lock() {
            if let Some((at, verdict)) = cache.get(&chain_id) {
                if at.elapsed() < HEALTH_TTL {
                    return verdict.clone();
                }
            }
        }
        let verdict = self.probe(chain_id);
        if let Ok(mut cache) = self.health.lock() {
            cache.insert(chain_id, (Instant::now(), verdict.clone()));
        }
        verdict
    }
}

impl VerifiedRouter for VerifiedProxyRouter {
    fn call(&self, chain_id: u64, method: &str, params: &Value) -> std::result::Result<Value, String> {
        self.healthy(chain_id)?;
        let raw = LogosModuleSDK::new()
            .plugin("verified_proxy_module")
            .call_json_with_timeout("rpc", &json!([method, params]), self.budget_for(chain_id))
            .map_err(|e| format!("{method} failed ({e:?})"))?;

        // `rpc` IS declared `-> result`, so this one DOES carry the envelope. The two methods
        // differ, and the SDK unwraps neither.
        if raw.get("success").and_then(Value::as_bool) != Some(true) {
            let why = raw.get("error").and_then(Value::as_str).unwrap_or("no detail");
            return Err(format!("{method}: {why}"));
        }
        Ok(raw.get("value").cloned().unwrap_or(Value::Null))
    }
}


#[derive(Default)]
struct EthRpcModuleImpl {
    rpc: RwLock<Option<EthRpc>>,
}

impl EthRpcModuleImpl {
    /// Run `f` against the initialized `EthRpc` under a READ lock — concurrent
    /// callers each take a shared read lock, so their (blocking) RPC round-trips
    /// overlap. Returns the not-initialized error string if context isn't ready.
    fn with_rpc(&self, f: impl FnOnce(&EthRpc) -> String) -> String {
        match self.rpc.read().unwrap().as_ref() {
            Some(rpc) => f(rpc),
            None => err("eth_rpc not initialized (context not ready)"),
        }
    }

    /// Run `f` against the initialized `EthRpc` under a WRITE lock (the two rare
    /// config mutators). Returns `false` if context isn't ready.
    /// Like `with_rpc_mut` but for methods answering `{ok,...}` rather than a bare bool — a
    /// bool cannot say WHY, and every config method here can fail for a reason the caller needs.
    fn with_rpc_mut_res(&self, f: impl FnOnce(&mut EthRpc) -> Value) -> String {
        match self.rpc.write() {
            Ok(mut g) => match g.as_mut() {
                Some(rpc) => f(rpc).to_string(),
                None => err("eth_rpc not initialized (context not ready)"),
            },
            Err(_) => err("eth_rpc lock poisoned"),
        }
    }

    fn with_rpc_mut(&self, f: impl FnOnce(&mut EthRpc) -> bool) -> bool {
        match self.rpc.write().unwrap().as_mut() {
            Some(rpc) => f(rpc),
            None => false,
        }
    }
}

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

fn ok_result(v: Value) -> String {
    json!({ "ok": true, "result": v }).to_string()
}

fn parse_json(s: &str) -> std::result::Result<Value, String> {
    serde_json::from_str(s).map_err(|e| e.to_string())
}

impl EthRpcModule for EthRpcModuleImpl {
    fn on_context_ready(&self, ctx: &RustModuleContext) {
        let path = std::path::Path::new(&ctx.instance_persistence_path).join("chains.json");
        let mut rpc = EthRpc::with_store(path);
        // Wired unconditionally: it costs nothing until a chain is set to `required`, and is
        // never CALLED until then, so a deployment with no verified proxy is unaffected.
        rpc.set_verified_router(std::sync::Arc::new(VerifiedProxyRouter::new()));
        *self.rpc.write().unwrap() = Some(rpc);
    }

    fn set_chain_config(&self, chain_id: i64, config_json: String) -> bool {
        let cfg: ChainConfig = match serde_json::from_str(&config_json) {
            Ok(c) => c,
            Err(_) => return false,
        };
        self.with_rpc_mut(|rpc| rpc.set_chain_config(chain_id as u64, cfg).is_ok())
    }

    fn get_chain_config(&self, chain_id: i64) -> String {
        self.with_rpc(|rpc| match rpc.get_chain_config(chain_id as u64) {
            Some(c) => json!({ "ok": true, "config": c }).to_string(),
            None => err(format!("no config for chain {chain_id}")),
        })
    }

    fn remove_chain_config(&self, chain_id: i64) -> bool {
        self.with_rpc_mut(|rpc| rpc.remove_chain_config(chain_id as u64))
    }

    fn list_chains(&self) -> String {
        self.with_rpc(|rpc| json!({ "ok": true, "chains": rpc.list_chains() }).to_string())
    }

    fn verify_chain_id(&self, chain_id: i64) -> String {
        self.with_rpc(|rpc| match rpc.verify_chain_id(chain_id as u64) {
            Ok(id) => json!({ "ok": true, "chainId": id }).to_string(),
            Err(e) => err(e),
        })
    }

    fn block_number(&self, chain_id: i64) -> String {
        self.with_rpc(|rpc| match rpc.block_number(chain_id as u64) {
            Ok(v) => ok_result(Value::String(v)),
            Err(e) => err(e),
        })
    }

    fn get_balance(&self, chain_id: i64, address: String) -> String {
        self.with_rpc(|rpc| match rpc.get_balance(chain_id as u64, &address) {
            Ok(v) => ok_result(Value::String(v)),
            Err(e) => err(e),
        })
    }

    fn call(&self, chain_id: i64, call_json: String) -> String {
        let call = match parse_json(&call_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        self.with_rpc(|rpc| match rpc.call(chain_id as u64, call) {
            Ok(v) => ok_result(Value::String(v)),
            Err(e) => err(e),
        })
    }

    fn get_transaction_count(&self, chain_id: i64, address: String) -> String {
        self.with_rpc(|rpc| match rpc.get_transaction_count(chain_id as u64, &address) {
            Ok(v) => ok_result(Value::String(v)),
            Err(e) => err(e),
        })
    }

    fn gas_price(&self, chain_id: i64) -> String {
        self.with_rpc(|rpc| match rpc.gas_price(chain_id as u64) {
            Ok(v) => ok_result(Value::String(v)),
            Err(e) => err(e),
        })
    }

    fn fee_history(&self, chain_id: i64, blocks: i64, reward_percentiles_json: String) -> String {
        let pct = match parse_json(&reward_percentiles_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        self.with_rpc(|rpc| match rpc.fee_history(chain_id as u64, blocks.max(0) as u64, pct) {
            Ok(v) => ok_result(v),
            Err(e) => err(e),
        })
    }

    fn estimate_gas(&self, chain_id: i64, tx_json: String) -> String {
        let tx = match parse_json(&tx_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        self.with_rpc(|rpc| match rpc.estimate_gas(chain_id as u64, tx) {
            Ok(v) => ok_result(Value::String(v)),
            Err(e) => err(e),
        })
    }

    fn send_raw_transaction(&self, chain_id: i64, raw_hex: String) -> String {
        self.with_rpc(|rpc| match rpc.send_raw_transaction(chain_id as u64, &raw_hex) {
            Ok(v) => json!({ "ok": true, "hash": v }).to_string(),
            Err(e) => err(e),
        })
    }

    fn get_transaction_receipt(&self, chain_id: i64, hash_hex: String) -> String {
        self.with_rpc(|rpc| match rpc.get_transaction_receipt(chain_id as u64, &hash_hex) {
            Ok(v) => ok_result(v),
            Err(e) => err(e),
        })
    }

    fn get_transaction_by_hash(&self, chain_id: i64, hash_hex: String) -> String {
        self.with_rpc(|rpc| match rpc.get_transaction_by_hash(chain_id as u64, &hash_hex) {
            Ok(v) => ok_result(v),
            Err(e) => err(e),
        })
    }

    fn raw_rpc(&self, chain_id: i64, method: String, params_json: String) -> String {
        let params = match parse_json(&params_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        self.with_rpc(|rpc| match rpc.rpc_call(chain_id as u64, &method, params) {
            Ok(v) => ok_result(v),
            Err(e) => err(e),
        })
    }

    fn ensure_chain_config(&self, chain_id: i64, config_json: String) -> String {
        let cfg: ChainConfig = match serde_json::from_str(&config_json) {
            Ok(c) => c,
            Err(e) => return err(e),
        };
        if let Err(e) = cfg.validate() {
            return err(e);
        }
        self.with_rpc_mut_res(|rpc| {
            json!({ "ok": true, "seeded": rpc.ensure_chain_config(chain_id as u64, &cfg) })
        })
    }

    fn patch_chain_transport(&self, chain_id: i64, timeout_secs: i64, verified_timeout_secs: i64) -> String {
        self.with_rpc_mut_res(|rpc| {
            let t = (timeout_secs > 0).then_some(timeout_secs as u64);
            let vt = (verified_timeout_secs > 0).then_some(verified_timeout_secs as u64);
            json!({ "ok": rpc.patch_chain_transport(chain_id as u64, t, vt) })
        })
    }

    fn set_verified_proxy_mode(&self, chain_id: i64, mode: String) -> String {
        let m = match mode.trim().to_ascii_lowercase().as_str() {
            "off" => VerifiedProxyMode::Off,
            "required" => VerifiedProxyMode::Required,
            other => return err(format!("unknown verified proxy mode '{other}' (expected off or required)")),
        };
        self.with_rpc_mut_res(|rpc| match rpc.set_verified_proxy_mode(chain_id as u64, m) {
            Ok(()) => json!({ "ok": true, "chainId": chain_id, "verifiedProxyMode": m }),
            Err(e) => json!({ "ok": false, "error": e }),
        })
    }

    /// Ungated observability: a user turning the toggle on deserves to know why it refuses.
    fn verified_proxy_status(&self, chain_id: i64) -> String {
        match VerifiedProxyRouter::new().probe(chain_id as u64) {
            Ok(()) => json!({ "ok": true, "usable": true, "chainId": chain_id }).to_string(),
            Err(e) => json!({ "ok": true, "usable": false, "chainId": chain_id, "reason": e }).to_string(),
        }
    }

    fn raw_rpc_url(&self, chain_id: i64, url: String, method: String, params_json: String) -> String {
        let params = match parse_json(&params_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        self.with_rpc(|rpc| match rpc.rpc_call_url(chain_id as u64, &url, &method, params) {
            Ok(v) => ok_result(v),
            Err(e) => err(e),
        })
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<EthRpcModuleImpl>();
}
