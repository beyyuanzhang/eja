<script lang="ts">
  import { Eye, EyeOff, Plus, Trash2 } from "lucide-svelte";
  import type { KeyValueDraft } from "./types";

  interface Props {
    items: KeyValueDraft[];
    kind: "env" | "headers";
    onchange: () => void;
  }

  let { items = $bindable(), kind, onchange }: Props = $props();
  let revealed = $state<Set<number>>(new Set());

  function looksSecret(key: string) {
    return /(secret|token|key|pass|auth|credential|cookie|session)/i.test(key);
  }

  function add() {
    items = [
      ...items,
      {
        key: "",
        value: "",
        secret: false,
        hasValue: false,
        redacted: false,
      },
    ];
    onchange();
  }

  function remove(index: number) {
    items = items.filter((_, itemIndex) => itemIndex !== index);
    revealed = new Set(
      [...revealed]
        .filter((item) => item !== index)
        .map((item) => (item > index ? item - 1 : item)),
    );
    onchange();
  }

  function toggle(index: number) {
    const next = new Set(revealed);
    if (next.has(index)) next.delete(index);
    else next.add(index);
    revealed = next;
  }
</script>

<div class="kv-list">
  {#each items as item, index}
    <div class="kv-row">
      <input
        class="kv-key"
        bind:value={item.key}
        oninput={() => {
          item.secret = looksSecret(item.key);
          onchange();
        }}
        type="text"
        placeholder={kind === "env" ? "VARIABLE_NAME" : "Authorization"}
        autocomplete="off"
        spellcheck="false"
        aria-label={kind === "env" ? "环境变量名" : "Header 名"}
      />
      <span class="kv-value">
        <input
          bind:value={item.value}
          oninput={() => {
            item.redacted = false;
            onchange();
          }}
          type={item.secret && !revealed.has(index) ? "password" : "text"}
          placeholder={item.redacted
            ? "已保存，留空保持"
            : kind === "env"
              ? "value 或 ${VAR}"
              : "value"}
          autocomplete="new-password"
          spellcheck="false"
          aria-label={kind === "env" ? "环境变量值" : "Header 值"}
        />
        {#if item.secret}
          <button
            type="button"
            aria-label={revealed.has(index) ? "隐藏值" : "显示值"}
            onclick={() => toggle(index)}
          >
            {#if revealed.has(index)}<EyeOff size={13} />{:else}<Eye size={13} />{/if}
          </button>
        {/if}
      </span>
      <button class="kv-remove" type="button" aria-label="移除" onclick={() => remove(index)}>
        <Trash2 size={13} />
      </button>
    </div>
  {/each}

  <button class="kv-add" type="button" onclick={add}>
    <Plus size={13} />
    {kind === "env" ? "添加环境变量" : "添加 Header"}
  </button>
</div>

<style>
  .kv-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .kv-row {
    display: grid;
    grid-template-columns: minmax(120px, 0.8fr) minmax(160px, 1.2fr) 30px;
    gap: 6px;
    align-items: center;
  }

  input {
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

  input:focus {
    border-color: rgba(189, 96, 66, 0.34);
    box-shadow: 0 0 0 3px rgba(189, 96, 66, 0.055);
  }

  input::placeholder {
    color: var(--faint);
  }

  .kv-value {
    display: flex;
    min-width: 0;
    align-items: center;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--surface);
  }

  .kv-value:focus-within {
    border-color: rgba(189, 96, 66, 0.34);
    box-shadow: 0 0 0 3px rgba(189, 96, 66, 0.055);
  }

  .kv-value input {
    min-width: 0;
    flex: 1;
    border: 0;
    box-shadow: none;
  }

  .kv-value input:focus {
    border: 0;
    box-shadow: none;
  }

  .kv-value button,
  .kv-remove {
    display: grid;
    width: 30px;
    height: 30px;
    flex: 0 0 30px;
    place-items: center;
    border-radius: 7px;
    color: var(--faint);
  }

  .kv-value button:hover {
    color: var(--ink);
  }

  .kv-remove:hover {
    background: var(--danger-wash);
    color: var(--danger);
  }

  .kv-add {
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

  .kv-add:hover {
    background: var(--surface-2);
    color: var(--ink-soft);
  }

  @media (max-width: 720px) {
    .kv-row {
      grid-template-columns: minmax(0, 1fr) 30px;
    }

    .kv-value {
      grid-column: 1 / 2;
    }

    .kv-remove {
      grid-column: 2;
      grid-row: 1 / 3;
    }
  }
</style>
