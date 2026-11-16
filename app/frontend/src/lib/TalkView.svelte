<script lang="ts">
  import {
    ArrowUp,
    Check,
    ChevronDown,
    Copy,
    FileText,
    Paperclip,
    X,
  } from "lucide-svelte";
  import { tick } from "svelte";
  import Markdown from "./Markdown.svelte";
  import ProposalCard from "./ProposalCard.svelte";
  import { clock, dayKey, dayLabel, leadTask, relative } from "./format";
  import type {
    ComposerSeed,
    Health,
    Message,
    OptimisticMessage,
    Proposal,
    Task,
  } from "./types";

  interface Props {
    messages: Message[];
    proposals: Proposal[];
    tasks: Task[];
    health: Health;
    pending: boolean;
    optimistic: OptimisticMessage | null;
    visible: boolean;
    seed: ComposerSeed | null;
    onSend: (text: string) => void;
    onApprove: (id: number) => void;
    onDismiss: (id: number) => void;
    onRaw: (address: string) => void;
    onRead: (id: number) => void;
  }

  let {
    messages,
    proposals,
    tasks,
    health,
    pending,
    optimistic,
    visible,
    seed,
    onSend,
    onApprove,
    onDismiss,
    onRaw,
    onRead,
  }: Props = $props();

  let scroller = $state<HTMLDivElement>();
  let textarea = $state<HTMLTextAreaElement>();
  let fileInput = $state<HTMLInputElement>();
  let draft = $state("");
  let attachments = $state<Array<{ name: string; content: string }>>([]);
  let expanded = $state(false);
  let atBottom = $state(true);
  let dragging = $state(false);
  let copiedId = $state<number | null>(null);
  let lastReadSent = 0;
  let firstLayout = true;

  const visibleLimit = 60;

  type TimelineItem =
    | { type: "day"; key: string; at: number; label: string }
    | { type: "message"; key: string; message: Message };

  const shownMessages = $derived(expanded ? messages : messages.slice(-visibleLimit));
  const hiddenCount = $derived(Math.max(0, messages.length - shownMessages.length));
  const canSend = $derived((draft.trim().length > 0 || attachments.length > 0) && !pending);

  const timeline = $derived.by<TimelineItem[]>(() => {
    const items: TimelineItem[] = [];
    let previousDay = "";
    for (const message of shownMessages) {
      const key = dayKey(message.at);
      if (key !== previousDay) {
        items.push({ type: "day", key: `day-${key}`, at: message.at, label: dayLabel(message.at) });
        previousDay = key;
      }
      items.push({ type: "message", key: `message-${message.id}`, message });
    }
    return items;
  });

  const optimisticDuplicate = $derived(
    optimistic
      ? messages.some(
          (message) =>
            message.role === "user" &&
            message.text.trim() === optimistic.text.trim() &&
            message.at >= optimistic.at - 5,
        )
      : false,
  );

  const presence = $derived.by(() => {
    if (!health.model && messages.length === 0) {
      return { tone: "waiting", text: "模型还没有接上" };
    }
    if (pending || health.turnInFlight) {
      return { tone: "running", text: "eja 正在想这件事" };
    }
    if (proposals.length) {
      return { tone: "waiting", text: "有一件事等你决定" };
    }
    const running = tasks.filter((task) => task.status === "running");
    if (running.length) {
      const lead = leadTask(running);
      return {
        tone: "running",
        text: `正在办 · ${lead?.title ?? "一件事"}${running.length > 1 ? ` +${running.length - 1}` : ""}`,
      };
    }
    const waiting = tasks.filter((task) => task.status === "waiting");
    if (waiting.length) {
      return { tone: "waiting", text: `等它收尾 · ${leadTask(waiting)?.title ?? "一件事"}` };
    }
    const open = tasks.filter((task) => task.status === "open");
    if (open.length) {
      return { tone: "open", text: `还没开始 · ${leadTask(open)?.title ?? "一件事"}` };
    }
    return { tone: "open", text: "它在" };
  });

  function isNearBottom(): boolean {
    if (!scroller) return true;
    return scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 130;
  }

  async function scrollToBottom(behavior: ScrollBehavior = "smooth") {
    await tick();
    if (!scroller) return;
    scroller.scrollTo({ top: scroller.scrollHeight, behavior });
    atBottom = true;
    pushRead();
  }

  function pushRead() {
    if (!atBottom || !visible) return;
    const max = messages.reduce((value, message) => Math.max(value, message.id), 0);
    if (max > lastReadSent) {
      lastReadSent = max;
      onRead(max);
    }
  }

  function resizeComposer() {
    if (!textarea) return;
    textarea.style.height = "0px";
    textarea.style.height = `${Math.min(176, Math.max(24, textarea.scrollHeight))}px`;
  }

  function composeText(): string {
    const body = draft.trim();
    const files = attachments
      .map(
        (file) =>
          `\n\n【文件：${file.name}】\n\`\`\`\n${file.content.trim()}\n\`\`\`\n`,
      )
      .join("");
    return `${body}${files}`.trim();
  }

  function submit() {
    if (!canSend) return;
    const text = composeText();
    if (!text) return;
    draft = "";
    attachments = [];
    atBottom = true;
    onSend(text);
    resizeComposer();
  }

  async function addFiles(files: FileList | File[]) {
    const next = [...files];
    for (const file of next.slice(0, 6)) {
      if (file.size > 256 * 1024) {
        continue;
      }
      try {
        attachments = [
          ...attachments,
          { name: file.name || "未命名.txt", content: await file.text() },
        ].slice(0, 6);
      } catch {
        // A non-text file simply does not become an attachment.
      }
    }
  }

  async function copyMessage(message: Message) {
    try {
      await navigator.clipboard.writeText(message.text);
      copiedId = message.id;
      setTimeout(() => {
        if (copiedId === message.id) copiedId = null;
      }, 1600);
    } catch {
      copiedId = null;
    }
  }

  $effect(() => {
    messages.length;
    optimistic?.text;
    pending;
    if (firstLayout || atBottom) {
      const initial = firstLayout;
      firstLayout = false;
      void scrollToBottom(initial ? "auto" : "smooth");
    }
  });

  $effect(() => {
    if (visible && atBottom) {
      void tick().then(pushRead);
    }
  });

  $effect(() => {
    if (!seed || !textarea) return;
    draft = seed.text;
    void tick().then(() => {
      resizeComposer();
      textarea?.focus();
      textarea?.setSelectionRange(seed.text.length, seed.text.length);
      scrollToBottom();
    });
  });

  $effect(() => {
    draft;
    attachments.length;
    void tick().then(resizeComposer);
  });
</script>

<section class="talk-view" aria-label="交流">
  <div
    class="thread-scroll"
    role="region"
    aria-label="对话记录"
    bind:this={scroller}
    onscroll={() => {
      atBottom = isNearBottom();
      if (atBottom) pushRead();
    }}
    ondragover={(event) => {
      event.preventDefault();
      dragging = true;
    }}
    ondragleave={(event) => {
      if (event.currentTarget === event.target) dragging = false;
    }}
    ondrop={(event) => {
      event.preventDefault();
      dragging = false;
      if (event.dataTransfer?.files) void addFiles(event.dataTransfer.files);
    }}
  >
    <div class="thread">
      {#if messages.length === 0 && !optimistic}
        <div class="empty-state">
          <div class="empty-mark" aria-hidden="true">
            <span></span>
          </div>
          <h1>我在这儿。</h1>
        </div>
      {:else}
        {#if hiddenCount > 0}
          <button class="earlier" type="button" onclick={() => (expanded = true)}>
            <ChevronDown size={12} />
            更早的 {hiddenCount} 条
          </button>
        {/if}

        {#each timeline as item (item.key)}
          {#if item.type === "day"}
            <div class="day-divider"><span>{item.label}</span></div>
          {:else if item.message.role === "user"}
            <article class="user-message">
              <div class="user-bubble">{item.message.text}</div>
              <time title={clock(item.message.at)}>{relative(item.message.at)}</time>
            </article>
          {:else}
            <article class="assistant-message">
              <div class="message-rail" aria-hidden="true">
                <span class="message-mark"></span>
              </div>
              <div class="message-body">
                <div class="message-label">
                  <span>eja</span>
                  <time>{relative(item.message.at)}</time>
                </div>
                <Markdown text={item.message.text || "（这一轮没有说话。）"} />
                <div class="message-actions">
                  <button
                    class="chip"
                    type="button"
                    onclick={() => onRaw(`log:event:${item.message.id}`)}
                  >
                    log:event:{item.message.id}
                  </button>
                  <button
                    class="copy-button"
                    type="button"
                    aria-label="复制这条回复"
                    onclick={() => copyMessage(item.message)}
                  >
                    {#if copiedId === item.message.id}
                      <Check size={13} />
                    {:else}
                      <Copy size={13} />
                    {/if}
                  </button>
                </div>
              </div>
            </article>
          {/if}
        {/each}

        {#if optimistic && !optimisticDuplicate}
          <article class="user-message pending-message">
            <div class="user-bubble">{optimistic.text}</div>
            <time>正在递过去</time>
          </article>
        {/if}

        {#if pending}
          <div class="thinking" aria-live="polite">
            <span class="thinking-dot"></span>
            <span class="thinking-dot"></span>
            <span class="thinking-dot"></span>
            <span>eja 在想</span>
          </div>
        {/if}
      {/if}

      {#if proposals.length}
        <div class="proposals">
          {#each proposals as proposal (proposal.id)}
            <ProposalCard {proposal} {pending} {onApprove} {onDismiss} />
          {/each}
        </div>
      {/if}
    </div>
  </div>

  <div class="composer-zone">
    {#if dragging}
      <div class="drop-overlay">
        <FileText size={22} strokeWidth={1.5} />
        <span>放到这里，一起交给 eja</span>
      </div>
    {/if}

    <div class="composer-inner">
      <div class="presence">
        <span class="presence-state">
          <i class="status-dot {presence.tone}"></i>
          <span>{presence.text}</span>
        </span>
      </div>

      <form
        class="composer"
        onsubmit={(event) => {
          event.preventDefault();
          submit();
        }}
      >
        {#if attachments.length}
          <div class="attachments">
            {#each attachments as file, index (`${file.name}-${index}`)}
              <span class="attachment">
                <FileText size={12} />
                <span>{file.name}</span>
                <button
                  type="button"
                  aria-label={`移除 ${file.name}`}
                  onclick={() => (attachments = attachments.filter((_, i) => i !== index))}
                >
                  <X size={11} />
                </button>
              </span>
            {/each}
          </div>
        {/if}
        <div class="composer-row">
          <button
            class="composer-tool"
            type="button"
            aria-label="添加文本文件"
            title="添加文本文件"
            onclick={() => fileInput?.click()}
          >
            <Paperclip size={18} strokeWidth={1.7} />
          </button>
          <textarea
            bind:this={textarea}
            bind:value={draft}
            rows="1"
            placeholder="说一件事…"
            spellcheck="false"
            onkeydown={(event) => {
              if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
                event.preventDefault();
                submit();
              }
            }}
          ></textarea>
          <button class="send-button" type="submit" aria-label="发送" disabled={!canSend}>
            <ArrowUp size={17} strokeWidth={2.1} />
          </button>
        </div>
      </form>
      <input
        bind:this={fileInput}
        class="sr-only"
        type="file"
        multiple
        accept=".txt,.md,.json,.jsonl,.csv,.log,.yaml,.yml,.toml,.rs,.ts,.js,.css,.html"
        onchange={() => {
          if (fileInput?.files) void addFiles(fileInput.files);
          if (fileInput) fileInput.value = "";
        }}
      />
    </div>
  </div>
</section>

<style>
  .talk-view {
    position: relative;
    display: flex;
    height: 100%;
    min-height: 0;
    flex-direction: column;
    overflow: hidden;
  }

  .thread-scroll {
    flex: 1;
    min-height: 0;
    overflow-x: hidden;
    overflow-y: auto;
    scrollbar-gutter: stable;
  }

  .thread {
    width: min(100%, 820px);
    min-height: 100%;
    margin: 0 auto;
    padding: 38px 30px 22px;
  }

  .day-divider {
    display: flex;
    align-items: center;
    gap: 12px;
    margin: 9px 0 30px;
    color: var(--faint);
    font-size: 10.5px;
    letter-spacing: 0.06em;
  }

  .day-divider::before,
  .day-divider::after {
    height: 1px;
    flex: 1;
    background: linear-gradient(90deg, transparent, var(--line), transparent);
    content: "";
  }

  .earlier {
    display: flex;
    align-items: center;
    gap: 5px;
    margin: 0 auto 30px;
    color: var(--muted);
    font-size: 11px;
  }

  .earlier:hover {
    color: var(--ink);
  }

  .empty-state {
    display: flex;
    min-height: calc(100vh - 330px);
    align-items: center;
    justify-content: center;
    flex-direction: column;
    padding: 40px 0 72px;
    text-align: center;
  }

  .empty-mark {
    display: grid;
    width: 48px;
    height: 48px;
    margin-bottom: 19px;
    place-items: center;
    border: 1px solid var(--line-strong);
    border-radius: 50%;
    background: color-mix(in srgb, var(--surface) 82%, transparent);
    box-shadow: var(--shadow-sm);
  }

  .empty-mark span {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    background: var(--accent);
    box-shadow: 0 0 0 5px rgba(189, 96, 66, 0.08);
    animation: dot-breathe 2.8s ease-out infinite;
  }

  .empty-state h1 {
    margin: 0;
    color: var(--ink);
    font-family: var(--serif);
    font-size: 34px;
    font-weight: 420;
    letter-spacing: -0.035em;
  }

  .assistant-message {
    display: grid;
    grid-template-columns: 26px minmax(0, 1fr);
    gap: 13px;
    margin-bottom: 34px;
    animation: message-in 240ms cubic-bezier(0.2, 0.7, 0.2, 1) both;
  }

  .message-rail {
    display: flex;
    justify-content: center;
    padding-top: 4px;
  }

  .message-mark {
    width: 22px;
    height: 22px;
    border: 1px solid var(--line-strong);
    border-radius: 50%;
    background:
      radial-gradient(circle at 50% 50%, var(--accent) 0 2.2px, transparent 2.6px),
      var(--surface);
    box-shadow: var(--shadow-xs);
  }

  .message-body {
    min-width: 0;
  }

  .message-label {
    display: flex;
    height: 23px;
    align-items: flex-start;
    gap: 8px;
  }

  .message-label span {
    color: var(--ink-soft);
    font-family: var(--serif);
    font-size: 13px;
    font-weight: 600;
    letter-spacing: 0.015em;
  }

  .message-label time {
    padding-top: 1px;
    color: var(--faint);
    font-size: 9.5px;
    opacity: 0;
    transition: opacity 160ms ease;
  }

  .assistant-message:hover .message-label time {
    opacity: 1;
  }

  .message-actions {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 9px;
    opacity: 0.44;
    transition: opacity 160ms ease;
  }

  .assistant-message:hover .message-actions,
  .message-actions:focus-within {
    opacity: 1;
  }

  .copy-button {
    display: inline-flex;
    width: 24px;
    height: 24px;
    align-items: center;
    justify-content: center;
    border-radius: 7px;
    color: var(--faint);
  }

  .copy-button:hover {
    background: var(--surface-2);
    color: var(--ink);
  }

  .user-message {
    display: flex;
    align-items: flex-end;
    flex-direction: column;
    gap: 5px;
    margin: 0 0 31px auto;
    animation: message-in 220ms cubic-bezier(0.2, 0.7, 0.2, 1) both;
  }

  .user-bubble {
    max-width: min(76%, 590px);
    padding: 10px 15px;
    border: 1px solid rgba(36, 35, 31, 0.065);
    border-radius: 18px 18px 5px 18px;
    background: #eae6dc;
    color: var(--ink);
    font-size: 14px;
    line-height: 1.62;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    box-shadow: 0 1px 1px rgba(36, 35, 31, 0.03);
  }

  .user-message time {
    padding-right: 3px;
    color: var(--faint);
    font-size: 9.5px;
  }

  .pending-message {
    opacity: 0.58;
  }

  .thinking {
    display: flex;
    align-items: center;
    gap: 4px;
    margin: -15px 0 28px 39px;
    color: var(--muted);
    font-size: 11px;
    animation: message-in 180ms ease both;
  }

  .thinking span:last-child {
    margin-left: 4px;
  }

  .thinking-dot {
    width: 4px;
    height: 4px;
    border-radius: 50%;
    background: var(--accent);
    animation: thinking 1.15s ease-in-out infinite;
  }

  .thinking-dot:nth-child(2) {
    animation-delay: 120ms;
  }

  .thinking-dot:nth-child(3) {
    animation-delay: 240ms;
  }

  .proposals {
    display: flex;
    max-width: 680px;
    flex-direction: column;
    gap: 10px;
    margin: 2px 0 30px 39px;
  }

  .composer-zone {
    position: relative;
    z-index: 5;
    flex: 0 0 auto;
    background: linear-gradient(180deg, rgba(248, 247, 242, 0), rgba(248, 247, 242, 0.92) 22%, var(--paper) 54%);
    padding: 16px 20px calc(14px + env(safe-area-inset-bottom)) 58px;
  }

  .composer-inner {
    width: min(100%, 780px);
    margin: 0 auto;
  }

  .presence {
    display: flex;
    height: 24px;
    align-items: center;
    justify-content: space-between;
    padding: 0 8px 6px;
    color: var(--muted);
    font-size: 10.5px;
  }

  .presence-state {
    display: flex;
    align-items: center;
    gap: 7px;
    min-width: 0;
  }

  .presence-state > span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .composer {
    overflow: hidden;
    border: 1px solid rgba(36, 35, 31, 0.14);
    border-radius: 22px;
    background: color-mix(in srgb, var(--surface) 96%, transparent);
    box-shadow:
      0 12px 38px rgba(36, 35, 31, 0.07),
      0 1px 1px rgba(255, 255, 255, 0.8) inset;
    backdrop-filter: blur(16px);
    transition:
      border-color 160ms ease,
      box-shadow 160ms ease;
  }

  .composer:focus-within {
    border-color: rgba(189, 96, 66, 0.33);
    box-shadow:
      0 14px 44px rgba(36, 35, 31, 0.09),
      0 0 0 3px rgba(189, 96, 66, 0.055);
  }

  .composer-row {
    display: flex;
    align-items: flex-end;
    gap: 6px;
    padding: 8px 9px 8px 10px;
  }

  .composer-tool {
    display: inline-flex;
    width: 34px;
    height: 34px;
    flex: 0 0 34px;
    align-items: center;
    justify-content: center;
    border-radius: 50%;
    color: var(--muted);
    transition:
      background 150ms ease,
      color 150ms ease;
  }

  .composer-tool:hover {
    background: var(--surface-2);
    color: var(--ink);
  }

  textarea {
    min-height: 24px;
    max-height: 176px;
    flex: 1;
    resize: none;
    border: 0;
    outline: 0;
    background: transparent;
    padding: 6px 3px;
    color: var(--ink);
    font-size: 14.5px;
    line-height: 1.55;
    scrollbar-width: none;
  }

  textarea::-webkit-scrollbar {
    display: none;
  }

  textarea::placeholder {
    color: var(--faint);
  }

  .send-button {
    display: inline-flex;
    width: 34px;
    height: 34px;
    flex: 0 0 34px;
    align-items: center;
    justify-content: center;
    border-radius: 50%;
    background: var(--ink);
    color: #fff;
    transition:
      transform 150ms ease,
      background 150ms ease,
      opacity 150ms ease;
  }

  .send-button:hover:not(:disabled) {
    background: #11110e;
    transform: translateY(-1px);
  }

  .send-button:disabled {
    background: #d7d3c8;
    opacity: 0.72;
  }

  .attachments {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    padding: 10px 12px 0;
  }

  .attachment {
    display: inline-flex;
    max-width: 220px;
    height: 27px;
    align-items: center;
    gap: 6px;
    padding: 0 5px 0 8px;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--surface-2);
    color: var(--muted);
    font-size: 10.5px;
  }

  .attachment > span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .attachment button {
    display: grid;
    width: 18px;
    height: 18px;
    place-items: center;
    border-radius: 4px;
    color: var(--faint);
  }

  .attachment button:hover {
    background: var(--surface-3);
    color: var(--ink);
  }

  .drop-overlay {
    position: absolute;
    inset: 4px 12px 12px;
    z-index: 10;
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 9px;
    border: 1px dashed rgba(189, 96, 66, 0.45);
    border-radius: 19px;
    background: rgba(248, 247, 242, 0.92);
    color: var(--accent-deep);
    font-size: 12px;
    backdrop-filter: blur(6px);
  }

  @keyframes message-in {
    from {
      opacity: 0;
      transform: translateY(5px);
    }
    to {
      opacity: 1;
      transform: none;
    }
  }

  @keyframes thinking {
    0%,
    70%,
    100% {
      opacity: 0.25;
      transform: translateY(0);
    }
    35% {
      opacity: 1;
      transform: translateY(-2px);
    }
  }

  @media (max-width: 720px) {
    .thread {
      padding: 26px 18px 14px;
    }

    .assistant-message {
      grid-template-columns: 20px minmax(0, 1fr);
      gap: 10px;
    }

    .message-mark {
      width: 18px;
      height: 18px;
    }

    .message-label time {
      opacity: 0.55;
    }

    .user-bubble {
      max-width: 88%;
    }

    .proposals {
      margin-left: 30px;
    }

    .composer-zone {
      padding: 12px 12px calc(11px + env(safe-area-inset-bottom)) 56px;
    }

    .presence {
      padding-inline: 5px;
    }

    .message-actions {
      opacity: 0.7;
    }
  }

  @media (max-width: 480px) {
    .thread {
      padding-inline: 15px;
    }

    .assistant-message {
      gap: 8px;
    }

    .message-rail {
      display: none;
    }

    .assistant-message {
      grid-template-columns: minmax(0, 1fr);
    }

    .proposals {
      margin-left: 0;
    }

    .empty-state h1 {
      font-size: 31px;
    }

  }
</style>
