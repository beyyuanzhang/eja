<script lang="ts">
  import { ChevronDown, ChevronRight, Plus, Sparkles, TerminalSquare } from "lucide-svelte";
  import ProviderEditor from "./ProviderEditor.svelte";
  import type { ProviderDraft, RoleBinding } from "./types";

  interface Props {
    providers: ProviderDraft[];
    roles: { eja: RoleBinding; aja: RoleBinding };
    onDirty: () => void;
  }

  let { providers = $bindable(), roles = $bindable(), onDirty }: Props = $props();

  let editor = $state<ProviderDraft | null>(null);
  let editingIndex = $state<number | null>(null);

  function blankProvider(): ProviderDraft {
    return {
      id: `p_${crypto.randomUUID().replaceAll("-", "").slice(0, 12)}`,
      name: "",
      baseUrl: "",
      apiKey: "",
      models: [],
      hasApiKey: false,
      local: false,
      headers: [],
    };
  }

  function addProvider() {
    editingIndex = null;
    editor = blankProvider();
  }

  function editProvider(provider: ProviderDraft, index: number) {
    editingIndex = index;
    editor = {
      ...provider,
      models: [...provider.models],
      headers: provider.headers.map((item) => ({ ...item })),
    };
  }

  function saveProvider(next: ProviderDraft) {
    const copy = [...providers];
    if (editingIndex === null) copy.push(next);
    else copy[editingIndex] = next;
    providers = copy;
    editor = null;
    editingIndex = null;
    onDirty();
  }

  function removeProvider(index: number) {
    const removed = providers[index];
    providers = providers.filter((_, itemIndex) => itemIndex !== index);
    for (const role of ["eja", "aja"] as const) {
      if (roles[role].providerId === removed.id) {
        roles = { ...roles, [role]: { providerId: "", model: "" } };
      }
    }
    editor = null;
    editingIndex = null;
    onDirty();
  }

  function changeProvider(role: "eja" | "aja", providerId: string) {
    const provider = providers.find((item) => item.id === providerId);
    const current = roles[role];
    const model =
      provider && current.model && provider.models.includes(current.model)
        ? current.model
        : provider?.models.length === 1
          ? provider.models[0]
          : "";
    roles = { ...roles, [role]: { providerId, model } };
    onDirty();
  }

  function providerUrl(provider: ProviderDraft) {
    try {
      return new URL(provider.baseUrl).host;
    } catch {
      return provider.baseUrl;
    }
  }

  function modelsFor(role: "eja" | "aja") {
    return providers.find((provider) => provider.id === roles[role].providerId)?.models ?? [];
  }

  function modelOptions(role: "eja" | "aja") {
    const models = modelsFor(role);
    const selected = roles[role].model;
    return selected && !models.includes(selected) ? [selected, ...models] : models;
  }
</script>

<div class="model-settings">
  <section class="settings-section">
    <header class="section-head">
      <span>Provider</span>
      {#if providers.length}
        <button type="button" aria-label="添加 Provider" title="添加 Provider" onclick={addProvider}>
          <Plus size={14} />
        </button>
      {/if}
    </header>

    {#if providers.length}
      <div class="provider-list">
        {#each providers as provider, index (provider.id)}
          <button class="provider-row" type="button" onclick={() => editProvider(provider, index)}>
            <span class="provider-mark" class:local={provider.local}>
              {provider.name.trim().slice(0, 1).toUpperCase() || "P"}
            </span>
            <span class="provider-copy">
              <strong>{provider.name}</strong>
              <code>{providerUrl(provider)}</code>
            </span>
            <ChevronRight size={14} />
          </button>
        {/each}
      </div>
    {:else}
      <button class="empty-provider" type="button" aria-label="添加 Provider" onclick={addProvider}>
        <Plus size={17} />
      </button>
    {/if}
  </section>

  <section class="settings-section">
    <div class="role-list">
      <div class="role-row">
        <span class="role-mark"><Sparkles size={15} /></span>
        <span class="role-name">eja</span>
        <span class="role-control">
          <span class="provider-select">
            <select
              value={roles.eja.providerId}
              aria-label="eja 的 Provider"
              onchange={(event) => changeProvider("eja", event.currentTarget.value)}
            >
              <option value="">Provider</option>
              {#each providers as provider (provider.id)}
                <option value={provider.id}>{provider.name}</option>
              {/each}
            </select>
            <ChevronDown size={12} />
          </span>
          <span class="provider-select">
            <select
              bind:value={roles.eja.model}
              disabled={!roles.eja.providerId}
              aria-label="eja 的模型"
              onchange={onDirty}
            >
              <option value="">模型</option>
              {#each modelOptions("eja") as model (model)}
                <option value={model}>{model}</option>
              {/each}
            </select>
            <ChevronDown size={12} />
          </span>
        </span>
      </div>

      <div class="role-row">
        <span class="role-mark aja"><TerminalSquare size={15} /></span>
        <span class="role-name">AJA</span>
        <span class="role-control">
          <span class="provider-select">
            <select
              value={roles.aja.providerId}
              aria-label="AJA 的 Provider"
              onchange={(event) => changeProvider("aja", event.currentTarget.value)}
            >
              <option value="">跟随 eja</option>
              {#each providers as provider (provider.id)}
                <option value={provider.id}>{provider.name}</option>
              {/each}
            </select>
            <ChevronDown size={12} />
          </span>
          <span class="provider-select">
            <select
              bind:value={roles.aja.model}
              disabled={!roles.aja.providerId}
              aria-label="AJA 的模型"
              onchange={onDirty}
            >
              <option value="">{roles.aja.providerId ? "模型" : "跟随 eja"}</option>
              {#each modelOptions("aja") as model (model)}
                <option value={model}>{model}</option>
              {/each}
            </select>
            <ChevronDown size={12} />
          </span>
        </span>
      </div>
    </div>
  </section>
</div>

{#if editor}
  <ProviderEditor
    provider={editor}
    existingNames={providers
      .filter((_, index) => index !== editingIndex)
      .map((item) => item.name)}
    onSave={saveProvider}
    onDelete={editingIndex === null ? undefined : () => removeProvider(editingIndex!)}
    onClose={() => {
      editor = null;
      editingIndex = null;
    }}
  />
{/if}

<style>
  .model-settings {
    display: flex;
    flex-direction: column;
    gap: 24px;
  }

  .settings-section {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .section-head {
    display: flex;
    height: 25px;
    align-items: center;
    justify-content: space-between;
    padding: 0 2px;
    color: var(--muted);
    font-size: 10px;
    font-weight: 560;
    letter-spacing: 0.02em;
  }

  .section-head button {
    display: grid;
    width: 25px;
    height: 25px;
    place-items: center;
    border-radius: 999px;
    color: var(--faint);
  }

  .section-head button:hover {
    background: var(--surface-2);
    color: var(--ink);
  }

  .provider-list {
    overflow: hidden;
    border: 1px solid var(--line);
    border-radius: 12px;
    background: rgba(255, 254, 250, 0.68);
  }

  .provider-row {
    display: grid;
    width: 100%;
    min-height: 58px;
    grid-template-columns: 34px minmax(0, 1fr) 18px;
    align-items: center;
    gap: 10px;
    padding: 9px 10px 9px 12px;
    text-align: left;
  }

  .provider-row + .provider-row {
    border-top: 1px solid var(--line);
  }

  .provider-row:hover {
    background: rgba(243, 241, 235, 0.58);
  }

  .provider-mark {
    display: grid;
    width: 34px;
    height: 34px;
    place-items: center;
    border-radius: 10px;
    background: var(--accent-wash);
    color: var(--accent-deep);
    font-family: var(--serif);
    font-size: 14px;
    font-weight: 600;
  }

  .provider-mark.local {
    background: var(--amber-wash);
    color: var(--amber);
  }

  .provider-copy {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 2px;
  }

  .provider-copy strong {
    overflow: hidden;
    color: var(--ink);
    font-size: 11.5px;
    font-weight: 560;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .provider-copy code {
    overflow: hidden;
    color: var(--faint);
    font-family: var(--mono);
    font-size: 8.5px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .provider-row > :global(svg) {
    color: var(--faint);
  }

  .empty-provider {
    display: grid;
    width: 100%;
    height: 74px;
    place-items: center;
    border: 1px dashed var(--line-strong);
    border-radius: 12px;
    color: var(--faint);
  }

  .empty-provider:hover {
    border-color: rgba(189, 96, 66, 0.35);
    background: rgba(246, 232, 225, 0.22);
    color: var(--accent-deep);
  }

  .role-list {
    overflow: hidden;
    border: 1px solid var(--line);
    border-radius: 12px;
    background: rgba(255, 254, 250, 0.68);
  }

  .role-row {
    display: grid;
    min-height: 74px;
    grid-template-columns: 32px 45px minmax(0, 1fr);
    align-items: center;
    gap: 8px;
    padding: 10px 11px;
  }

  .role-row + .role-row {
    border-top: 1px solid var(--line);
  }

  .role-mark {
    display: grid;
    width: 30px;
    height: 30px;
    place-items: center;
    border-radius: 9px;
    background: var(--accent-wash);
    color: var(--accent-deep);
  }

  .role-mark.aja {
    background: var(--amber-wash);
    color: var(--amber);
  }

  .role-name {
    color: var(--ink-soft);
    font-family: var(--serif);
    font-size: 13px;
    font-weight: 580;
  }

  .role-control {
    display: grid;
    min-width: 0;
    grid-template-columns: minmax(120px, 0.62fr) minmax(170px, 1.38fr);
    gap: 7px;
  }

  .provider-select {
    position: relative;
    display: flex;
    min-width: 0;
    height: 38px;
    align-items: center;
    border: 1px solid var(--line);
    border-radius: 9px;
    background: var(--surface);
  }

  .provider-select:focus-within {
    border-color: rgba(189, 96, 66, 0.34);
    box-shadow: 0 0 0 3px rgba(189, 96, 66, 0.055);
  }

  .provider-select select {
    min-width: 0;
    height: 100%;
    flex: 1;
    border: 0;
    outline: 0;
    appearance: none;
    background: transparent;
    padding: 0 28px 0 10px;
    color: var(--ink-soft);
    font-size: 9.5px;
    text-overflow: ellipsis;
  }

  .provider-select select:disabled {
    color: var(--faint);
    opacity: 1;
  }

  .provider-select :global(svg) {
    position: absolute;
    right: 9px;
    color: var(--faint);
    pointer-events: none;
  }

  @media (max-width: 720px) {
    .role-row {
      grid-template-columns: 30px 42px minmax(0, 1fr);
    }

    .role-control {
      grid-template-columns: 1fr;
      gap: 5px;
    }

    .role-row {
      min-height: 102px;
    }
  }
</style>
