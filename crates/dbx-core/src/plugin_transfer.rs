//! Assembly for the plugin-initiated transfer API (`host.transfer:run`).
//!
//! The rules live in `dbx_plugin_runtime::transfer::gate` as pure functions. This
//! module only gathers the *host owned* facts those rules need - who the endpoints
//! are, whether the target counts as production, and a stable request hash - so the
//! confirm dialog can show what the host resolved instead of trusting the plugin.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::connection::AppState;
use crate::models::connection::DatabaseType;
use crate::transfer::TransferRequest;

pub use dbx_plugin_runtime::transfer as gate;

/// One side of a transfer, resolved by the host (docs/07 §4 `EndpointCard`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointCard {
    pub connection_id: String,
    pub name: String,
    pub db_type: String,
    pub database: String,
    pub schema: String,
    /// Read-only targets never reach the dialog: the command layer refuses them with
    /// `E-CONN-READONLY` before preview, so this is always false for a preview that
    /// the user can see.
    pub read_only: bool,
    pub production: bool,
}

/// What the request actually resolves to (docs/07 §4 `TransferPreview.resolved`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedTransfer {
    pub source: EndpointCard,
    pub target: EndpointCard,
    pub tables: Vec<String>,
    pub mode: String,
    pub content: String,
    pub creates_tables: bool,
    pub drops_tables: bool,
    pub ownership_policy: String,
    pub filters: Vec<gate::TableFilterView>,
    pub estimated_rows: Option<u64>,
}

/// Full preview returned to the bridge and rendered by the confirm dialog.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferPreview {
    pub risk: gate::Risk,
    pub resolved: ResolvedTransfer,
    pub request_hash: String,
    /// Operations the plugin's grant must cover (docs/07 §9).
    pub operations: Vec<String>,
}

/// serde representation of an enum as the string the UI and the hash use, without
/// depending on a `Display` impl for every transfer enum.
fn enum_label<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value).ok().and_then(|value| value.as_str().map(str::to_string)).unwrap_or_default()
}

/// Resolve one endpoint. An unknown connection fails closed: it is reported as
/// production so the dialog escalates instead of quietly downgrading.
pub async fn endpoint_card(state: &AppState, connection_id: &str, database: &str, schema: &str) -> EndpointCard {
    let identity = {
        let configs = state.configs.read().await;
        configs.get(connection_id).map(|config| (config.name.clone(), config.db_type))
    };
    let (name, db_type, known) = match identity {
        Some((name, db_type)) => (name, db_type, true),
        // Unknown target: keep a placeholder name but never claim it is safe.
        None => (connection_id.to_string(), DatabaseType::Mysql, false),
    };
    let production = if known {
        crate::transfer_rebuild::classify_drop_target(state, connection_id, database, db_type).await.production
    } else {
        true
    };
    EndpointCard {
        connection_id: connection_id.to_string(),
        name,
        db_type: db_type.as_str().to_string(),
        database: database.to_string(),
        schema: schema.to_string(),
        read_only: false,
        production,
    }
}

/// Build the host side preview. Pure with respect to the request: it reads
/// connection metadata and production flags, and never writes anything.
pub async fn preview(state: &AppState, request: &TransferRequest, raw: &Value) -> TransferPreview {
    let gate_request: gate::TransferGateRequest = serde_json::from_value(raw.clone()).unwrap_or_default();
    // Same endpoint means "same connection + same database + same schema"; the gate owns
    // that definition so preview and run can never disagree about it.
    let same_endpoint = gate::check_same_endpoint(&gate_request).is_err();
    let source = endpoint_card(state, &request.source_connection_id, &request.source_database, &request.source_schema).await;
    let target = endpoint_card(state, &request.target_connection_id, &request.target_database, &request.target_schema).await;

    let mode = enum_label(&request.mode);
    let content = enum_label(&request.content);
    let ownership_policy = enum_label(&request.ownership_policy);

    let risk_input = gate::RiskInput {
        table_count: request.tables.len(),
        create_table: request.create_table,
        drop_target_before_create: request.drop_target_before_create,
        mode_overwrite_or_upsert: matches!(mode.as_str(), "overwrite" | "upsert"),
        ownership_policy_not_preserve: ownership_policy != "preserve",
        non_table_object_count: request.objects.as_ref().map(|objects| objects.len()).unwrap_or(0),
        same_connection_other_table: request.source_connection_id == request.target_connection_id
            && !same_endpoint,
        target_production: target.production,
        target_readonly: false,
        estimated_rows: gate_request.estimated_rows,
    };

    let mut filters: Vec<gate::TableFilterView> = request
        .table_filters
        .iter()
        .filter(|(_, sql)| !sql.trim().is_empty())
        .map(|(table, sql)| gate::TableFilterView { table: table.clone(), sql: sql.clone() })
        .collect();
    filters.sort_by(|left, right| left.table.cmp(&right.table));

    TransferPreview {
        risk: gate::classify_risk(&risk_input),
        resolved: ResolvedTransfer {
            source,
            target,
            tables: request.tables.clone(),
            mode,
            content,
            creates_tables: request.create_table,
            drops_tables: request.drop_target_before_create,
            ownership_policy,
            filters,
            estimated_rows: gate_request.estimated_rows,
        },
        request_hash: gate::request_hash(raw),
        operations: gate::operations_for_request(&gate_request).into_iter().map(|operation| operation.as_str().to_string()).collect(),
    }
}
