//! Plugin facing transfer commands (`host.transfer:run`, docs/07 §4/§9/§10).
//!
//! Everything here is called by the host bridge, never by the plugin directly: the
//! `pluginId` argument is injected by the bridge, and the confirm dialog lives in the
//! app shell (an in-app modal). A plugin can therefore *ask* for a transfer and
//! *observe* its own run, but it cannot approve one, and it cannot touch another
//! plugin's run.
//!
//! Two of these commands are deliberately **not** behind the runtime switch (docs/07
//! §17.1 L2): `plugin_transfer_cancel` and the grant pair. Turning the switch off must
//! not strand an in-flight run, and revoking consent has to stay possible while the
//! write path is closed — a closed switch is when a careful user revokes.

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

/// Whether plugin-initiated writes are allowed at all (docs/07 §17.1).
///
/// Two bits, not one: a build without the `plugin-transfer` feature stays closed even
/// when the stored setting says "on", because that value can outlive the build that
/// wrote it (open a nightly, close the switch, ship the stock build). The check is a
/// no-op guard on the read path only — it never interrupts a run already launched.
async fn ensure_enabled(state: &Arc<AppState>) -> Result<(), PluginTransferError> {
    if !cfg!(feature = "plugin-transfer") {
        return Err(PluginTransferError::new(
            "E-TRANSFER-DISABLED",
            "This DBX build does not include plugin initiated writes",
        ));
    }
    match state.storage.load_plugin_transfer_enabled().await {
        Ok(true) => Ok(()),
        // Fail closed on a storage error too: "could not read the switch" is not "on".
        Ok(false) => Err(PluginTransferError::new(
            "E-TRANSFER-DISABLED",
            "Plugin initiated writes are turned off in Settings \u{203a} Plugins",
        )),
        Err(message) => Err(PluginTransferError::new("E-TRANSFER-DISABLED", message)),
    }
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

/// "仅本次允许": the operations the user ticked in the grant dialog for this one call.
///
/// Unknown names are rejected instead of dropped: a typo must not quietly shrink an
/// approval the user believes is complete.
fn parse_session_grant(operations: Option<Vec<String>>) -> Result<BTreeSet<TransferOperation>, PluginTransferError> {
    let mut session = BTreeSet::new();
    for operation in operations.unwrap_or_default() {
        let parsed = TransferOperation::parse(&operation)
            .ok_or_else(|| GateError::invalid_request(format!("unknown transfer operation: {operation}")))?;
        session.insert(parsed);
    }
    Ok(session)
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
    ensure_enabled(&state).await?;
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
    let mut preview = dbx_core::plugin_transfer::preview(&state, &parsed, &request).await;
    if gate_request.dry_run {
        // Opening pools and reading metadata is the only work a dry run does here. A
        // failure to render the plan is a dry-run failure, not a request rejection.
        preview.dry_run = Some(
            dbx_core::plugin_transfer::dry_run_products(&state, &parsed)
                .await
                .map_err(|message| PluginTransferError::new("E-TRANSFER-DRYRUN-FAILED", message))?,
        );
    }
    Ok(preview)
}

/// Run a transfer the user already approved in the app shell modal.
///
/// `request_hash` and `approved` come from the modal, but the host re-derives the hash
/// from `request`, so an approval for one request cannot be replayed for another.
///
/// `session_grant` carries "仅本次允许": the operations the user ticked in the grant
/// dialog for this call alone. It is never persisted — `plugin_transfer_set_grant`
/// stays the only writer of a standing grant (docs/07 §9).
#[tauri::command]
pub async fn plugin_transfer_run(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    plugin_id: String,
    request: Value,
    request_hash: String,
    session_grant: Option<Vec<String>>,
    approved: bool,
) -> Result<PluginTransferRun, PluginTransferError> {
    let state = state.inner().clone();
    ensure_enabled(&state).await?;
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
    let session = parse_session_grant(session_grant)?;
    // A dry run needs neither a grant nor an approval: it writes nothing, so the
    // needed set is empty. A real run needs a standing grant or this one call's
    // approval covering every operation it would perform.
    gate::check_grant_or_session(&grants, &parsed.target_connection_id, &gate_request, &session)?;

    // The modal's approval *is* the drop-target confirmation (docs/07 §13.3), and the
    // plugin may not set that bit itself — hence the overwrite after the checks above.
    parsed.drop_target_confirmed = true;

    // A dry run returns its plan: the plugin must not have to rebuild it from the
    // preview, and nothing below may write.
    let products = if dry_run {
        Some(
            dbx_core::plugin_transfer::dry_run_products(&state, &parsed)
                .await
                .map_err(|message| PluginTransferError::new("E-TRANSFER-DRYRUN-FAILED", message))?,
        )
    } else {
        None
    };

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
    Ok(PluginTransferRun { run_id, dry_run: products })
}

/// What `plugin_transfer_run` hands back. A real run only carries the id the plugin
/// uses for progress and cancel; a dry run also carries the plan it resolved.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginTransferRun {
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<dbx_core::plugin_transfer::DryRunProducts>,
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
    /// Connection name at read time, so the revoke list can name a grant without a
    /// second lookup; `None` means the connection is gone and only its id is left.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_name: Option<String>,
    pub connection_id: String,
    pub operations: Vec<String>,
}

/// Stored grants with names resolved, for the plugin details page (docs/07 §9).
async fn grant_views(state: &Arc<AppState>, plugin_id: &str) -> Result<Vec<TransferGrantView>, PluginTransferError> {
    let stored = state
        .storage
        .load_plugin_transfer_grants(plugin_id)
        .await
        .map_err(|message| PluginTransferError::new("E-TRANSFER-NOT-AUTHORIZED", message))?;
    let configs = state.configs.read().await;
    Ok(stored
        .into_iter()
        .map(|(connection_id, operations)| TransferGrantView {
            connection_name: configs.get(&connection_id).map(|config| config.name.clone()),
            connection_id,
            operations,
        })
        .collect())
}

#[tauri::command]
pub async fn plugin_transfer_get_grants(
    state: State<'_, Arc<AppState>>,
    plugin_id: String,
) -> Result<Vec<TransferGrantView>, PluginTransferError> {
    let state = state.inner().clone();
    grant_views(&state, &plugin_id).await
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
    state
        .storage
        .set_plugin_transfer_grant(&plugin_id, &connection_id, normalized.into_iter().collect(), granted)
        .await
        .map_err(|message| PluginTransferError::new("E-TRANSFER-NOT-AUTHORIZED", message))?;
    grant_views(&state, &plugin_id).await
}

/// The two bits behind `capabilities.transferApi` (docs/07 §17.1/§17.3).
///
/// `featureEnabled` is the L1 compile flag, `enabled` the L2 user switch. The app shell
/// needs both to decide whether to offer the switch at all: a build without the feature
/// must not show a setting that can only be ineffective (§17.1).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginTransferSettings {
    pub enabled: bool,
    pub feature_enabled: bool,
}

impl PluginTransferSettings {
    fn current(enabled: bool) -> Self {
        Self { enabled, feature_enabled: cfg!(feature = "plugin-transfer") }
    }
}

/// Read the switch. Always registered, even in a build without the feature: this is how
/// the app shell learns that the feature is missing instead of guessing from a version.
#[tauri::command]
pub async fn plugin_transfer_settings(
    state: State<'_, Arc<AppState>>,
) -> Result<PluginTransferSettings, PluginTransferError> {
    let state = state.inner().clone();
    let enabled = state
        .storage
        .load_plugin_transfer_enabled()
        .await
        .map_err(|message| PluginTransferError::new("E-TRANSFER-DISABLED", message))?;
    Ok(PluginTransferSettings::current(enabled))
}

/// Flip the switch. Turning it on in a build without the feature is refused rather than
/// stored, so the value always means what it says (docs/07 §17.1 L2).
#[tauri::command]
pub async fn plugin_transfer_set_enabled(
    state: State<'_, Arc<AppState>>,
    enabled: bool,
) -> Result<PluginTransferSettings, PluginTransferError> {
    let state = state.inner().clone();
    let settings = PluginTransferSettings::current(enabled);
    if enabled && !settings.feature_enabled {
        return Err(PluginTransferError::new(
            "E-TRANSFER-DISABLED",
            "This DBX build does not include plugin initiated writes",
        ));
    }
    state
        .storage
        .save_plugin_transfer_enabled(enabled)
        .await
        .map_err(|message| PluginTransferError::new("E-TRANSFER-DISABLED", message))?;
    Ok(settings)
}
