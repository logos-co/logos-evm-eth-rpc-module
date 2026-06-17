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

use serde_json::{json, Value};

use crate::rpc::{ChainConfig, EthRpc};

pub trait EthRpcModule: Send + 'static {
    /// Store config for a chain. `config_json`: `{ endpoint, proxy?, proxyRequired?, timeoutSecs? }`.
    fn set_chain_config(&mut self, chain_id: i64, config_json: String) -> bool;
    fn get_chain_config(&mut self, chain_id: i64) -> String;
    fn remove_chain_config(&mut self, chain_id: i64) -> bool;
    /// `{ ok, chains: [chainId, ...] }`.
    fn list_chains(&mut self) -> String;

    /// `eth_chainId` round-trip → `{ ok, chainId }`.
    fn verify_chain_id(&mut self, chain_id: i64) -> String;
    fn block_number(&mut self, chain_id: i64) -> String;
    fn get_balance(&mut self, chain_id: i64, address: String) -> String;
    /// `eth_call` — `call_json` is a `{ to, data }` object (ERC20 reads).
    fn call(&mut self, chain_id: i64, call_json: String) -> String;
    fn get_transaction_count(&mut self, chain_id: i64, address: String) -> String;
    fn gas_price(&mut self, chain_id: i64) -> String;
    fn fee_history(&mut self, chain_id: i64, blocks: i64, reward_percentiles_json: String) -> String;
    fn estimate_gas(&mut self, chain_id: i64, tx_json: String) -> String;
    fn send_raw_transaction(&mut self, chain_id: i64, raw_hex: String) -> String;
    fn get_transaction_receipt(&mut self, chain_id: i64, hash_hex: String) -> String;
    fn get_transaction_by_hash(&mut self, chain_id: i64, hash_hex: String) -> String;
    /// Escape hatch for any standard JSON-RPC method. `params_json` is a JSON array.
    fn raw_rpc(&mut self, chain_id: i64, method: String, params_json: String) -> String;

    fn on_context_ready(&mut self, _ctx: &RustModuleContext) {}
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

#[derive(Default)]
struct EthRpcModuleImpl {
    rpc: Option<EthRpc>,
}

impl EthRpcModuleImpl {
    fn rpc(&mut self) -> std::result::Result<&mut EthRpc, String> {
        self.rpc.as_mut().ok_or_else(|| "eth_rpc not initialized (context not ready)".to_string())
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
    fn on_context_ready(&mut self, ctx: &RustModuleContext) {
        let path = std::path::Path::new(&ctx.instance_persistence_path).join("chains.json");
        self.rpc = Some(EthRpc::with_store(path));
    }

    fn set_chain_config(&mut self, chain_id: i64, config_json: String) -> bool {
        let cfg: ChainConfig = match serde_json::from_str(&config_json) {
            Ok(c) => c,
            Err(_) => return false,
        };
        match self.rpc() {
            Ok(rpc) => {
                rpc.set_chain_config(chain_id as u64, cfg);
                true
            }
            Err(_) => false,
        }
    }

    fn get_chain_config(&mut self, chain_id: i64) -> String {
        match self.rpc() {
            Ok(rpc) => match rpc.get_chain_config(chain_id as u64) {
                Some(c) => json!({ "ok": true, "config": c }).to_string(),
                None => err(format!("no config for chain {chain_id}")),
            },
            Err(e) => err(e),
        }
    }

    fn remove_chain_config(&mut self, chain_id: i64) -> bool {
        self.rpc().map(|rpc| rpc.remove_chain_config(chain_id as u64)).unwrap_or(false)
    }

    fn list_chains(&mut self) -> String {
        match self.rpc() {
            Ok(rpc) => json!({ "ok": true, "chains": rpc.list_chains() }).to_string(),
            Err(e) => err(e),
        }
    }

    fn verify_chain_id(&mut self, chain_id: i64) -> String {
        match self.rpc() {
            Ok(rpc) => match rpc.verify_chain_id(chain_id as u64) {
                Ok(id) => json!({ "ok": true, "chainId": id }).to_string(),
                Err(e) => err(e),
            },
            Err(e) => err(e),
        }
    }

    fn block_number(&mut self, chain_id: i64) -> String {
        match self.rpc() {
            Ok(rpc) => match rpc.block_number(chain_id as u64) {
                Ok(v) => ok_result(Value::String(v)),
                Err(e) => err(e),
            },
            Err(e) => err(e),
        }
    }

    fn get_balance(&mut self, chain_id: i64, address: String) -> String {
        match self.rpc() {
            Ok(rpc) => match rpc.get_balance(chain_id as u64, &address) {
                Ok(v) => ok_result(Value::String(v)),
                Err(e) => err(e),
            },
            Err(e) => err(e),
        }
    }

    fn call(&mut self, chain_id: i64, call_json: String) -> String {
        let call = match parse_json(&call_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        match self.rpc() {
            Ok(rpc) => match rpc.call(chain_id as u64, call) {
                Ok(v) => ok_result(Value::String(v)),
                Err(e) => err(e),
            },
            Err(e) => err(e),
        }
    }

    fn get_transaction_count(&mut self, chain_id: i64, address: String) -> String {
        match self.rpc() {
            Ok(rpc) => match rpc.get_transaction_count(chain_id as u64, &address) {
                Ok(v) => ok_result(Value::String(v)),
                Err(e) => err(e),
            },
            Err(e) => err(e),
        }
    }

    fn gas_price(&mut self, chain_id: i64) -> String {
        match self.rpc() {
            Ok(rpc) => match rpc.gas_price(chain_id as u64) {
                Ok(v) => ok_result(Value::String(v)),
                Err(e) => err(e),
            },
            Err(e) => err(e),
        }
    }

    fn fee_history(&mut self, chain_id: i64, blocks: i64, reward_percentiles_json: String) -> String {
        let pct = match parse_json(&reward_percentiles_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        match self.rpc() {
            Ok(rpc) => match rpc.fee_history(chain_id as u64, blocks.max(0) as u64, pct) {
                Ok(v) => ok_result(v),
                Err(e) => err(e),
            },
            Err(e) => err(e),
        }
    }

    fn estimate_gas(&mut self, chain_id: i64, tx_json: String) -> String {
        let tx = match parse_json(&tx_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        match self.rpc() {
            Ok(rpc) => match rpc.estimate_gas(chain_id as u64, tx) {
                Ok(v) => ok_result(Value::String(v)),
                Err(e) => err(e),
            },
            Err(e) => err(e),
        }
    }

    fn send_raw_transaction(&mut self, chain_id: i64, raw_hex: String) -> String {
        match self.rpc() {
            Ok(rpc) => match rpc.send_raw_transaction(chain_id as u64, &raw_hex) {
                Ok(v) => json!({ "ok": true, "hash": v }).to_string(),
                Err(e) => err(e),
            },
            Err(e) => err(e),
        }
    }

    fn get_transaction_receipt(&mut self, chain_id: i64, hash_hex: String) -> String {
        match self.rpc() {
            Ok(rpc) => match rpc.get_transaction_receipt(chain_id as u64, &hash_hex) {
                Ok(v) => ok_result(v),
                Err(e) => err(e),
            },
            Err(e) => err(e),
        }
    }

    fn get_transaction_by_hash(&mut self, chain_id: i64, hash_hex: String) -> String {
        match self.rpc() {
            Ok(rpc) => match rpc.get_transaction_by_hash(chain_id as u64, &hash_hex) {
                Ok(v) => ok_result(v),
                Err(e) => err(e),
            },
            Err(e) => err(e),
        }
    }

    fn raw_rpc(&mut self, chain_id: i64, method: String, params_json: String) -> String {
        let params = match parse_json(&params_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        match self.rpc() {
            Ok(rpc) => match rpc.rpc_call(chain_id as u64, &method, params) {
                Ok(v) => ok_result(v),
                Err(e) => err(e),
            },
            Err(e) => err(e),
        }
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<EthRpcModuleImpl>();
}
