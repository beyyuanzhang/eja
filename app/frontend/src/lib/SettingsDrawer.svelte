<script lang="ts">
  import {
    AlertCircle,
    Cable,
    Check,
    Copy,
    Cpu,
    FolderGit2,
    Gauge,
    LoaderCircle,
    Settings2,
    X,
  } from "lucide-svelte";
  import { fade, fly } from "svelte/transition";
  import ConnectorSettings from "./ConnectorSettings.svelte";
  import ModelSettings from "./ModelSettings.svelte";
  import NumberField from "./NumberField.svelte";
  import type {
    ConnectorDraft,
    ConnectorView,
    ProviderDraft,
    RoleBinding,
    SettingsView,
  } from "./types";

  type Section = "model" | "connectors" | "runtime" | "workspace";

  interface Props {
    open: boolean;
    settings: SettingsView | null;
    loading: boolean;
    loadError: string;
    onClose: () => void;
    onSave: (body: unknown) => Promise<boolean>;
  }

  let { open, settings, loading, loadError, onClose, onSave }: Props = $props();

  let section = $state<Section>("model");
  let providers = $state<ProviderDraft[]>([]);
  let roles = $state<{ eja: RoleBinding; aja: RoleBinding }>({
    eja: { providerId: "", model: "" },
    aja: { providerId: "", model: "" },
  });
  let connectors = $state<ConnectorDraft[]>([]);
  let slots = $state(1);
  let turnTimeoutMs = $state(0);
  let ajaSteps = $state(0);
  let ajaTimeoutMs = $state(0);
  let saving = $state(false);
  let saved = $state(false);
  let dirty = $state(false);
  let copiedPath = $state("");

  const sections = [
    { id: "model" as const, label: "模型", Icon: Cpu },
    { id: "connectors" as const, label: "连接器", Icon: Cable },
    { id: "runtime" as const, label: "运行", Icon: Gauge },
    { id: "workspace" as const, label: "工作区", Icon: FolderGit2 },
  ];

  const heading = $derived(sections.find((item) => item.id === section)?.label || "");
  const HeadingIcon = $derived(sections.find((item) => item.id === section)?.Icon || Settings2);

  function connectorDraft(value: ConnectorView): ConnectorDraft {
    return {
      name: value.name,
      mode: value.mode,
      command: value.command,
      url: value.url,
      args: [...value.args],
      cwd: value.cwd,
      env: value.env.map((item) => ({ ...item })),
      headers: value.headers.map((item) => ({ ...item })),
      enabled: value.enabled,
      timeoutMs: value.timeoutMs,
    };
  }

  function hydrate(value: SettingsView | null) {
    if (!value) return;
    providers = value.providers.map((provider) => ({
      ...provider,
      apiKey: "",
      clearApiKey: false,
      models: [...provider.models],
      headers: provider.headers.map((item) => ({ ...item })),
    }));
    roles = {
      eja: { ...value.roles.eja },
      aja: { ...value.roles.aja },
    };
    connectors = value.connectors.servers.map(connectorDraft);
    slots = value.limits.slots;
    turnTimeoutMs = value.limits.turnTimeoutMs;
    ajaSteps = value.limits.ajaSteps;
    ajaTimeoutMs = value.limits.ajaTimeoutMs;
    dirty = false;
  }

  function markDirty() {
    dirty = true;
    saved = false;
  }

  function serializeConnectors() {
    return connectors.map((connector) => ({
      ...connector,
      env: connector.env.map((item) => ({ ...item })),
      headers: connector.headers.map((item) => ({ ...item })),
    }));
  }

  async function saveAll() {
    if (!dirty || saving) return;
    saving = true;
    const ok = await onSave({
      providers: providers.map((provider) => ({
        id: provider.id,
        name: provider.name.trim(),
        baseUrl: provider.baseUrl.trim(),
        models: [...provider.models],
        headers: provider.headers.map((item) => ({ ...item })),
        ...(provider.apiKey.trim() ? { apiKey: provider.apiKey } : {}),
        ...(provider.clearApiKey ? { clearApiKey: true } : {}),
      })),
      roles,
      connectors: { servers: serializeConnectors() },
      limits: { slots, turnTimeoutMs, ajaSteps, ajaTimeoutMs },
    });
    saving = false;
    if (ok) {
      dirty = false;
      saved = true;
      setTimeout(() => (saved = false), 1800);
    }
  }

  async function copyPath(value: string, key: string) {
    try {
      await navigator.clipboard.writeText(value);
      copiedPath = key;
      setTimeout(() => {
        if (copiedPath === key) copiedPath = "";
      }, 1400);
    } catch {
      copiedPath = "";
    }
  }

  $effect(() => {
    hydrate(settings);
  });
</script>

{#if open}
  <div class="settings-layer">
    <button
      class="settings-backdrop"
      type="button"
      aria-label="关闭设置"
      onclick={onClose}
      transition:fade={{ duration: 150 }}
    ></button>
    <aside class="settings" aria-label="设置" transition:fly={{ x: -30, duration: 230, opacity: 1 }}>
      <header class="settings-head">
        <h2>设置</h2>
        <button class="icon-button" type="button" aria-label="关闭设置" onclick={onClose}>
          <X size={18} />
        </button>
      </header>

      <div class="settings-body">
        <nav aria-label="设置分类">
          {#each sections as item}
            <button class:active={section === item.id} type="button" onclick={() => (section = item.id)}>
              <span class="nav-icon"><item.Icon size={16} strokeWidth={1.65} /></span>
              <span>{item.label}</span>
            </button>
          {/each}
        </nav>

        <div class="settings-content">
          <div class="content-head">
            <HeadingIcon size={17} strokeWidth={1.65} />
            <h3>{heading}</h3>
          </div>

          {#if loading && !settings}
            <div class="settings-loading">
              <LoaderCircle size={18} class="spin" />
            </div>
          {:else if loadError && !settings}
            <div class="inline-error"><AlertCircle size={16} />{loadError}</div>
          {:else if section === "model"}
            <ModelSettings bind:providers bind:roles onDirty={markDirty} />
          {:else if section === "connectors"}
            <ConnectorSettings bind:connectors {settings} onDirty={markDirty} />
          {:else if section === "runtime"}
            <div class="runtime-page">
              <NumberField
                bind:value={slots}
                label="并行槽位"
                min={1}
                max={8}
                step={1}
                onchange={markDirty}
              />
              <NumberField
                bind:value={turnTimeoutMs}
                label="回合超时"
                min={0}
                max={7_200_000}
                step={60_000}
                suffix="ms"
                onchange={markDirty}
              />
              <NumberField
                bind:value={ajaSteps}
                label="AJA 步数"
                min={0}
                max={200}
                step={1}
                onchange={markDirty}
              />
              <NumberField
                bind:value={ajaTimeoutMs}
                label="AJA 超时"
                min={0}
                max={14_400_000}
                step={60_000}
                suffix="ms"
                onchange={markDirty}
              />
            </div>
          {:else}
            <div class="workspace-page">
              <div class="path-list">
                <div class="path-row">
                  <span class="path-kind">runtime</span>
                  <code>{settings?.runtimePath}</code>
                  <button
                    type="button"
                    aria-label="复制运行时路径"
                    onclick={() => copyPath(settings?.runtimePath || "", "runtime")}
                  >
                    {#if copiedPath === "runtime"}<Check size={13} />{:else}<Copy size={13} />{/if}
                  </button>
                </div>
                <div class="path-row">
                  <span class="path-kind">state</span>
                  <code>{settings?.stateDir}</code>
                  <button
                    type="button"
                    aria-label="复制状态目录"
                    onclick={() => copyPath(settings?.stateDir || "", "state")}
                  >
                    {#if copiedPath === "state"}<Check size={13} />{:else}<Copy size={13} />{/if}
                  </button>
                </div>
                <div class="path-row">
                  <span class="path-kind">project</span>
                  <code>{settings?.projectDir}</code>
                  <button
                    type="button"
                    aria-label="复制项目目录"
                    onclick={() => copyPath(settings?.projectDir || "", "project")}
                  >
                    {#if copiedPath === "project"}<Check size={13} />{:else}<Copy size={13} />{/if}
                  </button>
                </div>
              </div>
            </div>
          {/if}
        </div>
      </div>

      <footer class="settings-foot">
        <span class="save-state">
          {#if saving}
            <LoaderCircle size={12} class="spin" />
          {:else if saved}
            <Check size={12} />
          {:else if dirty}
            <i></i>
          {/if}
        </span>
        <button class="primary-button" type="button" disabled={!dirty || saving || loading} onclick={saveAll}>
          保存设置
        </button>
      </footer>
    </aside>
  </div>
{/if}

<style>
  .settings-layer {
    position: fixed;
    inset: 0;
    z-index: 70;
  }

  .settings-backdrop {
    position: absolute;
    inset: 0;
    background: rgba(36, 35, 31, 0.2);
    backdrop-filter: blur(3px);
  }

  .settings {
    position: relative;
    display: flex;
    width: min(730px, 94vw);
    height: 100%;
    flex-direction: column;
    overflow: hidden;
    border-right: 1px solid rgba(36, 35, 31, 0.11);
    background: #fbfaf6;
    box-shadow: 28px 0 90px rgba(36, 35, 31, 0.13);
  }

  .settings-head {
    display: flex;
    height: 66px;
    flex: 0 0 66px;
    align-items: center;
    justify-content: space-between;
    padding: 0 18px 0 24px;
    border-bottom: 1px solid var(--line);
  }

  h2 {
    margin: 0;
    font-family: var(--serif);
    font-size: 23px;
    font-weight: 480;
    letter-spacing: -0.025em;
  }

  .settings-body {
    display: grid;
    flex: 1;
    min-height: 0;
    grid-template-columns: 164px minmax(0, 1fr);
  }

  nav {
    display: flex;
    min-height: 0;
    flex-direction: column;
    gap: 2px;
    padding: 13px 10px;
    border-right: 1px solid var(--line);
    background: color-mix(in srgb, var(--paper) 72%, white);
  }

  nav button {
    display: flex;
    min-height: 39px;
    align-items: center;
    gap: 9px;
    padding: 5px 10px;
    border-radius: 9px;
    color: var(--muted);
    font-size: 10.5px;
    text-align: left;
    transition:
      color 150ms ease,
      background 150ms ease;
  }

  nav button:hover {
    background: rgba(255, 254, 250, 0.6);
    color: var(--ink-soft);
  }

  nav button.active {
    background: var(--surface);
    color: var(--ink);
    box-shadow: 0 1px 4px rgba(36, 35, 31, 0.04), 0 0 0 1px var(--line) inset;
  }

  .nav-icon {
    display: grid;
    width: 23px;
    place-items: center;
    color: var(--muted);
  }

  nav button.active .nav-icon {
    color: var(--accent-deep);
  }

  .settings-content {
    min-height: 0;
    overflow-y: auto;
    padding: 24px 28px 32px;
  }

  .content-head {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 18px;
    color: var(--accent-deep);
  }

  h3 {
    margin: 0;
    color: var(--ink);
    font-family: var(--serif);
    font-size: 18px;
    font-weight: 500;
    letter-spacing: -0.015em;
  }

  .settings-loading {
    display: flex;
    min-height: 160px;
    align-items: center;
    justify-content: center;
    color: var(--muted);
  }

  .inline-error {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 11px 12px;
    border-radius: 9px;
    background: var(--danger-wash);
    color: var(--danger);
    font-size: 10px;
  }

  .settings-foot {
    display: flex;
    height: 62px;
    flex: 0 0 62px;
    align-items: center;
    justify-content: space-between;
    padding: 0 18px 0 24px;
    border-top: 1px solid var(--line);
    background: rgba(251, 250, 246, 0.95);
    backdrop-filter: blur(12px);
  }

  .save-state {
    display: inline-flex;
    width: 18px;
    height: 18px;
    align-items: center;
    justify-content: center;
    color: var(--faint);
  }

  .save-state i {
    width: 5px;
    height: 5px;
    border-radius: 50%;
    background: var(--accent);
  }

  .runtime-page,
  .workspace-page {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .path-list {
    overflow: hidden;
    margin-bottom: 4px;
    border: 1px solid var(--line);
    border-radius: 11px;
    background: rgba(255, 254, 250, 0.62);
  }

  .path-row {
    display: grid;
    min-height: 53px;
    grid-template-columns: 56px minmax(0, 1fr) 28px;
    align-items: center;
    gap: 9px;
    padding: 7px 9px 7px 13px;
  }

  .path-row + .path-row {
    border-top: 1px solid var(--line);
  }

  .path-kind {
    color: var(--faint);
    font-family: var(--mono);
    font-size: 8px;
  }

  .path-row code {
    overflow: hidden;
    color: var(--muted);
    font-family: var(--mono);
    font-size: 8.5px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .path-row button {
    display: grid;
    width: 27px;
    height: 27px;
    place-items: center;
    border-radius: 7px;
    color: var(--faint);
  }

  .path-row button:hover {
    background: var(--surface-2);
    color: var(--ink);
  }

  :global(.spin) {
    animation: spin 0.9s linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  @media (max-width: 720px) {
    .settings {
      width: 100%;
    }

    .settings-head {
      height: 60px;
      flex-basis: 60px;
      padding-inline: 17px;
    }

    .settings-body {
      grid-template-columns: 1fr;
      grid-template-rows: auto minmax(0, 1fr);
    }

    nav {
      display: grid;
      grid-template-columns: repeat(4, 1fr);
      gap: 2px;
      padding: 7px 9px;
      border-right: 0;
      border-bottom: 1px solid var(--line);
      background: var(--paper);
    }

    nav button {
      min-height: 42px;
      justify-content: center;
      flex-direction: column;
      gap: 2px;
      padding: 4px;
      font-size: 9px;
      text-align: center;
    }

    .nav-icon {
      height: 18px;
    }

    .settings-content {
      padding: 20px 16px 28px;
    }

    .content-head {
      margin-bottom: 15px;
    }

    .settings-foot {
      height: 60px;
      flex-basis: 60px;
      padding: 0 16px;
      padding-bottom: env(safe-area-inset-bottom);
    }
  }
</style>
