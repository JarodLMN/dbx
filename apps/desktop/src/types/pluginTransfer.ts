/**
 * Plugin-initiated transfers for plugins (`host.transfer:run`, docs/07).
 *
 * Mirrors `crates/dbx-core/src/plugin_transfer.rs` and the thin commands in
 * `src-tauri/src/commands/plugin_transfer.rs`. The bridge injects `pluginId`;
 * the request payload is whatever the plugin compiled, and the backend stays
 * the only authority on its shape and on every rule that matters.
 */

/** Permission a plugin must declare to preview, run, or cancel a transfer. */
export const PLUGIN_TRANSFER_PERMISSION = "host.transfer:run";

/**
 * Capability for the full write path: preview + approved run + cancel.
 *
 * A host advertises it only when it can also show its own confirmation dialog,
 * because `run` without one would let a plugin write by asking twice.
 */
export const PLUGIN_TRANSFER_CAPABILITY = "transferApi";

/**
 * Capability for preview only, dry runs included.
 *
 * Advertised separately because a dry run never writes: it needs no
 * confirmation dialog and no grant, so a host that has not built the confirm
 * surface yet can still hand the plugin a plan instead of staying dark.
 */
export const PLUGIN_TRANSFER_PREVIEW_CAPABILITY = "transferPreview";

/** Mirrors the bound the core applies to `transferId` (it doubles as the run id). */
export const MAX_PLUGIN_TRANSFER_ID_CHARS = 128;

/**
 * The plugin's compiled request.
 *
 * Only the fields the bridge itself inspects are typed: everything else is
 * opaque here and re-deserialized into the core `TransferRequest`, which is the
 * only authority on its shape (snake_case-free camelCase, flat endpoints - the
 * core has no nested `source` / `target` objects).
 */
export interface PluginTransferRequest extends Record<string, unknown> {
  transferId: string;
  /** Must stay absent or `false`: the host fills it after the user confirms (docs/07 §1). */
  dropTargetConfirmed?: boolean;
  /** Plan and measure only. Part of the request hash, so it is never replayable as a write. */
  dryRun?: boolean;
}

/** Risk level as the gate serializes it (`RiskLevel` is `rename_all = "snake_case"`). */
export type PluginTransferRiskLevel = "l0" | "l1" | "l2";

export interface PluginTransferRiskReason {
  code: string;
  detail: string;
  level: PluginTransferRiskLevel;
}

export interface PluginTransferRisk {
  level: PluginTransferRiskLevel;
  reasons: PluginTransferRiskReason[];
  /** True when this risk belongs to a dry run: the plan is shown, nothing is written. */
  dryRun: boolean;
}

/** One endpoint as the host resolved it. Never taken from the plugin. */
export interface PluginTransferEndpointCard {
  connectionId: string;
  name: string;
  dbType: string;
  database: string;
  schema: string;
  readOnly: boolean;
  production: boolean;
}

export interface PluginTransferResolved {
  source: PluginTransferEndpointCard;
  target: PluginTransferEndpointCard;
  tables: string[];
  mode: string;
  content: string;
  createsTables: boolean;
  dropsTables: boolean;
  ownershipPolicy: string;
  filters: Array<{ table: string; sql: string }>;
  estimatedRows: number | null;
}

export interface PluginTransferPlannedStatement {
  kind: string;
  objectName?: string;
  sourceTable?: string;
  targetTable?: string;
}

/** What a dry run would do. Every statement here was generated, none was executed. */
export interface PluginTransferDryRunProducts {
  structureSql: string;
  operations: PluginTransferPlannedStatement[];
  schemaObjectsExpanded: boolean;
  rebuildSql?: string;
  backupSql?: string;
  cleanupSql?: string;
  /** Roles the target lacks: ownership DDL would fail in a real run. */
  missingOwners: string[];
  targetOwner: string;
}

export interface PluginTransferPreview {
  risk: PluginTransferRisk;
  resolved: PluginTransferResolved;
  /** Operations the plugin's grant must cover; empty for a dry run. */
  operations: string[];
  /** sha256(canonicalJson(request)) - the confirm step rejects a different one. */
  requestHash: string;
  /** Present only for a dry run. */
  dryRun?: PluginTransferDryRunProducts;
}

/** `plugin_transfer_run` returns the run id and, for a dry run, its plan. */
export interface PluginTransferRunHandle {
  runId: string;
  dryRun?: PluginTransferDryRunProducts;
}

/**
 * Validate what the bridge itself relies on, before the request reaches the
 * backend: a run needs an id, and a confirmation bit can never come from the
 * plugin. Everything else is the core's call.
 */
export function requirePluginTransferRequest(input: Record<string, unknown>): PluginTransferRequest {
  const raw = input.transferId;
  if (typeof raw !== "string" || !raw.trim()) throw new Error("host.transfer: transferId is required");
  const transferId = raw.trim();
  if (transferId.length > MAX_PLUGIN_TRANSFER_ID_CHARS) {
    throw new Error(`transferId must be at most ${MAX_PLUGIN_TRANSFER_ID_CHARS} characters`);
  }
  // A pre-set confirmation bit is an attempt to skip the dialog (docs/07 §1, §10).
  // Refuse it here as well as in the gate so the plugin gets the code immediately.
  if (input.dropTargetConfirmed !== undefined && input.dropTargetConfirmed !== false) {
    throw new Error("E-TRANSFER-PRECONFIRMED: dropTargetConfirmed must be filled by the host, not the plugin");
  }
  if (input.dryRun !== undefined && typeof input.dryRun !== "boolean") throw new Error("dryRun must be a boolean");
  return { ...input, transferId };
}

/**
 * Re-raise a `PluginTransferError { code, message }` as an Error whose message
 * leads with the `E-TRANSFER-*` code.
 *
 * The generic backend boundary keeps only the human text, and both the plugin UI
 * and its ledger branch on the code, so the code has to survive the hop.
 */
export function rethrowPluginTransferError(error: unknown): never {
  const record = error && typeof error === "object" ? (error as Record<string, unknown>) : null;
  const code = record && typeof record.code === "string" ? record.code : "";
  const message = record && typeof record.message === "string" ? record.message : error instanceof Error ? error.message : String(error);
  throw new Error(code ? `${code}: ${message}` : message);
}
