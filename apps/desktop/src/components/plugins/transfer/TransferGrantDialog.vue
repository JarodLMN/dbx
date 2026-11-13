<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { ShieldQuestion } from "@lucide/vue";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { isDestructiveTransferOperation, type PluginTransferOperation, type PluginTransferPreview } from "@/types/pluginTransfer";

/**
 * The grant dialog (docs/07 §9).
 *
 * A grant answers "may this plugin ask me to write to this connection?", which is a
 * different question from the confirm dialog's "do it this time". It is asked once
 * per connection until the user picks 始终允许, and the choices are what the host
 * persists - the plugin never sees or sets them.
 *
 * 破坏性操作默认不勾选: `truncate` and `drop` must be ticked on purpose, so a
 * standing grant can never be widened by an inattentive click.
 */
type GrantDecision = { decision: "denied" } | { decision: "once" | "always"; operations: PluginTransferOperation[] };

const { t } = useI18n();

const open = ref(false);
const preview = ref<PluginTransferPreview | null>(null);
const allowAlways = ref(true);
const selected = ref<Set<PluginTransferOperation>>(new Set());
let settle: ((decision: GrantDecision) => void) | undefined;

const connectionName = computed(() => preview.value?.resolved.target.name ?? "");
const operations = computed<PluginTransferOperation[]>(() => preview.value?.operations ?? []);
const canAllow = computed(() => operations.value.every((operation) => selected.value.has(operation)));

function operationLabel(operation: PluginTransferOperation): string {
  switch (operation) {
    case "insert":
      return t("pluginPlatform.transferGrant.opInsert");
    case "overwrite":
      return t("pluginPlatform.transferGrant.opOverwrite");
    case "createTable":
      return t("pluginPlatform.transferGrant.opCreateTable");
    case "truncate":
      return t("pluginPlatform.transferGrant.opTruncate");
    case "drop":
      return t("pluginPlatform.transferGrant.opDrop");
  }
}

function toggle(operation: PluginTransferOperation, checked: boolean) {
  const next = new Set(selected.value);
  if (checked) next.add(operation);
  else next.delete(operation);
  selected.value = next;
}

function finish(decision: GrantDecision) {
  const resolve = settle;
  settle = undefined;
  open.value = false;
  resolve?.(decision);
}

function deny() {
  finish({ decision: "denied" });
}

function allow(kind: "once" | "always") {
  if (!canAllow.value) return;
  // Keep the request's own order so the persisted array reads like the preview.
  finish({ decision: kind, operations: operations.value.filter((operation) => selected.value.has(operation)) });
}

/**
 * Ask for `(pluginId, connectionId, operations[])`. `allowAlways` is false for L2
 * runs, which are re-confirmed every time and must never write a standing grant.
 */
function request(next: PluginTransferPreview, options: { allowAlways: boolean }): Promise<GrantDecision> {
  if (settle) return Promise.reject(new Error("A plugin transfer grant prompt is already open"));
  preview.value = next;
  allowAlways.value = options.allowAlways;
  // Destructive operations start unticked (docs/07 §9).
  selected.value = new Set(next.operations.filter((operation) => !isDestructiveTransferOperation(operation)));
  open.value = true;
  return new Promise((resolve) => {
    settle = resolve;
  });
}

// Closing by Esc or the overlay is a refusal, same as the explicit button.
watch(open, (value) => {
  if (!value && settle) finish({ decision: "denied" });
});
onBeforeUnmount(() => finish({ decision: "denied" }));
defineExpose({ request, deny });
</script>

<template>
  <Dialog :open="open" @update:open="(value: boolean) => (open = value)">
    <DialogContent class="w-[calc(100vw-2rem)] max-w-md">
      <DialogHeader>
        <DialogTitle class="flex items-center gap-2 text-base">
          <ShieldQuestion class="h-4 w-4 shrink-0" />
          <span>{{ t("pluginPlatform.transferGrant.title", { connection: connectionName }) }}</span>
        </DialogTitle>
        <DialogDescription>{{ t("pluginPlatform.transferGrant.description") }}</DialogDescription>
      </DialogHeader>

      <div class="space-y-2 text-sm">
        <p class="text-xs uppercase tracking-wide text-muted-foreground">{{ t("pluginPlatform.transferGrant.operationsHeading") }}</p>
        <label v-for="operation in operations" :key="operation" class="flex items-start gap-2 rounded border p-2">
          <input type="checkbox" class="mt-0.5 h-3.5 w-3.5 shrink-0" :class="isDestructiveTransferOperation(operation) ? 'accent-destructive' : 'accent-primary'" :checked="selected.has(operation)" @change="toggle(operation, ($event.target as HTMLInputElement).checked)" />
          <span class="text-xs" :class="isDestructiveTransferOperation(operation) ? 'text-destructive' : ''">{{ operationLabel(operation) }}</span>
        </label>
        <p class="text-xs text-muted-foreground">{{ t("pluginPlatform.transferGrant.destructiveHint") }}</p>
        <p v-if="allowAlways" class="text-xs text-muted-foreground">{{ t("pluginPlatform.transferGrant.alwaysHint") }}</p>
      </div>

      <DialogFooter class="flex-wrap justify-end gap-2">
        <Button variant="outline" @click="deny">{{ t("pluginPlatform.transferGrant.deny") }}</Button>
        <Button variant="outline" :disabled="!canAllow" @click="allow('once')">{{ t("pluginPlatform.transferGrant.once") }}</Button>
        <Button v-if="allowAlways" :disabled="!canAllow" @click="allow('always')">{{ t("pluginPlatform.transferGrant.always") }}</Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
</template>
