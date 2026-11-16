<script lang="ts">
  import { Plus, X } from "lucide-svelte";

  interface Props {
    value: string[];
    placeholder?: string;
    onchange: () => void;
  }

  let {
    value = $bindable(),
    placeholder = "输入后按 Enter",
    onchange,
  }: Props = $props();

  let input = $state("");

  function add(raw = input) {
    const parts = raw
      .split(",")
      .map((item) => item.trim())
      .filter(Boolean);
    if (!parts.length) return;
    const next = [...value];
    for (const part of parts) if (!next.includes(part)) next.push(part);
    value = next;
    input = "";
    onchange();
  }

  function remove(item: string) {
    value = value.filter((entry) => entry !== item);
    onchange();
  }
</script>

<div class="token-editor">
  {#if value.length}
    <div class="tokens">
      {#each value as item (item)}
        <span class="token">
          <code>{item}</code>
          <button type="button" aria-label={`移除 ${item}`} onclick={() => remove(item)}>
            <X size={10} />
          </button>
        </span>
      {/each}
    </div>
  {/if}

  <div class="token-input">
    <input
      bind:value={input}
      type="text"
      {placeholder}
      autocomplete="off"
      spellcheck="false"
      onkeydown={(event) => {
        if (event.key === "Enter" || event.key === ",") {
          event.preventDefault();
          add();
        }
        if (event.key === "Backspace" && !input && value.length) {
          value = value.slice(0, -1);
          onchange();
        }
      }}
    />
    <button type="button" aria-label="添加" disabled={!input.trim()} onclick={() => add()}>
      <Plus size={13} />
    </button>
  </div>

</div>

<style>
  .token-editor {
    display: flex;
    flex-direction: column;
    gap: 7px;
  }

  .tokens {
    display: flex;
    flex-wrap: wrap;
    gap: 5px;
  }

  .token {
    display: inline-flex;
    height: 25px;
    align-items: center;
    gap: 4px;
    padding: 0 5px 0 8px;
    border: 1px solid var(--line);
    border-radius: 7px;
    background: var(--surface-2);
    color: var(--ink-soft);
  }

  .token code {
    font-family: var(--mono);
    font-size: 8.5px;
  }

  .token button {
    display: grid;
    width: 19px;
    height: 19px;
    place-items: center;
    border-radius: 5px;
    color: var(--faint);
  }

  .token button:hover {
    background: var(--danger-wash);
    color: var(--danger);
  }

  .token-input {
    display: flex;
    height: 36px;
    align-items: center;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--surface);
  }

  .token-input:focus-within {
    border-color: rgba(189, 96, 66, 0.34);
    box-shadow: 0 0 0 3px rgba(189, 96, 66, 0.055);
  }

  .token-input input {
    min-width: 0;
    flex: 1;
    border: 0;
    outline: 0;
    background: transparent;
    padding: 0 10px;
    font-family: var(--mono);
    font-size: 9px;
  }

  .token-input input::placeholder {
    color: var(--faint);
    font-family: var(--sans);
  }

  .token-input button {
    display: grid;
    width: 34px;
    height: 32px;
    place-items: center;
    color: var(--muted);
  }

  .token-input button:disabled {
    opacity: 0.35;
  }

</style>
