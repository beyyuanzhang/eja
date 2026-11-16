<script lang="ts">
  import { Check, Copy, ExternalLink, X } from "lucide-svelte";
  import { fade, scale } from "svelte/transition";
  import type { RawRecord } from "./types";

  let {
    raw,
    onClose,
  }: {
    raw: RawRecord;
    onClose: () => void;
  } = $props();

  let copied = $state(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(raw.content);
      copied = true;
      setTimeout(() => (copied = false), 1600);
    } catch {
      copied = false;
    }
  }
</script>

<div class="raw-layer">
  <button class="raw-backdrop" type="button" aria-label="关闭原始记录" onclick={onClose} transition:fade={{ duration: 150 }}></button>
  <dialog
    open
    class="raw-dialog"
    aria-modal="true"
    aria-label="原始记录"
    transition:scale={{ start: 0.975, duration: 190, opacity: 0.6 }}
  >
    <header>
      <div class="raw-title">
        <span class="raw-mark"><ExternalLink size={13} /></span>
        <div>
          <span>原始记录</span>
          <code>{raw.address}</code>
        </div>
      </div>
      <div class="raw-actions">
        <button class="copy" type="button" onclick={copy}>
          {#if copied}<Check size={13} />已复制{:else}<Copy size={13} />复制{/if}
        </button>
        <button class="icon-button" type="button" aria-label="关闭" onclick={onClose}>
          <X size={16} />
        </button>
      </div>
    </header>
    <div class="code-shell">
      <div class="code-gutter" aria-hidden="true">
        {#each Array(Math.min(200, raw.content.split("\n").length)) as _, index}
          <span>{index + 1}</span>
        {/each}
      </div>
      <pre>{raw.content}</pre>
    </div>
    <footer>
      <span>{raw.kind === "json" ? "结构化记录" : "文本"}</span>
      {#if raw.bytes}<span>{raw.bytes.toLocaleString()} bytes</span>{/if}
      {#if raw.truncated}<span class="truncated">内容已截断</span>{/if}
    </footer>
  </dialog>
</div>

<style>
  .raw-layer {
    position: fixed;
    inset: 0;
    z-index: 80;
    display: grid;
    place-items: center;
    padding: 24px;
  }

  .raw-backdrop {
    position: absolute;
    inset: 0;
    background: rgba(30, 29, 26, 0.32);
    backdrop-filter: blur(4px);
  }

  .raw-dialog {
    position: relative;
    display: flex;
    width: min(820px, 100%);
    max-height: min(82vh, 760px);
    margin: 0;
    padding: 0;
    flex-direction: column;
    overflow: hidden;
    border: 1px solid rgba(255, 255, 255, 0.08);
    border-radius: 18px;
    background: #292824;
    color: inherit;
    box-shadow: 0 32px 100px rgba(20, 19, 17, 0.3);
  }

  header {
    display: flex;
    min-height: 62px;
    align-items: center;
    justify-content: space-between;
    gap: 14px;
    padding: 0 15px 0 18px;
    border-bottom: 1px solid rgba(255, 255, 255, 0.075);
  }

  .raw-title {
    display: flex;
    min-width: 0;
    align-items: center;
    gap: 10px;
  }

  .raw-mark {
    display: grid;
    width: 28px;
    height: 28px;
    flex: 0 0 28px;
    place-items: center;
    border: 1px solid rgba(255, 255, 255, 0.09);
    border-radius: 8px;
    color: #d7a48e;
  }

  .raw-title > div {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 1px;
  }

  .raw-title span {
    color: #a8a49a;
    font-size: 8.5px;
    letter-spacing: 0.11em;
    text-transform: uppercase;
  }

  .raw-title code {
    overflow: hidden;
    color: #dedbd2;
    font-family: var(--mono);
    font-size: 10.5px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .raw-actions {
    display: flex;
    align-items: center;
    gap: 5px;
  }

  .copy {
    display: inline-flex;
    height: 30px;
    align-items: center;
    gap: 5px;
    padding: 0 9px;
    border: 1px solid rgba(255, 255, 255, 0.09);
    border-radius: 7px;
    color: #b8b4aa;
    font-size: 10px;
  }

  .copy:hover {
    border-color: rgba(255, 255, 255, 0.16);
    color: #f2f0ea;
  }

  .raw-actions :global(.icon-button) {
    color: #98948b;
  }

  .raw-actions :global(.icon-button:hover) {
    background: rgba(255, 255, 255, 0.06);
    color: #fff;
  }

  .code-shell {
    display: flex;
    flex: 1;
    min-height: 0;
    overflow: auto;
    background: #23221f;
  }

  .code-gutter {
    position: sticky;
    left: 0;
    display: flex;
    min-width: 48px;
    flex: 0 0 48px;
    align-items: flex-end;
    flex-direction: column;
    padding: 18px 12px 24px 8px;
    border-right: 1px solid rgba(255, 255, 255, 0.055);
    background: #23221f;
    color: #5e5b54;
    font-family: var(--mono);
    font-size: 9.5px;
    line-height: 1.67;
    user-select: none;
  }

  pre {
    flex: 1;
    margin: 0;
    padding: 18px 20px 28px;
    color: #d8d4ca;
    font-family: var(--mono);
    font-size: 10.5px;
    line-height: 1.67;
    tab-size: 2;
  }

  footer {
    display: flex;
    min-height: 34px;
    align-items: center;
    gap: 12px;
    padding: 0 17px;
    border-top: 1px solid rgba(255, 255, 255, 0.06);
    color: #77736b;
    font-family: var(--mono);
    font-size: 8.5px;
  }

  .truncated {
    color: #c78b75;
  }

  @media (max-width: 620px) {
    .raw-layer {
      align-items: end;
      padding: 10px;
    }

    .raw-dialog {
      max-height: 88vh;
      border-radius: 16px;
    }

    .code-gutter {
      display: none;
    }
  }
</style>
