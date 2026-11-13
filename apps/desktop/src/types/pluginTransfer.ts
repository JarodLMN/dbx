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
  operations: PluginTransferOperation[];
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
 * One write operation a grant can cover (docs/07 §9).
 *
 * Mirrors `gate::TransferOperation`: the serde labels are the wire names, so the
 * strings below are what the backend parses and what a stored grant holds.
 */
export type PluginTransferOperation = "insert" | "truncate" | "overwrite" | "createTable" | "drop";

export const PLUGIN_TRANSFER_OPERATIONS: readonly PluginTransferOperation[] = ["insert", "overwrite", "createTable", "truncate", "drop"];

/**
 * Destructive operations are never pre-checked in the grant dialog, and the confirm
 * dialog raises them as risk reasons instead (docs/07 §9).
 */
export function isDestructiveTransferOperation(operation: PluginTransferOperation): boolean {
  return operation === "truncate" || operation === "drop";
}

/** A stored `(pluginId, connectionId, operations[])` grant, as `get_grants` returns it. */
export interface PluginTransferGrant {
  /** Connection name at read time; absent when the connection no longer exists. */
  connectionName?: string;
  connectionId: string;
  operations: PluginTransferOperation[];
}

/**
 * One progress tick for a plugin-initiated run.
 *
 * Mirrors `TransferProgress` (`crates/dbx-core/src/data/transfer.rs`); the host
 * adds nothing, so a plugin reads the same shape the app's own transfer panel
 * does. `transferId` is the `runId` returned by `run`.
 */
export interface PluginTransferProgress {
  transferId: string;
  table: string;
  tableIndex: number;
  totalTables: number;
  rowsTransferred: number;
  totalRows: number | null;
  /** `running` | `tableDone` | `done` | `error` | `cancelled`. */
  status: string;
  error: string | null;
  /** True on the final tick only. */
  terminal: boolean;
}

/**
 * Event method a plugin receives its own run's ticks under (docs/07 §8).
 *
 * Not `host.transfer.progress` from the app's global broadcast: the host filters
 * `pluginId` first and re-emits under this method, so a plugin can never observe
 * another plugin's (or the app's) run.
 */
export const PLUGIN_TRANSFER_PROGRESS_METHOD = "host.transfer.progress";

/** `plugin-transfer-progress` payload emitted by `launch_transfer`. */
export interface PluginTransferProgressEvent {
  pluginId: string;
  progress: PluginTransferProgress;
  terminal?: boolean;
}

/**
 * Event method the finished record arrives under (docs/07 §8).
 *
 * `host.transfer.finished` is the one event a plugin's ledger must record; the
 * progress stream ends with `terminal: true` as well, but that tick is a
 * transfer-shaped packet and half its fields are per-table. This one is flat.
 */
export const PLUGIN_TRANSFER_FINISHED_METHOD = "host.transfer.finished";

/**
 * How a plugin-initiated run ended, as `plugin-transfer-finished` carries it.
 *
 * `status` is the serialized `TransferStatus` (`running | tableDone | done |
 * error | cancelled`); a finished event is only ever sent for a terminal one.
 * `rowsTransferred` is the runner's cumulative count - the runner does not
 * separate rows read from rows written, so neither does this event.
 * `dryRun` is true for a plan-only run: nothing was written and no ledger row
 * should be booked as a write.
 */
export interface PluginTransferFinished {
  runId: string;
  status: string;
  rowsTransferred: number;
  totalRows: number | null;
  durationMs: number;
  error: string | null;
  dryRun: boolean;
}

/** `plugin-transfer-finished` payload emitted by `launch_transfer`. */
export interface PluginTransferFinishedEvent extends PluginTransferFinished {
  pluginId: string;
}

/**
 * The two bits behind `capabilities.transferApi` (docs/07 §17.1/§17.3).
 *
 * `featureEnabled` is the L1 compile flag, `enabled` the L2 user switch. The
 * settings UI shows the switch only when the feature is compiled in, and the
 * workbench injects `transfer.*` only when both are true.
 */
export interface PluginTransferSettings {
  enabled: boolean;
  featureEnabled: boolean;
}

/** Confirmation dialog lifetime (docs/07 §7): expiry denies, it never approves. */
export const PLUGIN_TRANSFER_CONFIRM_TIMEOUT_MS = 120_000;

/**
 * Flatten the plugin's "why" into the one quotation the dialog shows.
 *
 * The plugin SDK hands over `{ title, reason }` (docs/07 §5 `intent.reason`), but
 * older plugins pass a plain string. Either way this is a quotation inside the
 * host's dialog and never a host claim, so it is bounded to a few lines and the
 * numbers it carries are ignored - the host gets its estimate from the request.
 */
export function normalizePluginTransferIntent(value: unknown): string | undefined {
  const max = 2000;
  if (typeof value === "string") return value.trim() ? value.trim().slice(0, max) : undefined;
  if (!value || typeof value !== "object") return undefined;
  const record = value as Record<string, unknown>;
  const title = typeof record.title === "string" ? record.title.trim() : "";
  const reason = typeof record.reason === "string" ? record.reason.trim() : "";
  const text = [title, reason].filter(Boolean).join("\n");
  return text ? text.slice(0, max) : undefined;
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
