<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { AlertTriangle } from "@lucide/vue";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { PLUGIN_TRANSFER_CONFIRM_TIMEOUT_MS, type PluginTransferPreview } from "@/types/pluginTransfer";

/**
 * The host's own confirmation dialog (docs/07 §5-§7).
 *
 * It is the L4 gate: nothing reaches the database until the user answers here, and
 * every fact shown is host-resolved (`preview.resolved` / `preview.risk`) or a
 * request field the host already validated. `intent` is the one plugin-supplied
 * text, and it is labelled as such.
 *
 * The component owns the dialog and the 120 s budget: `request()` resolves once,
 * either with the user's answer or with `expired`. Expiry closes and denies - a
 * timeout never approves, and it never re-asks on its own.
 */
const props = defineProps<{ pluginName: string }>();
const { t } = useI18n();

type TransferConfirmOutcome = "approved" | "denied" | "expired";

const open = ref(false);
const preview = ref<PluginTransferPreview | null>(null);
const intent = ref("");
const compatibilityAccepted = ref(false);
const typedName = ref("");
const remaining = ref(0);
let settle: ((outcome: TransferConfirmOutcome) => void) | undefined;
let timer: ReturnType<typeof setInterval> | undefined;

const level = computed(() => preview.value?.risk.level ?? "l0");
const target = computed(() => preview.value?.resolved.target);
const tableCount = computed(() => preview.value?.resolved.tables.length ?? 0);
const reasonCount = computed(() => preview.value?.risk.reasons.length ?? 0);
const filteredTables = computed(() => preview.value?.resolved.tables.slice(0, 5) ?? []);
const hiddenTables = computed(() => Math.max(0, tableCount.value - filteredTables.value.length));

const actionLabel = computed(() => {
  const resolved = preview.value?.resolved;
  if (!resolved) return "";
  if (resolved.dropsTables) return t("pluginPlatform.transferConfirm.actionRebuild");
  if (resolved.createsTables) return t("pluginPlatform.transferConfirm.actionCreate");
  if (resolved.mode === "overwrite" || resolved.mode === "upsert") return t("pluginPlatform.transferConfirm.actionOverwrite");
  return t("pluginPlatform.transferConfirm.actionAppend");
});

const modeLabel = computed(() => {
  const mode = preview.value?.resolved.mode;
  if (mode === "overwrite") return t("pluginPlatform.transferConfirm.modeOverwrite");
  if (mode === "upsert") return t("pluginPlatform.transferConfirm.modeUpsert");
  return t("pluginPlatform.transferConfirm.modeAppend");
});

const contentLabel = computed(() => {
  const content = preview.value?.resolved.content;
  if (content === "structureOnly") return t("pluginPlatform.transferConfirm.contentStructureOnly");
  if (content === "structureAndData") return t("pluginPlatform.transferConfirm.contentStructureAndData");
  return t("pluginPlatform.transferConfirm.contentDataOnly");
});

const impactLabel = computed(() => {
  const resolved = preview.value?.resolved;
  if (!resolved) return t("pluginPlatform.transferConfirm.impactNone");
  if (resolved.dropsTables) return t("pluginPlatform.transferConfirm.impactDrops");
  if (resolved.createsTables) return t("pluginPlatform.transferConfirm.impactCreates");
  return t("pluginPlatform.transferConfirm.impactNone");
});

const estimateLabel = computed(() => {
  const rows = preview.value?.resolved.estimatedRows;
  return rows === null || rows === undefined ? t("pluginPlatform.transferConfirm.estimatedRowsUnknown") : t("pluginPlatform.transferConfirm.estimatedRows", { rows: rows.toLocaleString() });
});

/** L2 is the only level that asks for the target's name typed back (docs/07 §6). */
const typeToConfirm = computed(() => level.value === "l2");
const requiresCompatibility = computed(() => level.value === "l1");
const typedMatches = computed(() => {
  const name = target.value?.name?.trim().toLowerCase() ?? "";
  return name.length > 0 && typedName.value.trim().toLowerCase() === name;
});

const canConfirm = computed(() => {
  if (typeToConfirm.value) return typedMatches.value;
  if (requiresCompatibility.value) return compatibilityAccepted.value;
  return true;
});

const confirmLabel = computed(() => {
  if (typeToConfirm.value) return t("pluginPlatform.transferConfirm.confirmAnyway", { count: tableCount.value });
  if (requiresCompatibility.value) return t("pluginPlatform.transferConfirm.confirmTables", { count: tableCount.value });
  return t("pluginPlatform.transferConfirm.confirm");
});

function stopTimer() {
  if (timer !== undefined) {
    clearInterval(timer);
    timer = undefined;
  }
}

function finish(outcome: TransferConfirmOutcome) {
  stopTimer();
  const resolve = settle;
  settle = undefined;
  open.value = false;
  resolve?.(outcome);
}

function cancel() {
  if (!settle) {
    open.value = false;
    return;
  }
  finish("denied");
}

/** Ask, and resolve exactly once: `approved`, `denied`, or `expired`. */
function request(next: PluginTransferPreview, pluginIntent?: string): Promise<TransferConfirmOutcome> {
  if (settle) return Promise.reject(new Error("A plugin transfer confirmation is already open"));
  preview.value = next;
  // The plugin's own words are shown as a quotation, never as a host claim.
  intent.value = (pluginIntent ?? "").slice(0, 2000);
  compatibilityAccepted.value = false;
  typedName.value = "";
  remaining.value = Math.round(PLUGIN_TRANSFER_CONFIRM_TIMEOUT_MS / 1000);
  open.value = true;
  stopTimer();
  timer = setInterval(() => {
    remaining.value -= 1;
    if (remaining.value <= 0) finish("expired");
  }, 1000);
  return new Promise((resolve) => {
    settle = resolve;
  });
}

// Esc, the close button and an outside click all mean "no"; the Dialog signal is
// the single place that turns any of them into a settled promise.
watch(open, (value) => {
  if (!value && settle) finish("denied");
});
onBeforeUnmount(() => finish("denied"));
defineExpose({ request, cancel });
</script>

<template>
  <Dialog :open="open" @update:open="(value: boolean) => (open = value)">
    <DialogContent class="flex max-h-[85vh] w-[calc(100vw-2rem)] max-w-2xl flex-col gap-0 p-0">
      <DialogHeader class="border-b px-5 py-4">
        <DialogTitle class="flex flex-wrap items-center gap-2 text-base">
          <AlertTriangle class="h-4 w-4 shrink-0 text-destructive" />
          <span>{{ t("pluginPlatform.transferConfirm.title") }}</span>
          <span class="rounded bg-muted px-1.5 py-0.5 text-xs font-normal text-muted-foreground">{{ actionLabel }}</span>
          <span class="rounded bg-muted px-1.5 py-0.5 text-xs font-normal text-muted-foreground">{{ pluginName }}</span>
          <span v-if="target?.production" class="rounded bg-destructive px-1.5 py-0.5 text-xs font-semibold text-destructive-foreground">{{ t("pluginPlatform.transferConfirm.productionBadge") }}</span>
        </DialogTitle>
        <DialogDescription>{{ t("pluginPlatform.transferConfirm.subtitle") }}</DialogDescription>
      </DialogHeader>

      <div class="min-h-0 flex-1 overflow-auto px-5 py-4 text-sm">
        <!-- Endpoints: the host resolved both sides, the plugin named neither. -->
        <div class="grid gap-3 sm:grid-cols-2">
          <section class="rounded border bg-muted/20 p-3">
            <p class="mb-1 text-xs uppercase tracking-wide text-muted-foreground">{{ t("pluginPlatform.transferConfirm.source") }}</p>
            <p class="break-words font-medium">{{ preview?.resolved.source.name }}</p>
            <p class="break-all text-xs text-muted-foreground">
              {{ [preview?.resolved.source.database, preview?.resolved.source.schema].filter(Boolean).join(".") || "—" }}
            </p>
            <ul class="mt-2 space-y-0.5 font-mono text-xs">
              <li v-for="table in filteredTables" :key="table" class="break-all">{{ table }}</li>
              <li v-if="hiddenTables > 0" class="text-muted-foreground">
                {{ t("pluginPlatform.transferConfirm.moreTables", { count: hiddenTables }) }}
              </li>
            </ul>
          </section>
          <section class="rounded border bg-muted/20 p-3">
            <p class="mb-1 text-xs uppercase tracking-wide text-muted-foreground">{{ t("pluginPlatform.transferConfirm.target") }}</p>
            <p class="flex flex-wrap items-center gap-1.5 break-words font-medium">
              <span>{{ target?.name }}</span>
              <span v-if="target?.production" class="rounded bg-destructive px-1 py-0.5 text-[0.7rem] text-destructive-foreground">{{ t("pluginPlatform.transferConfirm.productionBadge") }}</span>
            </p>
            <p class="break-all text-xs text-muted-foreground">
              {{ [target?.database, target?.schema].filter(Boolean).join(".") || "—" }} ·
              {{ t("pluginPlatform.transferConfirm.tableCount", { count: tableCount }) }}
            </p>
            <p class="mt-2 text-xs text-muted-foreground">{{ target?.dbType }}</p>
          </section>
        </div>

        <!-- Impact: mode, structure changes and the plugin's own estimate. -->
        <section class="mt-3 rounded border bg-muted/20 p-3">
          <p class="mb-2 text-xs uppercase tracking-wide text-muted-foreground">{{ t("pluginPlatform.transferConfirm.impact") }}</p>
          <dl class="grid gap-x-4 gap-y-1 text-xs sm:grid-cols-[auto_1fr]">
            <dt class="text-muted-foreground">{{ t("pluginPlatform.transferConfirm.writeMode") }}</dt>
            <dd>{{ modeLabel }} · {{ contentLabel }}</dd>
            <dt class="text-muted-foreground">{{ t("pluginPlatform.transferConfirm.structure") }}</dt>
            <dd :class="preview?.resolved.dropsTables ? 'font-medium text-destructive' : ''">{{ impactLabel }}</dd>
            <dt class="text-muted-foreground">{{ t("pluginPlatform.transferConfirm.estimatedRowsLabel") }}</dt>
            <dd>{{ estimateLabel }}</dd>
          </dl>
        </section>

        <!-- Filters: verbatim, never rewritten, so the user reads the real WHERE. -->
        <section v-if="(preview?.resolved.filters.length ?? 0) > 0" class="mt-3 rounded border bg-muted/20 p-3">
          <p class="mb-2 text-xs uppercase tracking-wide text-muted-foreground">{{ t("pluginPlatform.transferConfirm.filters") }}</p>
          <div v-for="filter in preview?.resolved.filters" :key="filter.table" class="mb-1.5 last:mb-0">
            <p class="break-all text-xs text-muted-foreground">{{ filter.table }}</p>
            <p class="max-h-24 overflow-auto whitespace-pre-wrap break-all rounded bg-muted/50 p-2 font-mono text-xs" dir="ltr">{{ filter.sql }}</p>
          </div>
        </section>

        <!-- Risk: host-classified, capped at three lines (docs/07 §5). -->
        <section v-if="reasonCount > 0" class="mt-3 rounded border border-destructive/40 bg-destructive/10 p-3">
          <p class="mb-1 text-xs uppercase tracking-wide text-destructive">{{ t("pluginPlatform.transferConfirm.riskHeading") }}</p>
          <ul class="space-y-0.5 text-xs">
            <li v-for="reason in preview?.risk.reasons.slice(0, 3)" :key="`${reason.code}-${reason.detail}`" class="flex gap-1.5">
              <AlertTriangle class="mt-0.5 h-3 w-3 shrink-0 text-destructive" />
              <span class="break-words">{{ reason.detail }}</span>
            </li>
          </ul>
        </section>

        <section v-if="intent" class="mt-3 rounded border-l-2 border-muted-foreground/40 bg-muted/30 px-3 py-2">
          <p class="text-xs uppercase tracking-wide text-muted-foreground">{{ t("pluginPlatform.transferConfirm.intentHeading") }}</p>
          <p class="mt-1 line-clamp-3 whitespace-pre-wrap break-words text-xs">{{ intent }}</p>
        </section>

        <label v-if="requiresCompatibility" class="mt-3 flex items-start gap-2 rounded border p-3 text-xs">
          <input v-model="compatibilityAccepted" type="checkbox" class="mt-0.5 h-3.5 w-3.5 shrink-0 accent-primary" />
          <span>{{ t("pluginPlatform.transferConfirm.compatibility") }}</span>
        </label>

        <div v-if="typeToConfirm" class="mt-3 rounded border border-destructive/40 p-3">
          <label class="block text-xs font-medium" for="plugin-transfer-confirm-typed-name">{{ t("pluginPlatform.transferConfirm.typeLabel", { name: target?.name ?? "" }) }}</label>
          <input id="plugin-transfer-confirm-typed-name" v-model="typedName" class="mt-1.5 h-8 w-full rounded border bg-background px-2 font-mono text-sm" :placeholder="t('pluginPlatform.transferConfirm.typePlaceholder')" autocomplete="off" spellcheck="false" />
          <p v-if="typedName.trim() && !typedMatches" class="mt-1 text-xs text-destructive" role="alert">{{ t("pluginPlatform.transferConfirm.typeMismatch") }}</p>
        </div>
      </div>

      <DialogFooter class="items-center justify-between gap-2 border-t px-5 py-3 sm:justify-between">
        <span class="text-xs text-muted-foreground">{{ t("pluginPlatform.transferConfirm.expiresIn", { seconds: Math.max(0, remaining) }) }}</span>
        <span class="flex gap-2">
          <Button variant="outline" @click="cancel">{{ t("common.cancel") }}</Button>
          <Button :variant="typeToConfirm ? 'destructive' : 'default'" :disabled="!canConfirm" @click="finish('approved')">{{ confirmLabel }}</Button>
        </span>
      </DialogFooter>
    </DialogContent>
  </Dialog>
</template>
