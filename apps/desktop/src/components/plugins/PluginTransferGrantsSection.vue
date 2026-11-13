<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { Loader2, ShieldAlert, X } from "@lucide/vue";
import * as api from "@/lib/backend/api";
import { Button } from "@/components/ui/button";
import { useToast } from "@/composables/useToast";
import type { InstalledPlugin } from "@/types/database";
import { PLUGIN_TRANSFER_PERMISSION, type PluginTransferGrant, type PluginTransferOperation } from "@/types/pluginTransfer";

/**
 * Connections this plugin may ask to write to, with a revoke button each
 * (docs/07 §9). The list is the only place a standing grant can be taken back
 * without depending on the next confirmation dialog happening.
 *
 * Revoking is not gated by the L2 switch: a closed write path is exactly when
 * the user wants to clean up the grants that are still on file.
 */
const props = defineProps<{ plugin: InstalledPlugin }>();

const { t } = useI18n();
const { toast } = useToast();

const pluginId = computed(() => props.plugin.manifest.id);
const declaresWrite = computed(() => (props.plugin.manifest.permissions || []).includes(PLUGIN_TRANSFER_PERMISSION));
const grants = ref<PluginTransferGrant[]>([]);
const revoking = ref<string | null>(null);
const writesEnabled = ref(true);

const OPERATION_KEYS: Record<PluginTransferOperation, string> = {
  insert: "pluginPlatform.transferGrant.opInsert",
  overwrite: "pluginPlatform.transferGrant.opOverwrite",
  truncate: "pluginPlatform.transferGrant.opTruncate",
  createTable: "pluginPlatform.transferGrant.opCreateTable",
  drop: "pluginPlatform.transferGrant.opDrop",
};

function operationLabel(operation: string): string {
  const key = OPERATION_KEYS[operation as PluginTransferOperation];
  return key ? t(key) : operation;
}

function operationsLabel(grant: PluginTransferGrant): string {
  return grant.operations.map(operationLabel).join(", ");
}

async function load() {
  grants.value = [];
  if (!declaresWrite.value) return;
  const id = pluginId.value;
  const [settings, current] = await Promise.all([api.getPluginTransferSettings().catch(() => null), api.getPluginTransferGrants(id).catch(() => [] as PluginTransferGrant[])]);
  // The selection may have moved on while the requests were in flight.
  if (id !== pluginId.value) return;
  writesEnabled.value = settings?.enabled === true && settings?.featureEnabled === true;
  grants.value = current;
}

async function revoke(grant: PluginTransferGrant) {
  if (revoking.value) return;
  revoking.value = grant.connectionId;
  try {
    grants.value = await api.setPluginTransferGrant(pluginId.value, grant.connectionId, grant.operations, false);
  } catch (error) {
    toast(t("pluginPlatform.writeAccess.revokeFailed", { message: error instanceof Error ? error.message : String(error) }), 5000);
  } finally {
    revoking.value = null;
  }
}

watch(pluginId, () => void load(), { immediate: true });
</script>

<template>
  <section v-if="declaresWrite" class="space-y-3 rounded-lg border p-4" data-plugin-write-access>
    <div class="flex min-w-0 items-start gap-2">
      <ShieldAlert class="mt-0.5 size-4 shrink-0 text-primary" />
      <div class="min-w-0">
        <div class="text-sm font-medium">{{ t("pluginPlatform.writeAccess.title") }}</div>
        <p class="mt-1 text-xs leading-5 text-muted-foreground">{{ t("pluginPlatform.writeAccess.description") }}</p>
      </div>
    </div>
    <div v-if="!writesEnabled" class="rounded-md border border-amber-500/30 bg-amber-500/5 px-3 py-2 text-xs text-amber-800 dark:text-amber-200">
      {{ t("pluginPlatform.writeAccess.globalOff") }}
    </div>
    <div v-if="!grants.length" class="rounded-md border border-dashed px-3 py-2 text-xs text-muted-foreground">{{ t("pluginPlatform.writeAccess.none") }}</div>
    <ul v-else class="divide-y rounded-md border">
      <li v-for="grant in grants" :key="grant.connectionId" class="flex items-start gap-2 px-3 py-2">
        <div class="min-w-0 flex-1">
          <div class="truncate text-xs">
            <span v-if="grant.connectionName">{{ grant.connectionName }}</span>
            <span v-else class="text-muted-foreground">{{ t("pluginPlatform.writeAccess.missingConnection", { id: grant.connectionId }) }}</span>
          </div>
          <div class="mt-0.5 text-[11px] leading-4 text-muted-foreground">{{ t("pluginPlatform.writeAccess.operations", { operations: operationsLabel(grant) }) }}</div>
        </div>
        <Button size="sm" variant="ghost" class="h-6 shrink-0 gap-1 px-2 text-xs text-destructive" :disabled="revoking === grant.connectionId" @click="revoke(grant)">
          <Loader2 v-if="revoking === grant.connectionId" class="size-3 animate-spin" />
          <X v-else class="size-3" />
          {{ t("pluginPlatform.writeAccess.revoke") }}
        </Button>
      </li>
    </ul>
  </section>
</template>
