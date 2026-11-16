<script lang="ts">
  import { untrack } from "svelte";
  import {
    AlertCircle,
    Check,
    ChevronDown,
    Eye,
    EyeOff,
    LoaderCircle,
    RefreshCw,
    Trash2,
    X,
  } from "lucide-svelte";
  import { discoverModels } from "./api";
  import KeyValueEditor from "./KeyValueEditor.svelte";
  import type { ModelDiscoverResult, ProviderDraft } from "./types";

  interface Props {
    provider: ProviderDraft;
    existingNames: string[];
    onSave: (provider: ProviderDraft) => void;
    onDelete?: () => void;
    onClose: () => void;
  }

  interface Preset {
    name: string;
    baseUrl: string;
  }

  const presets: Preset[] = [
    { name: "OpenAI", baseUrl: "https://api.openai.com/v1" },
    {
      name: "Google",
      baseUrl: "https://generativelanguage.googleapis.com/v1beta/openai",
    },
    { name: "OpenRouter", baseUrl: "https://openrouter.ai/api/v1" },
    { name: "DeepSeek", baseUrl: "https://api.deepseek.com/v1" },
    { name: "xAI", baseUrl: "https://api.x.ai/v1" },
    { name: "Groq", baseUrl: "https://api.groq.com/openai/v1" },
    { name: "Mistral", baseUrl: "https://api.mistral.ai/v1" },
    { name: "Moonshot", baseUrl: "https://api.moonshot.cn/v1" },
    { name: "Ollama", baseUrl: "http://127.0.0.1:11434/v1" },
    { name: "LM Studio", baseUrl: "http://127.0.0.1:1234/v1" },
  ];

  let { provider, existingNames, onSave, onDelete, onClose }: Props = $props();

  const initial = untrack(() => provider);
  let draft = $state<ProviderDraft>({
    ...initial,
    models: [...initial.models],
    headers: initial.headers.map((item) => ({ ...item })),
  });
  let showKey = $state(false);
  let discovering = $state(false);
  let discoverResult = $state<ModelDiscoverResult | null>(null);
  let attempted = $state(false);
  let deleteArmed = $state(false);

  const duplicate = $derived(
    draft.name.trim() !== provider.name.trim() && existingNames.includes(draft.name.trim()),
  );
  const valid = $derived(
    Boolean(draft.name.trim()) && Boolean(draft.baseUrl.trim()) && !duplicate,
  );
  const selectedPreset = $derived(
    presets.find((preset) => preset.baseUrl === draft.baseUrl.trim())?.name || "",
  );

  function update() {
    discoverResult = null;
    attempted = false;
    deleteArmed = false;
  }

  function applyPreset(name: string) {
    const preset = presets.find((item) => item.name === name);
    if (!preset) return;
    draft.name = preset.name;
    draft.baseUrl = preset.baseUrl;
    update();
  }

  function loadModels() {
    if (discovering || !draft.baseUrl.trim()) return;
    discovering = true;
    discoverResult = null;
    void discoverModels({
      id: draft.id,
      baseUrl: draft.baseUrl,
      apiKey: draft.apiKey,
      clearApiKey: draft.clearApiKey,
      headers: draft.headers.map((item) => ({ ...item })),
    })
      .then((result) => {
        discoverResult = result;
        if (result.ok && result.models) draft.models = result.models;
      })
      .catch((error) => {
        discoverResult = {
          ok: false,
          latencyMs: 0,
          error: error instanceof Error ? error.message : String(error),
        };
      })
      .finally(() => {
        discovering = false;
      });
  }

  function submit(event: SubmitEvent) {
    event.preventDefault();
    attempted = true;
    if (!valid) return;
    onSave({
      ...draft,
      name: draft.name.trim(),
      baseUrl: draft.baseUrl.trim(),
      apiKey: draft.apiKey.trim(),
      models: [...draft.models],
      headers: draft.headers.map((item) => ({ ...item })),
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
      <div class="title">
        <span class="mark">{draft.name.trim().slice(0, 1).toUpperCase() || "P"}</span>
        <h3>{draft.name.trim() || "Provider"}</h3>
      </div>
      <button class="icon-close" type="button" aria-label="关闭" onclick={onClose}>
        <X size={17} />
      </button>
    </header>

    <div class="editor-body">
      <label class="field">
        <span>Provider</span>
        <span class="select-wrap">
          <select
            value={selectedPreset}
            onchange={(event) => applyPreset(event.currentTarget.value)}
          >
            <option value="">Custom</option>
            {#each presets as preset}
              <option value={preset.name}>{preset.name}</option>
            {/each}
          </select>
          <ChevronDown size={12} />
        </span>
      </label>

      <label class="field">
        <span>名称</span>
        <input
          bind:value={draft.name}
          oninput={update}
          type="text"
          placeholder="OpenAI"
          maxlength="48"
          autocomplete="off"
          spellcheck="false"
        />
      </label>
      {#if attempted && (!draft.name.trim() || duplicate)}
        <p class="field-error">{duplicate ? "名称已存在" : "需要名称"}</p>
      {/if}

      <label class="field">
        <span>Base URL</span>
        <input
          bind:value={draft.baseUrl}
          oninput={update}
          type="url"
          placeholder="https://api.example.com/v1"
          autocomplete="url"
          spellcheck="false"
        />
      </label>

      <label class="field">
        <span>API key</span>
        <span class="secret-input">
          <input
            bind:value={draft.apiKey}
            oninput={() => {
              draft.clearApiKey = false;
              update();
            }}
            type={showKey ? "text" : "password"}
            placeholder={
              draft.clearApiKey
                ? ""
                : draft.hasApiKey
                  ? "留空保留"
                  : draft.local
                    ? "可选"
                    : "sk-…"
            }
            autocomplete="new-password"
          />
          {#if draft.hasApiKey}
            <button
              class:armed={draft.clearApiKey}
              type="button"
              title={draft.clearApiKey ? "保留现有密钥" : "保存时清除密钥"}
              aria-label={draft.clearApiKey ? "保留现有密钥" : "保存时清除密钥"}
              onclick={() => {
                draft.clearApiKey = !draft.clearApiKey;
                draft.apiKey = "";
                update();
              }}
            >
              <X size={13} />
            </button>
          {/if}
          <button
            type="button"
            aria-label={showKey ? "隐藏密钥" : "显示密钥"}
            onclick={() => (showKey = !showKey)}
          >
            {#if showKey}<EyeOff size={14} />{:else}<Eye size={14} />{/if}
          </button>
        </span>
      </label>

      <div class="field-block">
        <span>Headers</span>
        <KeyValueEditor bind:items={draft.headers} kind="headers" onchange={update} />
      </div>

      {#if discoverResult}
        <div class="discover-result" class:failed={!discoverResult.ok}>
          {#if discoverResult.ok}
            <Check size={13} />
            <span>{discoverResult.models?.length || 0} models · {discoverResult.latencyMs} ms</span>
          {:else}
            <AlertCircle size={13} />
            <span>{discoverResult.error}</span>
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
          title={deleteArmed ? "确认删除" : "删除"}
          aria-label={deleteArmed ? "确认删除" : "删除 Provider"}
          onclick={remove}
        >
          <Trash2 size={15} />
        </button>
      {:else}
        <span></span>
      {/if}
      <div class="foot-actions">
        <button
          class="discover-action"
          type="button"
          disabled={discovering || !draft.baseUrl.trim()}
          onclick={loadModels}
        >
          {#if discovering}
            <LoaderCircle size={13} class="spin" />
          {:else}
            <RefreshCw size={13} />
          {/if}
          读取模型
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
    z-index: 92;
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
    width: min(520px, 94vw);
    max-height: min(760px, 92dvh);
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

  .title {
    display: flex;
    min-width: 0;
    align-items: center;
    gap: 10px;
  }

  .mark {
    display: grid;
    width: 34px;
    height: 34px;
    flex: 0 0 34px;
    place-items: center;
    border-radius: 10px;
    background: var(--accent-wash);
    color: var(--accent-deep);
    font-family: var(--serif);
    font-size: 15px;
    font-weight: 600;
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

  .icon-close {
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

  .editor-body {
    display: flex;
    flex: 1;
    flex-direction: column;
    gap: 13px;
    overflow-y: auto;
    padding: 20px 22px 24px;
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

  .field > input,
  .secret-input,
  .select-wrap {
    width: 100%;
    height: 39px;
    border: 1px solid var(--line);
    border-radius: 9px;
    background: var(--surface);
  }

  .field > input {
    min-width: 0;
    outline: 0;
    padding: 0 11px;
    color: var(--ink);
    font-family: var(--mono);
    font-size: 10px;
  }

  .field > input:focus,
  .secret-input:focus-within,
  .select-wrap:focus-within {
    border-color: rgba(189, 96, 66, 0.34);
    box-shadow: 0 0 0 3px rgba(189, 96, 66, 0.055);
  }

  .field input::placeholder {
    color: var(--faint);
  }

  .secret-input {
    display: flex;
    align-items: center;
  }

  .secret-input input {
    min-width: 0;
    flex: 1;
    height: 100%;
    border: 0;
    outline: 0;
    background: transparent;
    padding: 0 9px 0 11px;
    color: var(--ink);
    font-family: var(--mono);
    font-size: 10px;
  }

  .secret-input button {
    display: grid;
    width: 32px;
    height: 32px;
    flex: 0 0 32px;
    place-items: center;
    border-radius: 7px;
    color: var(--faint);
  }

  .secret-input button:hover {
    color: var(--ink);
  }

  .secret-input button.armed {
    background: var(--danger-wash);
    color: var(--danger);
  }

  .select-wrap {
    position: relative;
    display: flex;
    align-items: center;
  }

  .select-wrap select {
    min-width: 0;
    height: 100%;
    flex: 1;
    border: 0;
    outline: 0;
    appearance: none;
    background: transparent;
    padding: 0 32px 0 11px;
    color: var(--ink-soft);
    font-size: 10.5px;
  }

  .select-wrap :global(svg) {
    position: absolute;
    right: 10px;
    color: var(--faint);
    pointer-events: none;
  }

  .field-error {
    margin: -7px 0 0;
    color: var(--danger);
    font-size: 9px;
  }

  .discover-result {
    display: flex;
    align-items: flex-start;
    gap: 7px;
    padding: 9px 10px;
    border-radius: 9px;
    background: var(--accent-wash);
    color: var(--accent-deep);
    font-size: 9.5px;
    line-height: 1.45;
  }

  .discover-result.failed {
    background: var(--danger-wash);
    color: var(--danger);
  }

  .discover-result :global(svg) {
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

  .foot-actions {
    display: flex;
    align-items: center;
    gap: 7px;
  }

  .delete-action {
    display: grid;
    width: 32px;
    height: 32px;
    place-items: center;
    border-radius: 999px;
    color: var(--faint);
  }

  .delete-action:hover,
  .delete-action.armed {
    background: var(--danger-wash);
    color: var(--danger);
  }

  .discover-action,
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

  .discover-action,
  .cancel-action {
    color: var(--muted);
  }

  .discover-action:hover:not(:disabled),
  .cancel-action:hover {
    background: var(--surface-2);
    color: var(--ink);
  }

  .discover-action:disabled {
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

    .discover-action {
      padding-inline: 9px;
    }
  }
</style>
