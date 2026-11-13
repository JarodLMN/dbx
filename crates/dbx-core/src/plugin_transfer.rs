//! Assembly for the plugin-initiated transfer API (`host.transfer:run`).
//!
//! The rules live in `dbx_plugin_runtime::transfer::gate` as pure functions. This
//! module only gathers the *host owned* facts those rules need - who the endpoints
//! are, whether the target counts as production, and a stable request hash - so the
//! confirm dialog can show what the host resolved instead of trusting the plugin.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

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
    /// Operations the plugin's grant must cover (docs/07 §9). Empty for a dry run.
    pub operations: Vec<String>,
    /// Present only for a dry run: the statements the write pass would execute, plus the
    /// rebuild plan when one was asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<DryRunProducts>,
}

/// What a dry run would do, rendered by the very generators the write pass uses
/// (docs/07 §13). Planning only: nothing here was executed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DryRunProducts {
    /// `CREATE SCHEMA` / `CREATE TABLE` / index / foreign key / comment statements.
    pub structure_sql: String,
    pub operations: Vec<PlannedStatement>,
    pub schema_objects_expanded: bool,
    /// The rename / create / cleanup plan of `dropTargetBeforeCreate`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rebuild_sql: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_sql: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup_sql: Option<String>,
    /// Roles the target lacks, so ownership DDL cannot be applied by the real run.
    pub missing_owners: Vec<String>,
    pub target_owner: String,
}

/// One planned operation, flattened for the plugin: the planner's enum becomes its serde
/// label so the payload does not change shape when the planner grows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedStatement {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_table: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_table: Option<String>,
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
    let source =
        endpoint_card(state, &request.source_connection_id, &request.source_database, &request.source_schema).await;
    let target =
        endpoint_card(state, &request.target_connection_id, &request.target_database, &request.target_schema).await;

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
        same_connection_other_table: request.source_connection_id == request.target_connection_id && !same_endpoint,
        target_production: target.production,
        target_readonly: false,
        dry_run: gate_request.dry_run,
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
        operations: gate::operations_for_request(&gate_request)
            .into_iter()
            .map(|operation| operation.as_str().to_string())
            .collect(),
        dry_run: None,
    }
}

async fn db_type_of(state: &AppState, connection_id: &str) -> Result<DatabaseType, String> {
    let configs = state.configs.read().await;
    configs.get(connection_id).map(|config| config.db_type).ok_or_else(|| format!("unknown connection {connection_id}"))
}

/// Render a dry run's products. Reads metadata and opens transfer pools, but every
/// statement it returns comes from the planner the write pass shares - nothing is
/// executed here, and the real run re-inspects the target before writing.
pub async fn dry_run_products(state: &Arc<AppState>, request: &TransferRequest) -> Result<DryRunProducts, String> {
    let source_db_type = db_type_of(state, &request.source_connection_id).await?;
    let target_db_type = db_type_of(state, &request.target_connection_id).await?;
    // A dry run only reads, so the target pool is opened read-only-ish: no DDL is sent
    // through it. `ensure_transfer_pool` is cached per (connection, database).
    let source_pool_key = crate::transfer::ensure_transfer_pool(
        state,
        &request.source_connection_id,
        &request.source_database,
        request.source_catalog.as_deref(),
    )
    .await?;
    let target_pool_key = crate::transfer::ensure_transfer_pool(
        state,
        &request.target_connection_id,
        &request.target_database,
        request.target_catalog.as_deref(),
    )
    .await?;

    // Ownership already renders the structure plan for structure-only and package
    // requests, and the rebuild plan when asked for; every other content mode renders
    // its DDL here, so a dry run always shows what the create pass would run.
    let ownership = crate::transfer::preview_transfer_ownership(
        state,
        request,
        &source_db_type,
        &target_db_type,
        &source_pool_key,
        &target_pool_key,
    )
    .await?;
    let structure = match ownership.structure {
        Some(structure) => structure,
        None => {
            crate::transfer::preview_transfer_structure(
                state,
                request,
                &source_db_type,
                &target_db_type,
                &source_pool_key,
                &target_pool_key,
            )
            .await?
        }
    };
    let (rebuild_sql, backup_sql, cleanup_sql) = match ownership.rebuild {
        Some(plan) => (Some(plan.sql), plan.backup_sql, plan.cleanup_sql),
        None => (None, None, None),
    };

    Ok(DryRunProducts {
        structure_sql: structure.sql,
        operations: structure
            .operations
            .iter()
            .map(|operation| PlannedStatement {
                kind: enum_label(&operation.kind),
                object_name: operation.object_name.clone(),
                source_table: operation.source_table.clone(),
                target_table: operation.target_table.clone(),
            })
            .collect(),
        schema_objects_expanded: structure.schema_objects_expanded,
        rebuild_sql,
        backup_sql,
        cleanup_sql,
        missing_owners: ownership.missing_owners,
        target_owner: ownership.target_owner,
    })
}
