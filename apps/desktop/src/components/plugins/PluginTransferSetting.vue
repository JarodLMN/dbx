<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { Loader2, ShieldCheck } from "@lucide/vue";
import * as api from "@/lib/backend/api";
import { Switch } from "@/components/ui/switch";
import { useToast } from "@/composables/useToast";
import { isTauriRuntime } from "@/lib/backend/tauriRuntime";
import type { PluginTransferSettings } from "@/types/pluginTransfer";

/**
 * The L2 runtime switch (docs/07 §17.1): one switch for every plugin.
 *
 * Hidden when the build does not carry the `plugin-transfer` feature - offering a
 * switch that can only be ineffective is worse than not offering it. The host
 * answers both bits, so nothing here guesses from a version number.
 */
const { t } = useI18n();
const { toast } = useToast();

const settings = ref<PluginTransferSettings | null>(null);
const saving = ref(false);
const visible = computed(() => settings.value?.featureEnabled === true);

async function load() {
  if (!isTauriRuntime()) return;
  try {
    settings.value = await api.getPluginTransferSettings();
  } catch {
    // A host that cannot answer has no write path worth a switch.
    settings.value = null;
  }
}

async function setEnabled(enabled: boolean) {
  if (saving.value) return;
  saving.value = true;
  try {
    settings.value = await api.setPluginTransferEnabled(enabled);
  } catch (error) {
    toast(t("pluginPlatform.transferWrites.saveFailed", { message: error instanceof Error ? error.message : String(error) }), 5000);
  } finally {
    saving.value = false;
  }
}

onMounted(() => void load());
</script>

<template>
  <section v-if="visible" class="space-y-3 rounded-xl border p-4" data-plugin-transfer-writes>
    <div class="flex items-start justify-between gap-3">
      <div class="flex min-w-0 items-start gap-2">
        <ShieldCheck class="mt-0.5 size-4 shrink-0 text-primary" />
        <div class="min-w-0">
          <div class="text-sm font-medium">{{ t("pluginPlatform.transferWrites.title") }}</div>
          <p class="mt-1 text-xs leading-5 text-muted-foreground">{{ t("pluginPlatform.transferWrites.description") }}</p>
        </div>
      </div>
      <Loader2 v-if="saving" class="mt-0.5 size-4 shrink-0 animate-spin text-muted-foreground" />
      <Switch v-else :model-value="settings?.enabled === true" :aria-label="t('pluginPlatform.transferWrites.title')" @update:model-value="setEnabled($event === true)" />
    </div>
  </section>
</template>
