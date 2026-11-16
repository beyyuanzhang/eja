<script lang="ts">
  import { untrack } from "svelte";
  import {
    AlertCircle,
    Check,
    ChevronDown,
    Clock3,
    Globe2,
    LoaderCircle,
    Play,
    TerminalSquare,
    Trash2,
    X,
  } from "lucide-svelte";
  import { testConnector } from "./api";
  import KeyValueEditor from "./KeyValueEditor.svelte";
  import TokenEditor from "./TokenEditor.svelte";
  import type { ConnectorDraft, ConnectorTestResult } from "./types";

  interface Props {
    connector: ConnectorDraft;
    existingNames: string[];
    onSave: (connector: ConnectorDraft) => void;
    onDelete?: () => void;
    onClose: () => void;
  }

  let { connector, existingNames, onSave, onDelete, onClose }: Props = $props();

  function cloneConnector(value: ConnectorDraft): ConnectorDraft {
    return {
      ...value,
      args: [...value.args],
      env: value.env.map((item) => ({ ...item })),
      headers: value.headers.map((item) => ({ ...item })),
    };
  }

  let draft = $state<ConnectorDraft>(cloneConnector(untrack(() => connector)));
  let testing = $state(false);
  let testResult = $state<ConnectorTestResult | null>(null);
  let attempted = $state(false);
  let deleteArmed = $state(false);

  const duplicate = $derived(
    draft.name.trim() !== connector.name.trim() && existingNames.includes(draft.name.trim()),
  );
  const validName = $derived(/^[A-Za-z0-9_-]{1,24}$/.test(draft.name.trim()));
  const validEndpoint = $derived(
    draft.mode === "command" ? Boolean(draft.command.trim()) : Boolean(draft.url.trim()),
  );
  const valid = $derived(validName && validEndpoint && !duplicate);

  function update() {
    testResult = null;
    attempted = false;
    deleteArmed = false;
  }

  function testNow() {
    if (testing) return;
    testing = true;
    testResult = null;
    void testConnector({
      ...draft,
      name: draft.name.trim() || "probe",
    })
      .then((result) => {
        testResult = result;
      })
      .catch((error) => {
        testResult = {
          ok: false,
          latencyMs: 0,
          error: error instanceof Error ? error.message : String(error),
        };
      })
      .finally(() => {
        testing = false;
      });
  }

  function submit(event: SubmitEvent) {
    event.preventDefault();
    attempted = true;
    if (!valid) return;
    onSave({
      ...draft,
      name: draft.name.trim(),
      command: draft.command.trim(),
      url: draft.url.trim(),
      cwd: draft.cwd.trim(),
    });
  }

  function remove() {
    if (!onDelete) return;
    if (!deleteArmed) {
      deleteArmed = true;
      return;
    }
    onDelete();
  }
</script>

<div class="editor-layer">
  <button class="editor-backdrop" type="button" aria-label="关闭" onclick={onClose}></button>
  <form class="editor" onsubmit={submit}>
    <header class="editor-head">
      <div class="editor-title">
        <span class="editor-mark" class:remote={draft.mode === "url"}>
          {#if draft.mode === "url"}<Globe2 size={17} />{:else}<TerminalSquare size={17} />{/if}
        </span>
        <h3>{draft.name || "连接器"}</h3>
      </div>
      <div class="head-actions">
        <label class="enabled-toggle" title={draft.enabled ? "停用" : "启用"}>
          <input
            type="checkbox"
            bind:checked={draft.enabled}
            aria-label={draft.enabled ? "停用连接器" : "启用连接器"}
            onchange={update}
          />
          <span></span>
        </label>
        <button class="icon-close" type="button" aria-label="关闭" onclick={onClose}>
          <X size={17} />
        </button>
      </div>
    </header>

    <div class="editor-body">
      <div class="mode-switch" role="radiogroup" aria-label="连接方式">
        <button
          type="button"
          class:active={draft.mode === "command"}
          role="radio"
          aria-checked={draft.mode === "command"}
          onclick={() => {
            draft.mode = "command";
            update();
          }}
        >
          <TerminalSquare size={14} />Local
        </button>
        <button
          type="button"
          class:active={draft.mode === "url"}
          role="radio"
          aria-checked={draft.mode === "url"}
          onclick={() => {
            draft.mode = "url";
            update();
          }}
        >
          <Globe2 size={14} />HTTP
        </button>
      </div>

      <label class="field">
        <span>名称</span>
        <input
          bind:value={draft.name}
          oninput={update}
          type="text"
          placeholder="linear"
          maxlength="24"
          autocomplete="off"
          spellcheck="false"
        />
      </label>
      {#if attempted && (!validName || duplicate)}
        <p class="field-error">
          {duplicate ? "名称已存在" : "使用 1–24 个字母、数字、下划线或短横线"}
        </p>
      {/if}

      {#if draft.mode === "command"}
        <label class="field">
          <span>命令</span>
          <input
            bind:value={draft.command}
            oninput={update}
            type="text"
            placeholder="/usr/local/bin/my-mcp-server"
            autocomplete="off"
            spellcheck="false"
          />
        </label>
        <div class="field-pair">
          <label class="field">
            <span>工作目录</span>
            <input
              bind:value={draft.cwd}
              oninput={update}
              type="text"
              placeholder=""
              autocomplete="off"
              spellcheck="false"
            />
          </label>
          <label class="field">
            <span>超时</span>
            <span class="select-wrap">
              <Clock3 size={13} />
              <select bind:value={draft.timeoutMs} onchange={update}>
                <option value={10000}>10s</option>
                <option value={15000}>15s</option>
                <option value={30000}>30s</option>
                <option value={60000}>1m</option>
                <option value={120000}>2m</option>
                <option value={300000}>5m</option>
              </select>
              <ChevronDown size={12} />
            </span>
          </label>
        </div>
        <div class="field-block">
          <span>参数</span>
          <TokenEditor bind:value={draft.args} placeholder="--stdio" onchange={update} />
        </div>
        <div class="field-block">
          <span>环境变量</span>
          <KeyValueEditor bind:items={draft.env} kind="env" onchange={update} />
        </div>
      {:else}
        <label class="field">
          <span>URL</span>
          <input
            bind:value={draft.url}
            oninput={update}
            type="url"
            placeholder="https://service.example/mcp"
            autocomplete="url"
            spellcheck="false"
          />
        </label>
        <label class="field short">
          <span>超时</span>
          <span class="select-wrap">
            <Clock3 size={13} />
            <select bind:value={draft.timeoutMs} onchange={update}>
              <option value={10000}>10s</option>
              <option value={15000}>15s</option>
              <option value={30000}>30s</option>
              <option value={60000}>1m</option>
              <option value={120000}>2m</option>
              <option value={300000}>5m</option>
            </select>
            <ChevronDown size={12} />
          </span>
        </label>
        <div class="field-block">
          <span>Headers</span>
          <KeyValueEditor bind:items={draft.headers} kind="headers" onchange={update} />
        </div>
      {/if}

      {#if testResult}
        <div class="test-result" class:failed={!testResult.ok}>
          {#if testResult.ok}
            <Check size={14} />
            <span>{testResult.count || 0} tools · {testResult.latencyMs} ms</span>
          {:else}
            <AlertCircle size={14} />
            <span>{testResult.error}</span>
          {/if}
        </div>
      {/if}
    </div>

    <footer class="editor-foot">
      {#if onDelete}
        <button
          class="delete-action"
          class:armed={deleteArmed}
          type="button"
          aria-label={deleteArmed ? "确认删除" : "删除连接器"}
          title={deleteArmed ? "确认删除" : "删除"}
          onclick={remove}
        >
          <Trash2 size={15} />
        </button>
      {:else}
        <span></span>
      {/if}
      <div class="foot-actions">
        <button class="test-action" type="button" disabled={testing || !validEndpoint} onclick={testNow}>
          {#if testing}
            <LoaderCircle size={13} class="spin" />
          {:else}
            <Play size={13} />
          {/if}
          测试
        </button>
        <button class="cancel-action" type="button" onclick={onClose}>取消</button>
        <button class="save-action" type="submit" disabled={!valid}>应用</button>
      </div>
    </footer>
  </form>
</div>

<style>
  .editor-layer {
    position: fixed;
    inset: 0;
    z-index: 90;
    display: grid;
    place-items: center;
    padding: 26px;
  }

  .editor-backdrop {
    position: absolute;
    inset: 0;
    background: rgba(36, 35, 31, 0.27);
    backdrop-filter: blur(7px);
  }

  .editor {
    position: relative;
    display: flex;
    width: min(650px, 94vw);
    max-height: min(820px, 92dvh);
    flex-direction: column;
    overflow: hidden;
    border: 1px solid var(--line-strong);
    border-radius: 20px;
    background: #fcfbf7;
    box-shadow: 0 32px 110px rgba(36, 35, 31, 0.24);
  }

  .editor-head {
    display: flex;
    min-height: 68px;
    flex: 0 0 68px;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    padding: 0 16px 0 20px;
    border-bottom: 1px solid var(--line);
  }

  .editor-title {
    display: flex;
    min-width: 0;
    align-items: center;
    gap: 10px;
  }

  .editor-mark {
    display: grid;
    width: 34px;
    height: 34px;
    flex: 0 0 34px;
    place-items: center;
    border-radius: 10px;
    background: var(--accent-wash);
    color: var(--accent-deep);
  }

  .editor-mark.remote {
    background: var(--amber-wash);
    color: var(--amber);
  }

  h3 {
    overflow: hidden;
    margin: 0;
    font-family: var(--serif);
    font-size: 20px;
    font-weight: 500;
    letter-spacing: -0.02em;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .head-actions,
  .foot-actions {
    display: flex;
    align-items: center;
    gap: 7px;
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

  .icon-close,
  .delete-action {
    display: grid;
    width: 32px;
    height: 32px;
    place-items: center;
    border-radius: 999px;
    color: var(--faint);
  }

  .icon-close:hover {
    background: var(--surface-2);
    color: var(--ink);
  }

  .delete-action:hover,
  .delete-action.armed {
    background: var(--danger-wash);
    color: var(--danger);
  }

  .editor-body {
    display: flex;
    flex: 1;
    flex-direction: column;
    gap: 13px;
    overflow-y: auto;
    padding: 20px 22px 24px;
  }

  .mode-switch {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 4px;
    margin-bottom: 3px;
    padding: 4px;
    border-radius: 11px;
    background: var(--surface-2);
  }

  .mode-switch button {
    display: flex;
    height: 34px;
    align-items: center;
    justify-content: center;
    gap: 6px;
    border-radius: 8px;
    color: var(--muted);
    font-size: 10px;
  }

  .mode-switch button.active {
    background: var(--surface);
    color: var(--ink);
    box-shadow: var(--shadow-xs);
  }

  .field,
  .field-block {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 6px;
  }

  .field > span:first-child,
  .field-block > span:first-child {
    color: var(--muted);
    font-size: 9.5px;
    font-weight: 540;
  }

  .field input,
  .select-wrap {
    width: 100%;
    height: 38px;
    border: 1px solid var(--line);
    border-radius: 9px;
    background: var(--surface);
  }

  .field input {
    min-width: 0;
    outline: 0;
    padding: 0 11px;
    color: var(--ink);
    font-family: var(--mono);
    font-size: 10px;
  }

  .field input:focus,
  .select-wrap:focus-within {
    border-color: rgba(189, 96, 66, 0.34);
    box-shadow: 0 0 0 3px rgba(189, 96, 66, 0.055);
  }

  .field input::placeholder {
    color: var(--faint);
    font-family: var(--sans);
  }

  .field-pair {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 120px;
    gap: 9px;
  }

  .field.short {
    width: 120px;
  }

  .select-wrap {
    position: relative;
    display: flex;
    align-items: center;
    padding-left: 10px;
    color: var(--faint);
  }

  .select-wrap select {
    min-width: 0;
    height: 100%;
    flex: 1;
    border: 0;
    outline: 0;
    appearance: none;
    background: transparent;
    padding: 0 26px 0 7px;
    color: var(--ink-soft);
    font-size: 9.5px;
  }

  .select-wrap > :global(svg:last-child) {
    position: absolute;
    right: 9px;
    pointer-events: none;
  }

  .field-error {
    margin: -7px 0 0;
    color: var(--danger);
    font-size: 9px;
  }

  .test-result {
    display: flex;
    align-items: flex-start;
    gap: 7px;
    margin-top: 2px;
    padding: 9px 10px;
    border-radius: 9px;
    background: var(--accent-wash);
    color: var(--accent-deep);
    font-size: 9.5px;
    line-height: 1.45;
  }

  .test-result.failed {
    background: var(--danger-wash);
    color: var(--danger);
  }

  .test-result :global(svg) {
    flex: 0 0 auto;
    margin-top: 1px;
  }

  .editor-foot {
    display: flex;
    min-height: 62px;
    flex: 0 0 62px;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    padding: 0 18px;
    border-top: 1px solid var(--line);
    background: rgba(251, 250, 246, 0.95);
  }

  .test-action,
  .cancel-action,
  .save-action {
    display: inline-flex;
    height: 32px;
    align-items: center;
    justify-content: center;
    gap: 5px;
    padding: 0 12px;
    border-radius: 999px;
    font-size: 10px;
  }

  .test-action,
  .cancel-action {
    color: var(--muted);
  }

  .test-action:hover:not(:disabled),
  .cancel-action:hover {
    background: var(--surface-2);
    color: var(--ink);
  }

  .test-action:disabled {
    opacity: 0.35;
  }

  .save-action {
    background: var(--ink);
    color: #fff;
    font-weight: 560;
  }

  .save-action:disabled {
    opacity: 0.4;
  }

  :global(.spin) {
    animation: spin 0.9s linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  @media (max-width: 620px) {
    .editor-layer {
      align-items: stretch;
      padding: 0;
    }

    .editor {
      width: 100%;
      max-height: none;
      border: 0;
      border-radius: 0;
    }

    .editor-body {
      padding: 18px 16px 22px;
    }

    .field-pair {
      grid-template-columns: 1fr;
    }

    .field.short {
      width: 100%;
    }

    .test-action {
      padding-inline: 9px;
    }
  }
</style>
