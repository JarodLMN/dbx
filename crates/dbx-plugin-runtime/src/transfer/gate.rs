//! Pure gate for `host.transfer:run` (docs/07 §3, §6, §7, §9, §10).
//!
//! Everything here is a function over plain data. No database handle, no Tauri
//! state, no plugin frame: the command layer owns all of that and passes plain
//! values in. Rules live here so they can be unit tested and so the preview and
//! the confirm step cannot drift apart.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Manifest permission that unlocks the Host API.
pub const TRANSFER_PERMISSION: &str = "host.transfer:run";
/// How many runs one plugin may have in flight (docs/07 §7).
pub const MAX_PLUGIN_TRANSFER_RUNS: usize = 3;
/// Confirmation dialog lifetime; expiry denies, it never auto-approves.
pub const CONFIRM_TIMEOUT_SECS: u64 = 120;
/// A plugin side estimate at or above this row count is L2 (docs/07 §6).
const LARGE_ESTIMATE_ROWS: u64 = 1_000_000;
/// Table count that turns a default write into L1 / L2 (docs/07 §6).
const MANY_TABLES_L1: usize = 4;
const MANY_TABLES_L2: usize = 20;

/// One write operation a grant can cover (docs/07 §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferOperation {
    Insert,
    Truncate,
    Overwrite,
    CreateTable,
    Drop,
}

impl TransferOperation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Truncate => "truncate",
            Self::Overwrite => "overwrite",
            Self::CreateTable => "createTable",
            Self::Drop => "drop",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "insert" => Some(Self::Insert),
            "truncate" => Some(Self::Truncate),
            "overwrite" => Some(Self::Overwrite),
            "createTable" | "create_table" => Some(Self::CreateTable),
            "drop" => Some(Self::Drop),
            _ => None,
        }
    }

    /// `drop` and `truncate` are destructive: a grant prompt never pre-checks
    /// them (docs/07 §9).
    pub const fn is_destructive(self) -> bool {
        matches!(self, Self::Truncate | Self::Drop)
    }
}

/// A stored grant for `(pluginId, connectionId, operations[])`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferGrant {
    pub plugin_id: String,
    pub connection_id: String,
    #[serde(default)]
    pub operations: Vec<TransferOperation>,
}

impl TransferGrant {
    pub fn covers(&self, operations: &BTreeSet<TransferOperation>) -> bool {
        operations.iter().all(|needed| self.operations.contains(needed))
    }
}

/// Risk levels; the numbers are what the confirm dialog switches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    L0,
    L1,
    L2,
}

impl RiskLevel {
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::L0 => 0,
            Self::L1 => 1,
            Self::L2 => 2,
        }
    }
}

/// Why the host raised the level. Codes are stable and shipped to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RiskReasonCode {
    ProductionTarget,
    DropAndRecreate,
    OverwriteMode,
    ManyTables,
    LargeEstimate,
    NoEstimate,
    NonTableObjects,
    SameConnection,
    OwnershipReassign,
    TargetReadonly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskReason {
    pub code: RiskReasonCode,
    pub detail: String,
    pub level: RiskLevel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Risk {
    pub level: RiskLevel,
    pub reasons: Vec<RiskReason>,
    /// True when this risk belongs to a dry run: the dialog shows *what would
    /// happen*, but nothing is written, so no write risk is raised.
    pub dry_run: bool,
}

/// Host-owned facts the plugin cannot influence (docs/07 §6).
#[derive(Debug, Clone, Default)]
pub struct RiskInput {
    pub table_count: usize,
    pub create_table: bool,
    pub drop_target_before_create: bool,
    pub mode_overwrite_or_upsert: bool,
    pub ownership_policy_not_preserve: bool,
    pub non_table_object_count: usize,
    pub same_connection_other_table: bool,
    pub target_production: bool,
    pub target_readonly: bool,
    /// Plan and measure only (docs/07 §13): no table is created, no row is
    /// written, nothing is renamed or dropped.
    pub dry_run: bool,
    /// `None` means the plugin supplied no estimate, which is an L2 signal.
    pub estimated_rows: Option<u64>,
}

/// Classify a request (docs/07 §6). The level is the maximum of every hit.
pub fn classify_risk(input: &RiskInput) -> Risk {
    // A dry run writes nothing, so every rule below (which describes a *write*) is
    // moot. Level L0 is not a downgrade for convenience: it is what the user is
    // being asked to approve - reading a target and producing a plan.
    if input.dry_run {
        return Risk { level: RiskLevel::L0, reasons: Vec::new(), dry_run: true };
    }

    let mut reasons: Vec<RiskReason> = Vec::new();
    let mut level = RiskLevel::L0;

    let mut raise = |to: RiskLevel, code: RiskReasonCode, detail: String, reasons: &mut Vec<RiskReason>| {
        if to > level {
            level = to;
        }
        reasons.push(RiskReason { code, detail, level: to });
    };

    if input.target_readonly {
        // Not a level: the request is rejected before the dialog is shown.
        raise(RiskLevel::L2, RiskReasonCode::TargetReadonly, "target connection is read-only".to_string(), &mut reasons);
    }
    if input.target_production {
        raise(
            RiskLevel::L2,
            RiskReasonCode::ProductionTarget,
            "target is marked as production".to_string(),
            &mut reasons,
        );
    }
    if input.drop_target_before_create {
        raise(
            RiskLevel::L2,
            RiskReasonCode::DropAndRecreate,
            "target tables are renamed to a backup and rebuilt".to_string(),
            &mut reasons,
        );
    }
    if input.table_count >= MANY_TABLES_L2 {
        raise(
            RiskLevel::L2,
            RiskReasonCode::ManyTables,
            format!("{} tables in one write", input.table_count),
            &mut reasons,
        );
    }
    match input.estimated_rows {
        Some(rows) if rows >= LARGE_ESTIMATE_ROWS => raise(
            RiskLevel::L2,
            RiskReasonCode::LargeEstimate,
            format!("plugin estimate is {rows} rows"),
            &mut reasons,
        ),
        None => raise(
            RiskLevel::L2,
            RiskReasonCode::NoEstimate,
            "plugin supplied no row estimate".to_string(),
            &mut reasons,
        ),
        _ => {}
    }

    if input.table_count >= MANY_TABLES_L1 {
        raise(
            RiskLevel::L1,
            RiskReasonCode::ManyTables,
            format!("{} tables in one write", input.table_count),
            &mut reasons,
        );
    }
    if input.create_table {
        raise(
            RiskLevel::L1,
            RiskReasonCode::DropAndRecreate,
            "missing target tables are created".to_string(),
            &mut reasons,
        );
    }
    if input.mode_overwrite_or_upsert {
        raise(
            RiskLevel::L1,
            RiskReasonCode::OverwriteMode,
            "write mode overwrites existing rows".to_string(),
            &mut reasons,
        );
    }
    if input.ownership_policy_not_preserve {
        raise(
            RiskLevel::L1,
            RiskReasonCode::OwnershipReassign,
            "ownership policy rewrites object owners".to_string(),
            &mut reasons,
        );
    }
    if input.non_table_object_count > 0 {
        raise(
            RiskLevel::L1,
            RiskReasonCode::NonTableObjects,
            format!("{} non-table objects in scope", input.non_table_object_count),
            &mut reasons,
        );
    }
    if input.same_connection_other_table {
        raise(
            RiskLevel::L1,
            RiskReasonCode::SameConnection,
            "source and target are the same connection".to_string(),
            &mut reasons,
        );
    }

    Risk { level, reasons, dry_run: false }
}

/// The subset of `TransferRequest` the gate needs. Every field is optional with a
/// conservative default so a malformed request fails closed instead of panicking.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferGateRequest {
    #[serde(default)]
    pub transfer_id: Option<String>,
    #[serde(default)]
    pub source_connection_id: String,
    #[serde(default)]
    pub source_database: String,
    #[serde(default)]
    pub source_schema: String,
    #[serde(default)]
    pub target_connection_id: String,
    #[serde(default)]
    pub target_database: String,
    #[serde(default)]
    pub target_schema: String,
    #[serde(default)]
    pub tables: Vec<String>,
    #[serde(default)]
    pub create_table: bool,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub ownership_policy: Option<String>,
    #[serde(default)]
    pub objects: Option<Vec<String>>,
    #[serde(default)]
    pub table_filters: BTreeMap<String, String>,
    #[serde(default)]
    pub drop_target_before_create: bool,
    /// Must stay false: the host fills it after the user confirms (docs/07 §1).
    #[serde(default)]
    pub drop_target_confirmed: bool,
    /// Plan and measure only. Part of the request hash on purpose: an approval
    /// given for a dry run must never be replayable as a real write.
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub estimated_rows: Option<u64>,
    #[serde(default)]
    pub estimated_duration_secs: Option<u64>,
}

impl TransferGateRequest {
    /// Parse the plugin supplied JSON. Unknown fields are ignored; the core
    /// deserializes the same payload into its own stricter `TransferRequest`.
    pub fn from_value(value: &Value) -> Result<Self, GateError> {
        serde_json::from_value(value.clone()).map_err(|error| GateError::invalid_request(error.to_string()))
    }

    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    pub fn same_connection(&self) -> bool {
        !self.source_connection_id.is_empty() && self.source_connection_id == self.target_connection_id
    }

    pub fn same_database(&self) -> bool {
        norm(&self.source_database) == norm(&self.target_database)
            && norm(&self.source_schema) == norm(&self.target_schema)
    }

    /// True only when source and target are the same connection, database, schema
    /// *and* table set - the one combination that is always meaningless.
    pub fn same_endpoint(&self) -> bool {
        self.same_connection() && self.same_database() && {
            let mut source = self.tables.clone();
            source.sort();
            source.dedup();
            !source.is_empty()
        }
    }
}

fn norm(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

/// `TransferPreview.risk` reasons plus the operations the grant must cover.
pub fn operations_for_request(request: &TransferGateRequest) -> BTreeSet<TransferOperation> {
    // Nothing is written, so no grant can be needed; callers still must not treat
    // an empty set as "any operation is allowed" (see check_grant_or_dry_run).
    if request.dry_run {
        return BTreeSet::new();
    }
    let mut operations = BTreeSet::new();
    operations.insert(TransferOperation::Insert);
    let mode = request.mode.as_deref().unwrap_or_default();
    if matches!(mode, "overwrite" | "upsert") {
        operations.insert(TransferOperation::Overwrite);
        operations.insert(TransferOperation::Truncate);
    }
    if request.create_table {
        operations.insert(TransferOperation::CreateTable);
    }
    if request.drop_target_before_create {
        operations.insert(TransferOperation::Drop);
    }
    operations
}

/// Errors carry the `E-TRANSFER-*` code from docs/07 §10 so the bridge can hand
/// the code to the plugin unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateError {
    pub code: String,
    pub message: String,
}

impl GateError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_string(), message: message.into() }
    }

    pub fn not_authorized() -> Self {
        Self::new("E-TRANSFER-NOT-AUTHORIZED", "the plugin has no grant for this connection")
    }

    pub fn rejected() -> Self {
        Self::new("E-TRANSFER-REJECTED", "the user cancelled the write")
    }

    pub fn expired() -> Self {
        Self::new("E-TRANSFER-EXPIRED", "the confirmation expired without a decision")
    }

    pub fn stale() -> Self {
        Self::new("E-TRANSFER-STALE", "the request changed while the confirmation was open")
    }

    pub fn preconfirmed() -> Self {
        Self::new("E-TRANSFER-PRECONFIRMED", "confirmation flags must be filled by the host")
    }

    pub fn same_endpoint() -> Self {
        Self::new("E-TRANSFER-SAME-ENDPOINT", "source and target are the same table")
    }

    pub fn readonly_target() -> Self {
        Self::new("E-CONN-READONLY", "the target connection is read-only")
    }

    pub fn concurrent_limit() -> Self {
        Self::new("E-TRANSFER-CONCURRENT-LIMIT", "too many writes in flight for this plugin")
    }

    pub fn invalid_request(detail: impl Into<String>) -> Self {
        Self::new("E-TRANSFER-INVALID-REQUEST", detail)
    }

    /// The gate understands dry runs (no grant, no write risk) but this host build
    /// cannot execute one yet: the runner would still write. Refusing here is the
    /// interlock that keeps an accepted "dry run" from becoming a real one.
    pub fn dryrun_unsupported() -> Self {
        Self::new("E-TRANSFER-DRYRUN-UNSUPPORTED", "this host build cannot execute a dry run yet")
    }
}

impl std::fmt::Display for GateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for GateError {}

/// Rule 2 of docs/07 §1: the plugin may not pre-fill a confirmation bit.
pub fn check_not_preconfirmed(request: &TransferGateRequest) -> Result<(), GateError> {
    if request.drop_target_confirmed {
        return Err(GateError::preconfirmed());
    }
    Ok(())
}

/// Same table on both ends: meaningless, refuse before showing a dialog.
pub fn check_same_endpoint(request: &TransferGateRequest) -> Result<(), GateError> {
    if request.same_endpoint() {
        return Err(GateError::same_endpoint());
    }
    Ok(())
}

/// Read-only target connections never reach the dialog (docs/07 §5, §6).
pub fn check_readonly_target(target_readonly: bool) -> Result<(), GateError> {
    if target_readonly { Err(GateError::readonly_target()) } else { Ok(()) }
}

/// Grant covers "may we ask"; the dialog still decides "do it this time".
pub fn check_grant(grants: &[TransferGrant], connection_id: &str, needed: &BTreeSet<TransferOperation>) -> Result<(), GateError> {
    grants
        .iter()
        .find(|grant| grant.connection_id == connection_id && grant.covers(needed))
        .map(|_| ())
        .ok_or_else(GateError::not_authorized)
}

/// Dry runs need no grant: they write nothing, so `operations_for_request` is empty
/// and the lookup is skipped. A real request still needs a grant covering every
/// operation, which is the point of keeping the two paths in one function.
pub fn check_grant_or_dry_run(
    grants: &[TransferGrant],
    connection_id: &str,
    request: &TransferGateRequest,
) -> Result<(), GateError> {
    let needed = operations_for_request(request);
    if needed.is_empty() {
        return Ok(());
    }
    check_grant(grants, connection_id, &needed)
}

/// A one-shot approval: the user allowed *this* call, nothing is persisted.
///
/// `session` is what the grant dialog's checkboxes reported. It can only widen the
/// call it was given for — a standing grant stays the only thing that lets a later
/// call skip the prompt, which is what keeps "may we ask" and "do it now" apart
/// (docs/07 §9). An empty needed set (dry run) needs neither.
pub fn check_grant_or_session(
    grants: &[TransferGrant],
    connection_id: &str,
    request: &TransferGateRequest,
    session: &BTreeSet<TransferOperation>,
) -> Result<(), GateError> {
    let needed = operations_for_request(request);
    if needed.is_empty() {
        return Ok(());
    }
    if check_grant(grants, connection_id, &needed).is_ok() || needed.is_subset(session) {
        return Ok(());
    }
    Err(GateError::not_authorized())
}

/// docs/07 §7: at most `MAX_PLUGIN_TRANSFER_RUNS` runs in flight per plugin.
pub fn check_concurrency(in_flight: usize) -> Result<(), GateError> {
    if in_flight >= MAX_PLUGIN_TRANSFER_RUNS { Err(GateError::concurrent_limit()) } else { Ok(()) }
}

/// What the dialog returned, mapped to a gate decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmOutcome {
    Approved,
    Denied,
    Expired,
    Stale,
}

impl ConfirmOutcome {
    pub fn into_result(self) -> Result<(), GateError> {
        match self {
            Self::Approved => Ok(()),
            Self::Denied => Err(GateError::rejected()),
            Self::Expired => Err(GateError::expired()),
            Self::Stale => Err(GateError::stale()),
        }
    }
}

/// Sorted-key JSON with `transferId` and `dropTargetConfirmed` removed, so a
/// preview and the later run agree even though the host fills in both
/// (docs/07 §7).
pub fn canonical_request_json(request: &Value) -> Value {
    match request {
        Value::Object(map) => {
            let mut sorted: BTreeMap<&str, Value> = BTreeMap::new();
            for (key, value) in map {
                if matches!(key.as_str(), "transferId" | "transfer_id" | "dropTargetConfirmed" | "drop_target_confirmed") {
                    continue;
                }
                sorted.insert(key.as_str(), canonical_request_json(value));
            }
            let mut out = Map::new();
            for (key, value) in sorted {
                out.insert(key.to_string(), value);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_request_json).collect()),
        other => other.clone(),
    }
}

/// `sha256(canonicalJson(request))`, hex encoded.
pub fn request_hash(request: &Value) -> String {
    let canonical = canonical_request_json(request);
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A filter row as the confirm dialog shows it: verbatim, never rewritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableFilterView {
    pub table: String,
    pub sql: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_request() -> TransferGateRequest {
        TransferGateRequest {
            source_connection_id: "c1".into(),
            source_database: "shop".into(),
            source_schema: "public".into(),
            target_connection_id: "c2".into(),
            target_database: "shop".into(),
            target_schema: "public".into(),
            tables: vec!["orders".into()],
            content: Some("dataOnly".into()),
            mode: Some("append".into()),
            ownership_policy: Some("preserve".into()),
            estimated_rows: Some(1000),
            ..TransferGateRequest::default()
        }
    }

    #[test]
    fn default_append_is_l0() {
        let risk = classify_risk(&RiskInput { table_count: 1, estimated_rows: Some(1000), ..RiskInput::default() });
        assert_eq!(risk.level, RiskLevel::L0);
        assert!(risk.reasons.is_empty());
    }

    #[test]
    fn missing_estimate_and_production_and_drop_are_l2() {
        let risk = classify_risk(&RiskInput {
            table_count: 1,
            drop_target_before_create: true,
            target_production: true,
            estimated_rows: None,
            ..RiskInput::default()
        });
        assert_eq!(risk.level, RiskLevel::L2);
        let codes: Vec<_> = risk.reasons.iter().map(|reason| reason.code).collect();
        assert!(codes.contains(&RiskReasonCode::ProductionTarget));
        assert!(codes.contains(&RiskReasonCode::DropAndRecreate));
        assert!(codes.contains(&RiskReasonCode::NoEstimate));
    }

    #[test]
    fn table_counts_escalate() {
        let l1 = classify_risk(&RiskInput { table_count: 4, estimated_rows: Some(10), ..RiskInput::default() });
        assert_eq!(l1.level, RiskLevel::L1);
        let l2 = classify_risk(&RiskInput { table_count: 20, estimated_rows: Some(10), ..RiskInput::default() });
        assert_eq!(l2.level, RiskLevel::L2);
    }

    #[test]
    fn plugin_may_not_preconfirm() {
        let mut request = base_request();
        request.drop_target_confirmed = true;
        assert_eq!(check_not_preconfirmed(&request).unwrap_err().code, "E-TRANSFER-PRECONFIRMED");
    }

    #[test]
    fn same_endpoint_is_refused() {
        let mut request = base_request();
        request.target_connection_id = request.source_connection_id.clone();
        assert_eq!(check_same_endpoint(&request).unwrap_err().code, "E-TRANSFER-SAME-ENDPOINT");
    }

    #[test]
    fn same_connection_other_database_is_allowed() {
        let mut request = base_request();
        request.target_connection_id = request.source_connection_id.clone();
        request.target_database = "shop_archive".into();
        assert!(check_same_endpoint(&request).is_ok());
    }

    #[test]
    fn grant_must_cover_every_operation() {
        let mut request = base_request();
        request.drop_target_before_create = true;
        request.create_table = true;
        let needed = operations_for_request(&request);
        assert!(needed.contains(&TransferOperation::Drop));

        let insert_only = TransferGrant {
            plugin_id: "com.dbx.datax".into(),
            connection_id: "c2".into(),
            operations: vec![TransferOperation::Insert, TransferOperation::CreateTable],
        };
        assert_eq!(check_grant(&[insert_only.clone()], "c2", &needed).unwrap_err().code, "E-TRANSFER-NOT-AUTHORIZED");

        let mut full = insert_only;
        full.operations.push(TransferOperation::Drop);
        assert!(check_grant(&[full], "c2", &needed).is_ok());
    }

    #[test]
    fn grant_is_per_connection() {
        let needed: BTreeSet<_> = [TransferOperation::Insert].into_iter().collect();
        let grants = [TransferGrant {
            plugin_id: "com.dbx.datax".into(),
            connection_id: "other".into(),
            operations: vec![TransferOperation::Insert],
        }];
        assert!(check_grant(&grants, "c2", &needed).is_err());
    }

    #[test]
    fn concurrency_caps_at_three() {
        assert!(check_concurrency(2).is_ok());
        assert_eq!(check_concurrency(3).unwrap_err().code, "E-TRANSFER-CONCURRENT-LIMIT");
    }

    #[test]
    fn request_hash_ignores_host_filled_fields() {
        let a = json!({"targetDatabase": "shop", "tables": ["b", "a"], "transferId": "t1", "dropTargetConfirmed": false});
        let b = json!({"tables": ["b", "a"], "targetDatabase": "shop", "transferId": "t2", "dropTargetConfirmed": true});
        assert_eq!(request_hash(&a), request_hash(&b));
        let c = json!({"targetDatabase": "other", "tables": ["b", "a"]});
        assert_ne!(request_hash(&a), request_hash(&c));
    }

    #[test]
    fn request_hash_is_key_order_independent() {
        let a = json!({"a": 1, "b": {"c": 2, "d": 3}});
        let b = json!({"b": {"d": 3, "c": 2}, "a": 1});
        assert_eq!(request_hash(&a), request_hash(&b));
    }

    #[test]
    fn dry_run_needs_no_operations_and_no_grant() {
        let mut request = base_request();
        request.dry_run = true;
        request.create_table = true;
        request.drop_target_before_create = true;
        assert!(operations_for_request(&request).is_empty());
        // No grants at all: still allowed, because nothing is written.
        assert!(check_grant_or_dry_run(&[], "c2", &request).is_ok());
    }

    #[test]
    fn dry_run_is_l0_even_for_destructive_requests() {
        let risk = classify_risk(&RiskInput {
            table_count: 30,
            create_table: true,
            drop_target_before_create: true,
            mode_overwrite_or_upsert: true,
            target_production: true,
            estimated_rows: None,
            dry_run: true,
            ..RiskInput::default()
        });
        assert_eq!(risk.level, RiskLevel::L0);
        assert!(risk.reasons.is_empty());
        assert!(risk.dry_run);
    }

    #[test]
    fn dry_run_cannot_be_replayed_as_a_write() {
        let dry = json!({"targetDatabase": "shop", "tables": ["orders"], "dryRun": true});
        let real = json!({"targetDatabase": "shop", "tables": ["orders"], "dryRun": false});
        let absent = json!({"targetDatabase": "shop", "tables": ["orders"]});
        assert_ne!(request_hash(&dry), request_hash(&real));
        assert_ne!(request_hash(&dry), request_hash(&absent));
    }

    #[test]
    fn real_request_still_needs_a_grant() {
        let request = base_request();
        assert_eq!(check_grant_or_dry_run(&[], "c2", &request).unwrap_err().code, "E-TRANSFER-NOT-AUTHORIZED");
    }

    #[test]
    fn session_approval_is_needed_when_no_grant_is_stored() {
        let request = base_request();
        let none = BTreeSet::new();
        assert_eq!(
            check_grant_or_session(&[], "c2", &request, &none).unwrap_err().code,
            "E-TRANSFER-NOT-AUTHORIZED"
        );
        let insert: BTreeSet<_> = [TransferOperation::Insert].into_iter().collect();
        assert!(check_grant_or_session(&[], "c2", &request, &insert).is_ok());
    }

    #[test]
    fn session_approval_must_cover_every_operation() {
        let mut request = base_request();
        request.drop_target_before_create = true;
        let insert: BTreeSet<_> = [TransferOperation::Insert].into_iter().collect();
        assert_eq!(
            check_grant_or_session(&[], "c2", &request, &insert).unwrap_err().code,
            "E-TRANSFER-NOT-AUTHORIZED"
        );
        let full: BTreeSet<_> = [TransferOperation::Insert, TransferOperation::Drop].into_iter().collect();
        assert!(check_grant_or_session(&[], "c2", &request, &full).is_ok());
    }

    #[test]
    fn stored_grant_makes_session_approval_unnecessary() {
        let request = base_request();
        let grant = TransferGrant {
            plugin_id: "com.dbx.datax".into(),
            connection_id: "c2".into(),
            operations: vec![TransferOperation::Insert],
        };
        // A dry run needs nothing at all, even with no grant and no approval.
        let mut dry = base_request();
        dry.dry_run = true;
        assert!(check_grant_or_session(&[], "c2", &dry, &BTreeSet::new()).is_ok());
        assert!(check_grant_or_session(&[grant], "c2", &request, &BTreeSet::new()).is_ok());
    }

    #[test]
    fn confirm_outcomes_map_to_codes() {
        assert!(ConfirmOutcome::Approved.into_result().is_ok());
        assert_eq!(ConfirmOutcome::Denied.into_result().unwrap_err().code, "E-TRANSFER-REJECTED");
        assert_eq!(ConfirmOutcome::Expired.into_result().unwrap_err().code, "E-TRANSFER-EXPIRED");
        assert_eq!(ConfirmOutcome::Stale.into_result().unwrap_err().code, "E-TRANSFER-STALE");
    }
}
