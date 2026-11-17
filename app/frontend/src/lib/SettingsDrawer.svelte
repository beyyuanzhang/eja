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
    Plus,
    Settings2,
    Trash2,
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

  interface WorkspaceDraft {
    name: string;
    path: string;
  }

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
  let workspaces = $state<WorkspaceDraft[]>([]);
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
  const paths = $derived([
    {
      key: "runtime",
      label: "runtime",
      value: settings?.runtimePath ?? "",
      ariaLabel: "复制运行时路径",
    },
    {
      key: "state",
      label: "state",
      value: settings?.stateDir ?? "",
      ariaLabel: "复制状态目录",
    },
    {
      key: "project",
      label: "project",
      value: settings?.projectDir ?? "",
      ariaLabel: "复制项目目录",
    },
  ]);

  function connectorDraft(value: ConnectorView): ConnectorDraft {
    return {
      name: value.name,
      builtin: value.builtin,
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
    workspaces = Object.entries(value.workspaces).map(([name, path]) => ({
      name,
      path,
    }));
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
    return connectors
      .filter((connector) => !connector.builtin)
      .map((connector) => ({
        ...connector,
        env: connector.env.map((item) => ({ ...item })),
        headers: connector.headers.map((item) => ({ ...item })),
      }));
  }

  function serializeWorkspaces() {
    const entries: [string, string][] = [];
    for (const item of workspaces) {
      const name = item.name.trim();
      const path = item.path.trim();
      if (!name && !path) continue;
      entries.push([name, path]);
    }
    return Object.fromEntries(entries);
  }

  function addWorkspace() {
    workspaces = [...workspaces, { name: "", path: "" }];
    markDirty();
  }

  function removeWorkspace(index: number) {
    workspaces = workspaces.filter((_, itemIndex) => itemIndex !== index);
    markDirty();
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
      connectors: {
        servers: serializeConnectors(),
        workspaceTools: connectors.find((connector) => connector.builtin)?.enabled ?? true,
      },
      limits: { slots, turnTimeoutMs, ajaSteps, ajaTimeoutMs },
      workspaces: serializeWorkspaces(),
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
                {#each paths as path (path.key)}
                  <div class="path-row">
                    <span class="path-kind">{path.label}</span>
                    <code>{path.value}</code>
                    <button
                      type="button"
                      aria-label={path.ariaLabel}
                      onclick={() => copyPath(path.value, path.key)}
                    >
                      {#if copiedPath === path.key}<Check size={13} />{:else}<Copy size={13} />{/if}
                    </button>
                  </div>
                {/each}
              </div>

              <div class="ws-list">
                <div class="ws-row">
                  <code class="ws-name">default</code>
                  <code class="ws-path">{settings?.projectDir ?? ""}</code>
                  <span class="ws-slot"></span>
                </div>
                {#each workspaces as item, index}
                  <div class="ws-row">
                    <input
                      class="ws-name"
                      bind:value={item.name}
                      oninput={markDirty}
                      type="text"
                      placeholder="name"
                      autocomplete="off"
                      spellcheck="false"
                      aria-label="工作区名"
                    />
                    <input
                      class="ws-path"
                      bind:value={item.path}
                      oninput={markDirty}
                      type="text"
                      placeholder="/path/to/dir"
                      autocomplete="off"
                      spellcheck="false"
                      aria-label="工作区路径"
                    />
                    <button
                      class="ws-remove"
                      type="button"
                      aria-label="移除工作区"
                      onclick={() => removeWorkspace(index)}
                    >
                      <Trash2 size={13} />
                    </button>
                  </div>
                {/each}

                <button class="ws-add" type="button" onclick={addWorkspace}>
                  <Plus size={13} />
                  添加工作区
                </button>
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

  .ws-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .ws-row {
    display: grid;
    grid-template-columns: minmax(110px, 0.6fr) minmax(170px, 1.4fr) 30px;
    gap: 6px;
    align-items: center;
  }

  .ws-row input {
    width: 100%;
    height: 36px;
    border: 1px solid var(--line);
    border-radius: 8px;
    outline: 0;
    background: var(--surface);
    padding: 0 9px;
    color: var(--ink);
    font-family: var(--mono);
    font-size: 9px;
  }

  .ws-row input:focus {
    border-color: rgba(189, 96, 66, 0.34);
    box-shadow: 0 0 0 3px rgba(189, 96, 66, 0.055);
  }

  .ws-row input::placeholder {
    color: var(--faint);
  }

  .ws-row code {
    overflow: hidden;
    padding: 0 9px;
    color: var(--muted);
    font-family: var(--mono);
    font-size: 9px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .ws-slot {
    width: 30px;
    height: 30px;
  }

  .ws-remove {
    display: grid;
    width: 30px;
    height: 30px;
    flex: 0 0 30px;
    place-items: center;
    border-radius: 7px;
    color: var(--faint);
  }

  .ws-remove:hover {
    background: var(--danger-wash);
    color: var(--danger);
  }

  .ws-add {
    display: inline-flex;
    height: 32px;
    align-items: center;
    align-self: flex-start;
    gap: 5px;
    margin-top: 1px;
    padding: 0 9px;
    border-radius: 8px;
    color: var(--muted);
    font-size: 9px;
  }

  .ws-add:hover {
    background: var(--surface-2);
    color: var(--ink-soft);
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

    .ws-row {
      grid-template-columns: minmax(0, 1fr) 30px;
    }

    .ws-row .ws-name {
      grid-column: 1;
    }

    .ws-row .ws-path {
      grid-column: 1;
    }

    .ws-remove,
    .ws-slot {
      grid-column: 2;
      grid-row: 1 / 3;
    }
  }
</style>
