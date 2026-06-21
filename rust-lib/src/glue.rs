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

use std::sync::RwLock;

use serde_json::{json, Value};

use crate::rpc::{ChainConfig, EthRpc};

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

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

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
        *self.rpc.write().unwrap() = Some(EthRpc::with_store(path));
    }

    fn set_chain_config(&self, chain_id: i64, config_json: String) -> bool {
        let cfg: ChainConfig = match serde_json::from_str(&config_json) {
            Ok(c) => c,
            Err(_) => return false,
        };
        self.with_rpc_mut(|rpc| {
            rpc.set_chain_config(chain_id as u64, cfg);
            true
        })
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
