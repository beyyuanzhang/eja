<script lang="ts">
  import {
    AlertCircle,
    ChevronRight,
    FolderGit2,
    Globe2,
    Plus,
    TerminalSquare,
  } from "lucide-svelte";
  import ConnectorEditor from "./ConnectorEditor.svelte";
  import type { ConnectorDraft, SettingsView } from "./types";

  interface Props {
    connectors: ConnectorDraft[];
    settings: SettingsView | null;
    onDirty: () => void;
  }

  let { connectors = $bindable(), settings, onDirty }: Props = $props();

  let editor = $state<ConnectorDraft | null>(null);
  let editingIndex = $state<number | null>(null);

  function cloneConnector(connector: ConnectorDraft): ConnectorDraft {
    return {
      ...connector,
      args: [...connector.args],
      env: connector.env.map((item) => ({ ...item })),
      headers: connector.headers.map((item) => ({ ...item })),
    };
  }

  function blank(): ConnectorDraft {
    return {
      name: "",
      builtin: false,
      mode: "command",
      command: "",
      url: "",
      args: [],
      cwd: "",
      env: [],
      headers: [],
      enabled: true,
      timeoutMs: 30000,
    };
  }

  function add() {
    editingIndex = null;
    editor = blank();
  }

  function edit(connector: ConnectorDraft, index: number) {
    editingIndex = index;
    editor = cloneConnector(connector);
  }

  function save(next: ConnectorDraft) {
    const copy = [...connectors];
    if (editingIndex === null) copy.push(next);
    else copy[editingIndex] = next;
    connectors = copy;
    editor = null;
    editingIndex = null;
    onDirty();
  }

  function remove(index: number) {
    connectors = connectors.filter((_, itemIndex) => itemIndex !== index);
    editor = null;
    editingIndex = null;
    onDirty();
  }

  function endpoint(connector: ConnectorDraft) {
    if (connector.mode === "url") return connector.url;
    return [connector.command, ...connector.args].filter(Boolean).join(" ");
  }
</script>

<div class="connector-settings">
  {#if connectors.length}
    <div class="connector-tools">
      <button type="button" aria-label="添加连接器" title="添加连接器" onclick={add}>
        <Plus size={15} />
      </button>
    </div>
  {/if}

  {#if settings?.connectors.error}
    <div class="connector-error">
      <AlertCircle size={14} />
      <span>{settings.connectors.error}</span>
    </div>
  {/if}

  {#if connectors.length}
    <div class="connector-list">
      {#each connectors as connector, index (connector.name)}
        <article class:disabled={!connector.enabled}>
          {#if connector.builtin}
            <div class="connector-main builtin">
              <span class="connector-mark">
                <FolderGit2 size={17} />
              </span>
              <span class="connector-copy">
                <strong>工作区文件</strong>
              </span>
              <label class="enabled-toggle" title={connector.enabled ? "停用" : "启用"}>
                <input
                  type="checkbox"
                  bind:checked={connector.enabled}
                  aria-label={connector.enabled ? "停用工作区文件" : "启用工作区文件"}
                  onchange={onDirty}
                />
                <span></span>
              </label>
            </div>
          {:else}
            <button class="connector-main" type="button" onclick={() => edit(connector, index)}>
              <span class="connector-mark" class:remote={connector.mode === "url"}>
                {#if connector.mode === "url"}<Globe2 size={17} />{:else}<TerminalSquare size={17} />{/if}
              </span>
              <span class="connector-copy">
                <strong>{connector.name}</strong>
                <code title={endpoint(connector)}>{endpoint(connector)}</code>
              </span>
              <ChevronRight size={14} />
            </button>
          {/if}
        </article>
      {/each}
    </div>
  {:else}
    <button class="empty-add" type="button" aria-label="添加连接器" onclick={add}>
      <Plus size={18} />
    </button>
  {/if}
</div>

{#if editor}
  <ConnectorEditor
    connector={editor}
    existingNames={connectors
      .filter((_, index) => index !== editingIndex)
      .map((item) => item.name)}
    onSave={save}
    onDelete={editingIndex === null ? undefined : () => remove(editingIndex!)}
    onClose={() => {
      editor = null;
      editingIndex = null;
    }}
  />
{/if}

<style>
  .connector-settings {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .connector-tools {
    display: flex;
    justify-content: flex-end;
    margin-bottom: -2px;
  }

  .connector-tools button {
    display: grid;
    width: 32px;
    height: 32px;
    place-items: center;
    border: 1px solid var(--line);
    border-radius: 999px;
    background: var(--surface);
    color: var(--muted);
    box-shadow: var(--shadow-xs);
  }

  .connector-tools button:hover {
    border-color: var(--line-strong);
    color: var(--ink);
  }

  .connector-error {
    display: flex;
    align-items: flex-start;
    gap: 7px;
    padding: 10px 11px;
    border-radius: 9px;
    background: var(--danger-wash);
    color: var(--danger);
    font-size: 9px;
    line-height: 1.45;
  }

  .connector-error :global(svg) {
    flex: 0 0 auto;
    margin-top: 1px;
  }

  .connector-list {
    display: flex;
    flex-direction: column;
    gap: 7px;
  }

  article {
    overflow: hidden;
    border: 1px solid var(--line);
    border-radius: 12px;
    background: rgba(255, 254, 250, 0.68);
    box-shadow: 0 1px 0 rgba(255, 255, 255, 0.65) inset;
    transition:
      border-color 150ms ease,
      background 150ms ease;
  }

  article:hover {
    border-color: var(--line-strong);
    background: var(--surface);
  }

  article.disabled {
    opacity: 0.62;
  }

  .connector-main {
    display: grid;
    width: 100%;
    min-height: 62px;
    grid-template-columns: 36px minmax(0, 1fr) 16px;
    align-items: center;
    gap: 10px;
    padding: 9px 9px 9px 11px;
    text-align: left;
  }

  .connector-mark {
    display: grid;
    width: 36px;
    height: 36px;
    place-items: center;
    border-radius: 10px;
    background: var(--accent-wash);
    color: var(--accent-deep);
  }

  .connector-mark.remote {
    background: var(--amber-wash);
    color: var(--amber);
  }

  .connector-copy {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 2px;
  }

  .connector-copy strong {
    overflow: hidden;
    color: var(--ink);
    font-size: 12px;
    font-weight: 560;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .connector-copy code {
    overflow: hidden;
    color: var(--faint);
    font-family: var(--mono);
    font-size: 8.5px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .connector-main > :global(svg) {
    color: var(--faint);
  }

  .connector-main.builtin {
    grid-template-columns: 36px minmax(0, 1fr) 34px;
  }

  .enabled-toggle {
    position: relative;
    display: flex;
    width: 34px;
    height: 22px;
    cursor: pointer;
  }

  .enabled-toggle input {
    position: absolute;
    opacity: 0;
    pointer-events: none;
  }

  .enabled-toggle span {
    width: 31px;
    height: 18px;
    margin: auto;
    border-radius: 999px;
    background: var(--surface-3);
    transition: background 150ms ease;
  }

  .enabled-toggle span::after {
    display: block;
    width: 14px;
    height: 14px;
    margin: 2px;
    border-radius: 50%;
    background: #fff;
    box-shadow: 0 1px 3px rgba(36, 35, 31, 0.15);
    content: "";
    transition: transform 150ms ease;
  }

  .enabled-toggle input:checked + span {
    background: var(--accent);
  }

  .enabled-toggle input:checked + span::after {
    transform: translateX(13px);
  }

  .empty-add {
    display: grid;
    width: 100%;
    height: 92px;
    place-items: center;
    border: 1px dashed var(--line-strong);
    border-radius: 12px;
    color: var(--faint);
    transition:
      border-color 150ms ease,
      background 150ms ease,
      color 150ms ease;
  }

  .empty-add:hover {
    border-color: rgba(189, 96, 66, 0.35);
    background: rgba(246, 232, 225, 0.22);
    color: var(--accent-deep);
  }

</style>
