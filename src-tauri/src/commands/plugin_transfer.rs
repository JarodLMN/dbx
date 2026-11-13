//! Plugin facing transfer commands (`host.transfer:run`, docs/07 §4/§9/§10).
//!
//! Everything here is called by the host bridge, never by the plugin directly: the
//! `pluginId` argument is injected by the bridge, and the confirm dialog lives in the
//! app shell (an in-app modal). A plugin can therefore *ask* for a transfer and
//! *observe* its own run, but it cannot approve one, and it cannot touch another
//! plugin's run.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, State};

use crate::commands::connection::{ensure_connection_writable, AppState};
use dbx_core::persistence::task_history::TaskLifecycleOwner;
use dbx_core::plugin_transfer::gate::{self, GateError, TransferGrant, TransferOperation};
use dbx_core::transfer::TransferRequest;

/// Error shape the bridge converts into `E-TRANSFER-*` rejections.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginTransferError {
    pub code: String,
    pub message: String,
}

impl From<GateError> for PluginTransferError {
    fn from(error: GateError) -> Self {
        Self { code: error.code, message: error.message }
    }
}

impl PluginTransferError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_string(), message: message.into() }
    }
}

/// `runId -> pluginId`. In memory on purpose: a run only lives in this process, and a
/// restart cancels everything anyway.
fn run_owners() -> &'static Mutex<HashMap<String, String>> {
    static OWNERS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    OWNERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_run(run_id: &str, plugin_id: &str) {
    if let Ok(mut owners) = run_owners().lock() {
        owners.insert(run_id.to_string(), plugin_id.to_string());
    }
}

fn forget_run(run_id: &str) {
    if let Ok(mut owners) = run_owners().lock() {
        owners.remove(run_id);
    }
}

/// Per plugin in-flight count for the concurrency gate (docs/07: max 3).
fn runs_in_flight(plugin_id: &str) -> usize {
    run_owners().lock().map(|owners| owners.values().filter(|owner| owner.as_str() == plugin_id).count()).unwrap_or(0)
}

fn gate_request(request: &Value) -> Result<gate::TransferGateRequest, PluginTransferError> {
    gate::TransferGateRequest::from_value(request).map_err(PluginTransferError::from)
}

fn parse_request(request: &Value) -> Result<TransferRequest, PluginTransferError> {
    serde_json::from_value(request.clone())
        .map_err(|error| GateError::invalid_request(format!("request does not match TransferRequest: {error}")).into())
}

/// Read-only targets are refused before anything is shown or run.
async fn ensure_writable_target(state: &Arc<AppState>, target_connection_id: &str) -> Result<(), PluginTransferError> {
    match ensure_connection_writable(state, target_connection_id, "Transfer").await {
        Ok(()) => Ok(()),
        Err(message) => Err(PluginTransferError::new("E-TRANSFER-READONLY-TARGET", message)),
    }
}

async fn grants_for(state: &Arc<AppState>, plugin_id: &str) -> Result<Vec<TransferGrant>, PluginTransferError> {
    let stored = state
        .storage
        .load_plugin_transfer_grants(plugin_id)
        .await
        .map_err(|message| PluginTransferError::new("E-TRANSFER-NOT-AUTHORIZED", message))?;
    Ok(stored
        .into_iter()
        .map(|(connection_id, operations)| TransferGrant {
            plugin_id: plugin_id.to_string(),
            connection_id,
            operations: operations.iter().filter_map(|operation| TransferOperation::parse(operation)).collect(),
        })
        .collect())
}

/// Resolve what a request would do, with the risk the host sees. No writes, no pool
/// creation: this is what the modal renders and what `plugin_transfer_run` re-derives.
#[tauri::command]
pub async fn plugin_transfer_preview(
    state: State<'_, Arc<AppState>>,
    plugin_id: String,
    request: Value,
) -> Result<dbx_core::plugin_transfer::TransferPreview, PluginTransferError> {
    let state = state.inner().clone();
    let gate_request = gate_request(&request)?;
    gate::check_not_preconfirmed(&gate_request)?;
    gate::check_same_endpoint(&gate_request)?;
    let parsed = parse_request(&request)?;
    // A dry run only reads the target, so a read-only connection is a valid target
    // for it. The write path below keeps the veto.
    if !gate_request.dry_run {
        ensure_writable_target(&state, &parsed.target_connection_id).await?;
    }
    dbx_core::transfer::validate_transfer_request(&parsed)
        .map_err(|message| PluginTransferError::new("E-TRANSFER-INVALID-REQUEST", message))?;
    let _ = plugin_id;
    Ok(dbx_core::plugin_transfer::preview(&state, &parsed, &request).await)
}

/// Run a transfer the user already approved in the app shell modal.
///
/// `request_hash` and `approved` come from the modal, but the host re-derives the hash
/// from `request`, so an approval for one request cannot be replayed for another.
#[tauri::command]
pub async fn plugin_transfer_run(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    plugin_id: String,
    request: Value,
    request_hash: String,
    approved: bool,
) -> Result<String, PluginTransferError> {
    let state = state.inner().clone();
    let gate_request = gate_request(&request)?;
    gate::check_not_preconfirmed(&gate_request)?;
    gate::check_same_endpoint(&gate_request)?;
    // Plan-only run: the runner stops before its first write, `launch_transfer` skips the
    // read-only veto / drop confirmation / journal, and the gate asked for no grant.
    let dry_run = gate_request.dry_run;

    if gate::request_hash(&request) != request_hash {
        return Err(GateError::stale().into());
    }
    if !approved {
        return Err(GateError::rejected().into());
    }

    let mut parsed = parse_request(&request)?;
    if !dry_run {
        ensure_writable_target(&state, &parsed.target_connection_id).await?;
    }
    if parsed.transfer_id.trim().is_empty() {
        return Err(GateError::invalid_request("transferId is required").into());
    }
    let run_id = parsed.transfer_id.clone();

    gate::check_concurrency(runs_in_flight(&plugin_id))?;
    let grants = grants_for(&state, &plugin_id).await?;
    // A dry run needs no grant: it writes nothing, so the needed set is empty and the
    // lookup is skipped (a real run still needs a grant covering every operation).
    gate::check_grant_or_dry_run(&grants, &parsed.target_connection_id, &gate_request)?;

    // The modal's approval *is* the drop-target confirmation (docs/07 §13.3), and the
    // plugin may not set that bit itself — hence the overwrite after the checks above.
    parsed.drop_target_confirmed = true;

    remember_run(&run_id, &plugin_id);
    let launched = crate::commands::transfer::launch_transfer(
        &app,
        state,
        parsed,
        TaskLifecycleOwner::Plugin(plugin_id.clone()),
        None,
        dry_run,
    )
    .await;
    if let Err(message) = launched {
        forget_run(&run_id);
        return Err(PluginTransferError::new("E-TRANSFER-REJECTED", message));
    }
    Ok(run_id)
}

/// Cancel one of *this* plugin's runs. A plugin cannot cancel an app run, nor another
/// plugin's run.
#[tauri::command]
pub async fn plugin_transfer_cancel(plugin_id: String, run_id: String) -> Result<(), PluginTransferError> {
    let owner = run_owners().lock().ok().and_then(|owners| owners.get(&run_id).cloned());
    match owner {
        Some(owner) if owner == plugin_id => {}
        Some(_) | None => return Err(GateError::not_authorized().into()),
    }
    dbx_core::transfer::set_cancelled(&run_id).await;
    forget_run(&run_id);
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferGrantView {
    pub connection_id: String,
    pub operations: Vec<String>,
}

#[tauri::command]
pub async fn plugin_transfer_get_grants(
    state: State<'_, Arc<AppState>>,
    plugin_id: String,
) -> Result<Vec<TransferGrantView>, PluginTransferError> {
    let state = state.inner().clone();
    let stored = state
        .storage
        .load_plugin_transfer_grants(&plugin_id)
        .await
        .map_err(|message| PluginTransferError::new("E-TRANSFER-NOT-AUTHORIZED", message))?;
    Ok(stored.into_iter().map(|(connection_id, operations)| TransferGrantView { connection_id, operations }).collect())
}

/// Write a grant. Only the app shell calls this, after the user answered "always allow"
/// in the same modal.
#[tauri::command]
pub async fn plugin_transfer_set_grant(
    state: State<'_, Arc<AppState>>,
    plugin_id: String,
    connection_id: String,
    operations: Vec<String>,
    granted: bool,
) -> Result<Vec<TransferGrantView>, PluginTransferError> {
    let state = state.inner().clone();
    let mut normalized: BTreeSet<String> = BTreeSet::new();
    for operation in &operations {
        match TransferOperation::parse(operation) {
            Some(operation) => {
                normalized.insert(operation.as_str().to_string());
            }
            None => {
                return Err(GateError::invalid_request(format!("unknown transfer operation: {operation}")).into());
            }
        }
    }
    let stored = state
        .storage
        .set_plugin_transfer_grant(&plugin_id, &connection_id, normalized.into_iter().collect(), granted)
        .await
        .map_err(|message| PluginTransferError::new("E-TRANSFER-NOT-AUTHORIZED", message))?;
    Ok(stored.into_iter().map(|(connection_id, operations)| TransferGrantView { connection_id, operations }).collect())
}
