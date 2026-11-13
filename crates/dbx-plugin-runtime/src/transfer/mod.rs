//! Host-initiated data transfer API (`host.transfer:run`).
//!
//! The gate decides *whether* a plugin may ask the host to write and *how much
//! ceremony* that write needs. It is deliberately free of I/O: the Tauri command
//! layer resolves connections, reads grants and launches the transfer, then calls
//! into the pure functions here. That keeps the security rules testable without a
//! database, an app handle, or a live plugin frame (docs/07 §3, §6, §9).
//!
//! Check order matters and mirrors `dbx-core/src/query/plugin_data.rs`: permission
//! first, then grant, and only then endpoint resolution, so an unauthorized plugin
//! cannot tell "unknown connection" from "not allowed".

pub mod gate;

pub use gate::{
    canonical_request_json, check_concurrency, check_grant, check_grant_or_dry_run, check_not_preconfirmed,
    check_readonly_target,
    check_same_endpoint, classify_risk, operations_for_request, request_hash, ConfirmOutcome, GateError,
    Risk, RiskInput, RiskLevel, RiskReason, RiskReasonCode, TableFilterView, TransferGateRequest, TransferGrant,
    TransferOperation, CONFIRM_TIMEOUT_SECS, MAX_PLUGIN_TRANSFER_RUNS, TRANSFER_PERMISSION,
};
