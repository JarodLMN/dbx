// @vitest-environment happy-dom

import { createApp, nextTick, type App } from "vue";
import { createI18n } from "vue-i18n";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { PluginTransferOperation, PluginTransferPreview, PluginTransferRiskLevel } from "@/types/pluginTransfer";
import TransferConfirmDialog from "./TransferConfirmDialog.vue";
import TransferGrantDialog from "./TransferGrantDialog.vue";

/**
 * The L4 confirm dialog and the L3 grant dialog are the only two places where a
 * plugin write can be stopped, so the gates they enforce get asserted here rather
 * than left to a manual click-through: the compatibility checkbox (L1), the typed
 * connection name (L2), the 120 s budget, and "destructive operations start
 * unticked" in the grant prompt.
 *
 * i18n is mounted with empty messages, so `t("...")` returns the key itself; the
 * assertions below then read keys instead of prose and stay locale-independent.
 */

interface ConfirmInstance {
  request: (preview: PluginTransferPreview, intent?: string) => Promise<"approved" | "denied" | "expired">;
  cancel: () => void;
}

type GrantAnswer = { decision: "denied" } | { decision: "once" | "always"; operations: PluginTransferOperation[] };

interface GrantInstance {
  request: (preview: PluginTransferPreview, options: { allowAlways: boolean }) => Promise<GrantAnswer>;
  deny: () => void;
}

const mountedApps: App[] = [];

afterEach(() => {
  for (const app of mountedApps.splice(0)) app.unmount();
  document.body.replaceChildren();
  vi.useRealTimers();
});

function mount(component: typeof TransferConfirmDialog | typeof TransferGrantDialog, props: Record<string, unknown>): unknown {
  const host = document.createElement("div");
  document.body.append(host);
  const app = createApp(component, props);
  app.use(createI18n({ legacy: false, locale: "en", messages: { en: {} }, missingWarn: false, fallbackWarn: false }));
  mountedApps.push(app);
  return app.mount(host);
}

/** Dialogs open through a transition and a portal, so a single nextTick is not enough. */
async function flush() {
  for (let turn = 0; turn < 5; turn += 1) {
    await nextTick();
    await new Promise((resolve) => setTimeout(resolve));
    await nextTick();
  }
}

function button(label: string): HTMLButtonElement {
  const found = [...document.querySelectorAll("button")].find((candidate) => (candidate.textContent ?? "").trim() === label);
  if (!found) throw new Error(`button not rendered: ${label}`);
  return found as HTMLButtonElement;
}

function box(): HTMLInputElement {
  const found = document.querySelector<HTMLInputElement>('input[type="checkbox"]');
  if (!found) throw new Error("no checkbox was rendered");
  return found;
}

function check(input: HTMLInputElement, checked: boolean) {
  input.checked = checked;
  input.dispatchEvent(new Event("change"));
}

function type(input: HTMLInputElement, value: string) {
  input.value = value;
  input.dispatchEvent(new Event("input"));
}

function endpoint(name: string, production = false) {
  return {
    connectionId: name.toLowerCase().replaceAll(" ", "-"),
    name,
    dbType: "mysql",
    database: "shop",
    schema: "shop",
    readOnly: false,
    production,
  };
}

function fixture(options: { level: PluginTransferRiskLevel; operations?: PluginTransferOperation[] }): PluginTransferPreview {
  return {
    risk: {
      level: options.level,
      reasons: options.level === "l0" ? [] : [{ code: "structure", detail: "target tables will be rebuilt", level: options.level }],
      dryRun: false,
    },
    resolved: {
      source: endpoint("Prod MySQL"),
      target: endpoint("Prod MySQL", true),
      tables: ["orders"],
      mode: "append",
      content: "dataOnly",
      createsTables: false,
      dropsTables: false,
      ownershipPolicy: "preserve",
      filters: [],
      estimatedRows: 1200,
    },
    operations: options.operations ?? ["insert"],
    requestHash: "host-computed-hash",
  };
}

describe("transfer confirmation (L4)", () => {
  it("keeps an L1 write blocked until the compatibility box is ticked", async () => {
    const dialog = mount(TransferConfirmDialog, { pluginName: "datax" }) as ConfirmInstance;
    const outcome = dialog.request(fixture({ level: "l1" }), "把 orders 同步");
    await flush();

    const confirm = button("pluginPlatform.transferConfirm.confirmTables");
    expect(confirm.disabled).toBe(true);

    check(box(), true);
    await flush();
    expect(confirm.disabled).toBe(false);

    confirm.click();
    await flush();
    await expect(outcome).resolves.toBe("approved");
  });

  it("asks for the target connection name on L2 and rejects a near match", async () => {
    const dialog = mount(TransferConfirmDialog, { pluginName: "datax" }) as ConfirmInstance;
    const outcome = dialog.request(fixture({ level: "l2" }));
    await flush();

    const confirm = button("pluginPlatform.transferConfirm.confirmAnyway");
    // No checkbox at L2: the only way through is the typed name.
    expect(document.querySelector('input[type="checkbox"]')).toBeNull();
    expect(confirm.disabled).toBe(true);

    const nameInput = document.querySelector<HTMLInputElement>("#plugin-transfer-confirm-typed-name");
    if (!nameInput) throw new Error("the type-to-confirm input was not rendered");

    type(nameInput, "Prod MySQ");
    await flush();
    expect(confirm.disabled).toBe(true);
    expect(document.querySelector('[role="alert"]')?.textContent).toBe("pluginPlatform.transferConfirm.typeMismatch");

    // Trimmed and case-folded, exactly as docs/07 §6 specifies.
    type(nameInput, "  prod mysql ");
    await flush();
    expect(confirm.disabled).toBe(false);

    confirm.click();
    await flush();
    await expect(outcome).resolves.toBe("approved");
  });

  it("expires after the 120 s budget and never turns that into an approval", async () => {
    vi.useFakeTimers();
    const dialog = mount(TransferConfirmDialog, { pluginName: "datax" }) as ConfirmInstance;
    const outcome = dialog.request(fixture({ level: "l0" }));

    await vi.advanceTimersByTimeAsync(119_000);
    let settled: unknown = "pending";
    void outcome.then((value) => {
      settled = value;
    });
    await vi.advanceTimersByTimeAsync(0);
    expect(settled).toBe("pending");

    await vi.advanceTimersByTimeAsync(2_000);
    await expect(outcome).resolves.toBe("expired");
  });

  it("treats the cancel button as a refusal", async () => {
    const dialog = mount(TransferConfirmDialog, { pluginName: "datax" }) as ConfirmInstance;
    const outcome = dialog.request(fixture({ level: "l0" }));
    await flush();

    button("common.cancel").click();
    await flush();
    await expect(outcome).resolves.toBe("denied");
  });
});

describe("transfer grant (L3)", () => {
  it("leaves destructive operations unticked so a grant cannot widen by one click", async () => {
    const dialog = mount(TransferGrantDialog, {}) as GrantInstance;
    const answer = dialog.request(fixture({ level: "l2", operations: ["insert", "drop"] }), { allowAlways: false });
    await flush();

    const boxes = [...document.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')];
    expect(boxes.map((candidate) => candidate.checked)).toEqual([true, false]);

    const once = button("pluginPlatform.transferGrant.once");
    expect(once.disabled).toBe(true);
    // L2 runs are re-confirmed every time, so the standing-grant button must not exist.
    expect(() => button("pluginPlatform.transferGrant.always")).toThrow();

    check(boxes[1], true);
    await flush();
    expect(once.disabled).toBe(false);

    once.click();
    await flush();
    await expect(answer).resolves.toEqual({ decision: "once", operations: ["insert", "drop"] });
  });

  it("offers 始终允许 for L0/L1 and reports the refusal explicitly", async () => {
    const dialog = mount(TransferGrantDialog, {}) as GrantInstance;
    const answer = dialog.request(fixture({ level: "l1", operations: ["insert"] }), { allowAlways: true });
    await flush();

    button("pluginPlatform.transferGrant.always").click();
    await flush();
    await expect(answer).resolves.toEqual({ decision: "always", operations: ["insert"] });

    const refused = dialog.request(fixture({ level: "l1", operations: ["insert"] }), { allowAlways: true });
    await flush();
    button("pluginPlatform.transferGrant.deny").click();
    await flush();
    await expect(refused).resolves.toEqual({ decision: "denied" });
  });
});
