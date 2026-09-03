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
use std::time::Duration;

use logos_rust_sdk::LogosModuleSDK;

use serde_json::{json, Value};

use crate::rpc::{
    diff_chain, methods, mode_label, route_label, ChainChange, ChainConfig, ChainConfigWire,
    EthRpc, VerifiedClass, VerifiedProxyMode, VerifiedRouter,
};
use crate::verdict::{classify_readiness, GateCache, GateProbe, Readiness, Verdict, PROXY_MODULE};

pub trait EthRpcModule: Send + Sync + 'static {
    /// Store config for a chain. `config_json`: `{ endpoint, proxy?, proxyRequired?, timeoutSecs? }`.
    /// An OMITTED `verifiedProxyMode` / `verifiedTimeoutSecs` preserves what is stored — only an
    /// explicit `"off"` lowers the mode, so a sibling wallet cannot revoke it by silence.
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
    /// is a JSON array. On a chain set to `required` the proof-backed reads are REFUSED here:
    /// an arbitrary url cannot prove them, and answering anyway is a silent downgrade.
    fn raw_rpc_url(&self, chain_id: i64, url: String, method: String, params_json: String) -> String;

    /// Seed a chain only where it is ABSENT, per field, returning which fields were written.
    /// `chains.json` is shared with other wallets on this device: a blanket overwrite silently
    /// retunes theirs, a blanket skip leaves a stale value we own.
    fn ensure_chain_config(&self, chain_id: i64, config_json: String) -> String;
    /// Overwrite only the transport timeouts this module owns. Lowering a default is useless
    /// without this — an existing chains.json already carries the old value. 0 leaves a field.
    fn patch_chain_transport(&self, chain_id: i64, timeout_secs: i64, verified_timeout_secs: i64) -> String;
    /// Overwrite ONLY the endpoint, creating the chain with defaults if it is absent.
    /// `chains.json` is shared with other wallets on this device, so a user retyping an
    /// endpoint must not silently reset their verified-proxy mode or timeouts.
    fn patch_chain_endpoint(&self, chain_id: i64, endpoint: String) -> String;
    /// `"off"` talks to the endpoint; `"required"` routes through the light-client proxy and
    /// REFUSES rather than falling back. There is no `preferred`: answering from an unverified
    /// source when verification was asked for is the failure this prevents. This module is the
    /// single owner of the mode — a consumer keeping its own copy can disagree with the truth.
    fn set_verified_proxy_mode(&self, chain_id: i64, mode: String) -> String;
    /// The verified-proxy verdict for one chain: what state it is in, whether a verified call
    /// would answer now, and what the user has to do about it.
    /// `{ ok, chainId, mode, state, usable, blocking, message, action, detail }`.
    fn verified_proxy_status(&self, chain_id: i64) -> String;

    /// Whether a config has been SET, and what it is — no network I/O, no call to another
    /// module, so it is cheap enough for a consumer's startup path. `state` is the machine
    /// discriminator (`unready` / `unconfigured` / `configured`); never match on the message.
    /// `{ ok, state, source, chains: [{ chainId, state, source, endpoint?, verifiedProxyMode? }] }`.
    fn config_status(&self) -> String;
    /// Seed the built-in public endpoints, per chain and per FIELD, only where absent. Keyed
    /// and idempotent per chain, so a consumer may call it unconditionally; a second call from
    /// any consumer writes nothing and answers `applied: false`, which is not an error.
    /// `{ ok, applied, seeded: { "<chainId>": ["*" | "endpoint", ...] } }`.
    fn init_defaults(&self) -> String;

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

/// Typed events. Two, at two granularities, because there are two consumers.
///
/// `chain_config_changed` is the record for ONE chain — per chain and not per field, since a
/// viewer redraws the whole row anyway, and not one global "something changed", which makes a
/// three-chain consumer re-read all three for a change to one. It carries no config: this
/// module is the single owner of that record, and a copy on the event plane is a second place
/// it can go stale.
///
/// `verified_proxy_mode_changed` is the single field a wallet GATES on, carried inline so it
/// learns the direction without a read-back. It is a refinement, not an alternative: a mode
/// change emits both, so a consumer that only redraws config subscribes to one event.
pub trait EthRpcModuleEvents {
    /// One chain's stored record was added, removed, or altered. Re-read `get_chain_config`.
    fn chain_config_changed(&self, chain_id: i64);
    /// The verified-proxy gate for one chain moved. `mode` is the value now in force — `"off"`
    /// or `"required"` — and a chain whose config was removed reports `"off"`, which is what
    /// `verified_proxy_status` answers for it.
    fn verified_proxy_mode_changed(&self, chain_id: i64, mode: String);
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
//
// modules_state is called the same way and for the same reason: `dependencies` stays [] so no
// consumer of this module inherits a closure just because the gate consults the host registry.

/// Bounded so an ABSENT proxy costs a second, not the 20s protocol deadline: an untyped call to
/// an unloaded module blocks for the full default timeout, the async variant included. The
/// readiness gate skips it outright; this bound covers what the registry cannot answer.
const PROBE_BUDGET: Duration = Duration::from_millis(1500);

/// The modules_state budget, for both the readiness listing and the post-probe refinement. It
/// is a local registry lookup, not a network hop.
const MODULES_STATE_BUDGET: Duration = Duration::from_millis(750);

/// The outbound calls. The TTL policy and the memo they feed live in [`GateCache`], where they
/// are testable without the Logos runtime.
#[derive(Default)]
struct VerifiedProxyRouter {
    gate: GateCache,
}

impl VerifiedProxyRouter {
    /// The full verdict for `chain_id`, memoized. `mode_required == false` short-circuits to
    /// `disabled` without probing anything, so an `off` wallet polling the indicator pays
    /// nothing at all.
    fn verdict(&self, chain_id: u64, mode_required: bool) -> Verdict {
        if !mode_required {
            return Verdict::disabled();
        }
        self.gate.verdict(chain_id, self)
    }
}

impl GateProbe for VerifiedProxyRouter {
    /// The raw registry lookup; `GateCache` decides how long it is reused. `list_modules` and
    /// not `is_ready`: a bool cannot separate "not loaded" from "the registry has nothing to
    /// say", and only the listing carries `partial`.
    fn readiness(&self) -> Readiness {
        let listing = LogosModuleSDK::new()
            .plugin("modules_state")
            .call_json_with_timeout("list_modules", &json!([]), MODULES_STATE_BUDGET)
            .ok();
        classify_readiness(listing.as_ref())
    }

    /// `status` is declared `-> {tstr: any}`, NOT `-> result`, so it answers the object itself
    /// rather than a {success,value,error} envelope. Reading `value.state` here would silently
    /// see nothing and call every healthy proxy unusable.
    fn proxy_status(&self) -> std::result::Result<Value, String> {
        LogosModuleSDK::new()
            .plugin(PROXY_MODULE)
            .call_json_with_timeout("status", &json!([]), PROBE_BUDGET)
            .map_err(|e| format!("{e:?}"))
    }

    /// Only ever after a failed probe, where it can sharpen the reason but never veto.
    fn module_record(&self) -> Option<Value> {
        LogosModuleSDK::new()
            .plugin("modules_state")
            .call_json_with_timeout("module_record", &json!([PROXY_MODULE]), MODULES_STATE_BUDGET)
            .ok()
    }
}

impl VerifiedRouter for VerifiedProxyRouter {
    fn call(&self, chain_id: u64, method: &str, params: &Value, budget: Duration)
        -> std::result::Result<Value, String>
    {
        // The same cached verdict the UI polls gates the call: one probe serves both.
        let v = self.verdict(chain_id, true);
        if !v.usable {
            return Err(if v.detail.is_empty() {
                v.message
            } else {
                format!("{} ({})", v.message, v.detail)
            });
        }
        let raw = LogosModuleSDK::new()
            .plugin(PROXY_MODULE)
            .call_json_with_timeout("rpc", &json!([method, params]), budget)
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
    /// Held here, not built per call: a fresh router bypasses `HEALTH_TTL` entirely, so a
    /// polling UI would pay a live probe every time it asked.
    router: std::sync::Arc<VerifiedProxyRouter>,
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

    /// Run a config mutator under the WRITE lock and report what it actually moved. The
    /// snapshot pair is taken inside the lock and diffed there; the guard drops on return, so
    /// the emit in [`Self::settle`] happens with the new value already visible and on disk.
    fn write_diffed<T>(&self, chain_id: i64, f: impl FnOnce(&mut EthRpc) -> T)
        -> std::result::Result<(T, ChainChange), String>
    {
        let mut g = self.rpc.write().map_err(|_| "eth_rpc lock poisoned".to_string())?;
        let rpc = g
            .as_mut()
            .ok_or_else(|| "eth_rpc not initialized (context not ready)".to_string())?;
        let before = rpc.get_chain_config(chain_id as u64).cloned();
        let out = f(rpc);
        let change = diff_chain(before.as_ref(), rpc.get_chain_config(chain_id as u64));
        Ok((out, change))
    }

    /// The tail every mutator shares: drop the chain's memoized verdict and the host readiness
    /// under it (without this a user flipping the toggle is told about the previous answer for
    /// seconds), then announce what moved — and only what moved.
    fn settle<T>(&self, chain_id: i64, out: T, change: ChainChange) -> T {
        self.router.gate.invalidate(chain_id as u64);
        if change.config {
            emit_chain_config_changed(chain_id);
        }
        if let Some(m) = change.mode {
            emit_verified_proxy_mode_changed(chain_id, mode_label(m));
        }
        out
    }

    /// A config mutator answering `{ok,...}` rather than a bare bool — a bool cannot say WHY,
    /// and every config method here can fail for a reason the caller needs.
    fn mutate(&self, chain_id: i64, f: impl FnOnce(&mut EthRpc) -> Value) -> String {
        match self.write_diffed(chain_id, f) {
            Ok((v, change)) => self.settle(chain_id, v.to_string(), change),
            Err(e) => err(e),
        }
    }

    /// The same, for the two mutators whose contract is a bare bool.
    fn mutate_bool(&self, chain_id: i64, f: impl FnOnce(&mut EthRpc) -> bool) -> bool {
        match self.write_diffed(chain_id, f) {
            Ok((ok, change)) => self.settle(chain_id, ok, change),
            Err(_) => false,
        }
    }
}

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

/// The readiness refusal, carrying `state` so a consumer can tell "ask again in a moment" from
/// "nothing is configured" without matching on the message. The message itself is unchanged:
/// `eth_rpc_ui` renders it verbatim.
fn unready() -> String {
    json!({ "ok": false, "state": "unready",
            "error": "eth_rpc not initialized (context not ready)" })
    .to_string()
}

/// Every answer carries `route`: `"verified"` (proof-backed), `"proxied"` (forwarded by the
/// proxy on trust) or `"direct"`. Without it a consumer can only read the MODE, and would badge
/// a forwarded receipt or fee figure with the same "Verified" as a proven balance.
fn ok_result(v: Value, route: Option<VerifiedClass>) -> String {
    json!({ "ok": true, "result": v, "route": route_label(route) }).to_string()
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
        rpc.set_verified_router(self.router.clone());
        *self.rpc.write().unwrap() = Some(rpc);
    }

    fn set_chain_config(&self, chain_id: i64, config_json: String) -> bool {
        let wire: ChainConfigWire = match serde_json::from_str(&config_json) {
            Ok(c) => c,
            Err(_) => return false,
        };
        self.mutate_bool(chain_id, |rpc| rpc.apply_chain_config(chain_id as u64, wire).is_ok())
    }

    fn get_chain_config(&self, chain_id: i64) -> String {
        self.with_rpc(|rpc| match rpc.get_chain_config(chain_id as u64) {
            Some(c) => json!({ "ok": true, "config": c }).to_string(),
            None => err(format!("no config for chain {chain_id}")),
        })
    }

    fn remove_chain_config(&self, chain_id: i64) -> bool {
        self.mutate_bool(chain_id, |rpc| rpc.remove_chain_config(chain_id as u64))
    }

    fn list_chains(&self) -> String {
        self.with_rpc(|rpc| json!({ "ok": true, "chains": rpc.list_chains() }).to_string())
    }

    fn verify_chain_id(&self, chain_id: i64) -> String {
        self.with_rpc(|rpc| match rpc.verify_chain_id(chain_id as u64) {
            Ok(id) => json!({ "ok": true, "chainId": id,
                              "route": route_label(rpc.route_of(chain_id as u64, methods::CHAIN_ID)) })
                .to_string(),
            Err(e) => err(e),
        })
    }

    fn block_number(&self, chain_id: i64) -> String {
        self.with_rpc(|rpc| match rpc.block_number(chain_id as u64) {
            Ok(v) => ok_result(Value::String(v), rpc.route_of(chain_id as u64, methods::BLOCK_NUMBER)),
            Err(e) => err(e),
        })
    }

    fn get_balance(&self, chain_id: i64, address: String) -> String {
        self.with_rpc(|rpc| match rpc.get_balance(chain_id as u64, &address) {
            Ok(v) => ok_result(Value::String(v), rpc.route_of(chain_id as u64, methods::GET_BALANCE)),
            Err(e) => err(e),
        })
    }

    fn call(&self, chain_id: i64, call_json: String) -> String {
        let call = match parse_json(&call_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        self.with_rpc(|rpc| match rpc.call(chain_id as u64, call) {
            Ok(v) => ok_result(Value::String(v), rpc.route_of(chain_id as u64, methods::CALL)),
            Err(e) => err(e),
        })
    }

    fn get_transaction_count(&self, chain_id: i64, address: String) -> String {
        self.with_rpc(|rpc| match rpc.get_transaction_count(chain_id as u64, &address) {
            Ok(v) => ok_result(Value::String(v),
                               rpc.route_of(chain_id as u64, methods::GET_TRANSACTION_COUNT)),
            Err(e) => err(e),
        })
    }

    fn gas_price(&self, chain_id: i64) -> String {
        self.with_rpc(|rpc| match rpc.gas_price(chain_id as u64) {
            Ok(v) => ok_result(Value::String(v), rpc.route_of(chain_id as u64, methods::GAS_PRICE)),
            Err(e) => err(e),
        })
    }

    fn fee_history(&self, chain_id: i64, blocks: i64, reward_percentiles_json: String) -> String {
        let pct = match parse_json(&reward_percentiles_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        self.with_rpc(|rpc| match rpc.fee_history(chain_id as u64, blocks.max(0) as u64, pct) {
            Ok(v) => ok_result(v, rpc.route_of(chain_id as u64, methods::FEE_HISTORY)),
            Err(e) => err(e),
        })
    }

    fn estimate_gas(&self, chain_id: i64, tx_json: String) -> String {
        let tx = match parse_json(&tx_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        self.with_rpc(|rpc| match rpc.estimate_gas(chain_id as u64, tx) {
            Ok(v) => ok_result(Value::String(v), rpc.route_of(chain_id as u64, methods::ESTIMATE_GAS)),
            Err(e) => err(e),
        })
    }

    fn send_raw_transaction(&self, chain_id: i64, raw_hex: String) -> String {
        self.with_rpc(|rpc| match rpc.send_raw_transaction(chain_id as u64, &raw_hex) {
            Ok(v) => json!({ "ok": true, "hash": v,
                             "route": route_label(rpc.route_of(chain_id as u64,
                                                               methods::SEND_RAW_TRANSACTION)) })
                .to_string(),
            Err(e) => err(e),
        })
    }

    fn get_transaction_receipt(&self, chain_id: i64, hash_hex: String) -> String {
        self.with_rpc(|rpc| match rpc.get_transaction_receipt(chain_id as u64, &hash_hex) {
            Ok(v) => ok_result(v, rpc.route_of(chain_id as u64, methods::GET_TRANSACTION_RECEIPT)),
            Err(e) => err(e),
        })
    }

    fn get_transaction_by_hash(&self, chain_id: i64, hash_hex: String) -> String {
        self.with_rpc(|rpc| match rpc.get_transaction_by_hash(chain_id as u64, &hash_hex) {
            Ok(v) => ok_result(v, rpc.route_of(chain_id as u64, methods::GET_TRANSACTION_BY_HASH)),
            Err(e) => err(e),
        })
    }

    fn raw_rpc(&self, chain_id: i64, method: String, params_json: String) -> String {
        let params = match parse_json(&params_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        self.with_rpc(|rpc| match rpc.rpc_call(chain_id as u64, &method, params) {
            Ok(v) => ok_result(v, rpc.route_of(chain_id as u64, &method)),
            Err(e) => err(e),
        })
    }

    fn ensure_chain_config(&self, chain_id: i64, config_json: String) -> String {
        // Not a bare deserialize: `source` is on ChainConfig for the store's sake, and a caller
        // sending `"source":"default"` must not have its own write labelled a built-in.
        let cfg = match ChainConfig::from_caller_json(&config_json) {
            Ok(c) => c,
            Err(e) => return err(e),
        };
        if let Err(e) = cfg.validate() {
            return err(e);
        }
        self.mutate(chain_id, |rpc| {
            json!({ "ok": true, "seeded": rpc.ensure_chain_config(chain_id as u64, &cfg) })
        })
    }

    fn patch_chain_transport(&self, chain_id: i64, timeout_secs: i64, verified_timeout_secs: i64) -> String {
        self.mutate(chain_id, |rpc| {
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
        self.mutate(chain_id, |rpc| match rpc.set_verified_proxy_mode(chain_id as u64, m) {
            Ok(()) => json!({ "ok": true, "chainId": chain_id, "verifiedProxyMode": m }),
            Err(e) => json!({ "ok": false, "error": e }),
        })
    }

    fn patch_chain_endpoint(&self, chain_id: i64, endpoint: String) -> String {
        if endpoint.trim().is_empty() {
            return err("endpoint must not be empty");
        }
        self.mutate(chain_id, |rpc| {
            let written = rpc.patch_chain_endpoint(chain_id as u64, &endpoint);
            json!({ "ok": written, "chainId": chain_id, "endpoint": endpoint.trim() })
        })
    }

    /// Ungated observability: a user turning the toggle on deserves to know why it refuses.
    /// `ok` is false only when this module has no context — every reachable outcome, `missing`
    /// included, is an answer.
    fn verified_proxy_status(&self, chain_id: i64) -> String {
        let chain = chain_id as u64;
        let mode = match self.rpc.read() {
            Ok(g) => match g.as_ref() {
                Some(rpc) => rpc.get_chain_config(chain).map(|c| c.verified_proxy_mode),
                None => return err("eth_rpc not initialized (context not ready)"),
            },
            Err(_) => return err("eth_rpc lock poisoned"),
        };
        // No config at all is `off`, not an error: a wallet before the user has set an
        // endpoint must not show a red indicator.
        let required = mode == Some(VerifiedProxyMode::Required);
        let mut v = self.router.verdict(chain, required);
        if mode.is_none() {
            v.detail = format!("no configuration for chain {chain_id}");
        }
        v.to_json(chain_id, required).to_string()
    }

    fn config_status(&self) -> String {
        match self.rpc.read() {
            Ok(g) => match g.as_ref() {
                Some(rpc) => rpc.config_status().to_string(),
                None => unready(),
            },
            Err(_) => err("eth_rpc lock poisoned"),
        }
    }

    fn init_defaults(&self) -> String {
        let seeded = {
            let mut guard = match self.rpc.write() {
                Ok(g) => g,
                Err(_) => return err("eth_rpc lock poisoned"),
            };
            match guard.as_mut() {
                Some(rpc) => rpc.init_defaults(),
                None => return unready(),
            }
        };
        // Same reason `mutate` does it: a memoized verdict for a chain we just seeded is stale.
        // Seeding only ever fills an ABSENT field with a builtin record, whose mode is `off` —
        // the value an unconfigured chain already reported — so the gate never moves here.
        let mut applied = false;
        let mut fields = serde_json::Map::new();
        for (id, written) in &seeded {
            if !written.is_empty() {
                applied = true;
                self.router.gate.invalidate(*id);
                emit_chain_config_changed(*id as i64);
            }
            fields.insert(id.to_string(), json!(written));
        }
        json!({ "ok": true, "applied": applied, "seeded": fields }).to_string()
    }

    fn raw_rpc_url(&self, chain_id: i64, url: String, method: String, params_json: String) -> String {
        let params = match parse_json(&params_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        // Always `direct`: this path never goes through the verified proxy, so labelling it from
        // the chain's mode would call a bundler's answer "proxied" when nothing proxied it.
        self.with_rpc(|rpc| match rpc.rpc_call_url(chain_id as u64, &url, &method, params) {
            Ok(v) => ok_result(v, None),
            Err(e) => err(e),
        })
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<EthRpcModuleImpl>();
}
