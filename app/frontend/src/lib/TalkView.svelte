<script lang="ts">
  import { ArrowUp, Check, ChevronRight } from "lucide-svelte";
  import { tick } from "svelte";
  import { fade } from "svelte/transition";
  import Markdown from "./Markdown.svelte";
  import { leadTask } from "./format";
  import type {
    ComposerSeed,
    Health,
    Message,
    OpenLoop,
    OptimisticMessage,
    Task,
  } from "./types";

  interface Props {
    mode: "ambient" | "encounter";
    loops: OpenLoop[];
    actions: OpenLoop[];
    messages: Message[];
    presence: Message | null;
    tasks: Task[];
    health: Health;
    pending: boolean;
    optimistic: OptimisticMessage | null;
    visible: boolean;
    seed: ComposerSeed | null;
    onOpenLoop: (loop: OpenLoop) => void;
    onSend: (text: string) => void;
    onApprove: (proposalId: number) => void;
    onInspectTask: (task: Task) => void;
    onBye: () => void;
  }

  let {
    mode,
    loops,
    actions,
    messages,
    presence,
    tasks,
    health,
    pending,
    optimistic,
    visible,
    seed,
    onOpenLoop,
    onSend,
    onApprove,
    onInspectTask,
    onBye,
  }: Props = $props();

  let scroller = $state<HTMLDivElement>();
  let textarea = $state<HTMLTextAreaElement>();
  let draft = $state("");
  let firstLayout = true;
  let displayedPresence = $state<Message | null>(null);

  $effect(() => {
    if (presence) displayedPresence = presence;
  });

  const visibleMessages = $derived(
    messages.filter((message) => message.text.trim().length > 0).slice(-6),
  );
  const hiddenCount = $derived(Math.max(0, messages.length - visibleMessages.length));
  const canSend = $derived(draft.trim().length > 0 && !pending);
  const background = $derived(
    tasks.filter(
      (task) =>
        task.status === "running" && (pending || health.turnInFlight),
    ),
  );
  const lead = $derived(leadTask(background));
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

  function resizeComposer() {
    if (!textarea) return;
    textarea.style.height = "0px";
    textarea.style.height = `${Math.min(132, Math.max(24, textarea.scrollHeight))}px`;
  }

  function submit() {
    if (!canSend) return;
    const text = draft.trim();
    if (!text) return;
    draft = "";
    onSend(text);
    resizeComposer();
  }

  $effect(() => {
    messages.length;
    optimistic?.text;
    actions.length;
    mode;
    if (!scroller) return;
    const initial = firstLayout;
    firstLayout = false;
    void tick().then(() => {
      if (mode === "ambient") {
        scroller?.scrollTo({ top: 0, behavior: initial ? "auto" : "smooth" });
        return;
      }
      scroller?.scrollTo({
        top: scroller.scrollHeight,
        behavior: initial ? "auto" : "smooth",
      });
    });
  });

  $effect(() => {
    if (!seed || !textarea || !visible) return;
    draft = seed.text;
    void tick().then(() => {
      resizeComposer();
      textarea?.focus();
      textarea?.setSelectionRange(seed.text.length, seed.text.length);
    });
  });

  $effect(() => {
    draft;
    void tick().then(resizeComposer);
  });
</script>

<section class="talk-view" class:engaged={mode === "encounter"} aria-label="交流">
  <div class="talk-scroll" bind:this={scroller}>
    {#if mode === "ambient"}
      <section class="ambient" class:quiet={!loops.length} aria-label="回来">
        {#if displayedPresence}
          <article class="presence-turn" out:fade={{ duration: 220 }}>
            <Markdown text={displayedPresence.text} />
          </article>
        {:else if !loops.length && !background.length}
          <div class="stillness" aria-hidden="true"><span></span></div>
        {/if}
        {#if loops.length}
          <div class="loops">
            {#each loops as loop (loop.key)}
              <button
                class="loop-row"
                type="button"
                aria-label={loop.title}
                onclick={() => onOpenLoop(loop)}
              >
                <i class="status-dot {loop.kind === "paused" ? "open" : "waiting"}"></i>
                <span class="loop-copy">
                  <span class="loop-title">{loop.title}</span>
                </span>
                <span class="loop-arrow" aria-hidden="true">
                  <ChevronRight size={15} strokeWidth={1.6} />
                </span>
              </button>
            {/each}
          </div>
          {#if background.length}
            <p class="background-line">
              <i class="status-dot running"></i>
              {#if lead?.title}<span>{lead.title}</span>{/if}
            </p>
          {/if}
        {:else if background.length}
          <p class="background-line">
            <i class="status-dot running"></i>
            {#if lead?.title}<span>{lead.title}</span>{/if}
          </p>
        {/if}
      </section>
    {:else}
      <section class="encounter" aria-label="正在交谈">
        {#if hiddenCount > 0}
          <div class="earlier-mark" aria-label={`还有 ${hiddenCount} 条更早的话`}>…</div>
        {/if}

        {#each visibleMessages as message (message.id)}
          {#if message.role === "user"}
            <article class="user-turn">
              <p>{message.text}</p>
            </article>
          {:else}
            <article class="eja-turn">
              <Markdown text={message.text} />
            </article>
          {/if}
        {/each}

        {#if optimistic && !optimisticDuplicate}
          <article class="user-turn pending-turn">
            <p>{optimistic.text}</p>
          </article>
        {/if}

        {#if pending || health.turnInFlight}
          {#if background.length}
            <p class="work-line" aria-live="polite">
              <i class="status-dot running"></i>
              {#if lead?.title}<span>{lead.title}</span>{/if}
            </p>
          {:else}
            <div class="thinking" aria-live="polite">
              <i></i><i></i><i></i>
            </div>
          {/if}
        {/if}

        {#if actions.length}
          <div class="action-stack">
            {#each actions as action (action.key)}
              {#if action.kind === "decision"}
                <section class="action-panel decision" aria-label="proposal">
                  <p class="action-copy">{action.proposal.text}</p>
                  <button
                    class="approve"
                    type="button"
                    aria-label="approve"
                    title="approve"
                    disabled={pending}
                    onclick={() => onApprove(action.proposal.id)}
                  >
                    <Check size={16} strokeWidth={1.8} />
                  </button>
                </section>
              {:else if action.kind === "review" || action.kind === "interrupted"}
                <section class="action-panel task" aria-label={action.kind}>
                  <div class="action-head">
                    <h2>{action.task.title}</h2>
                    <button
                      class="inspect"
                      type="button"
                      aria-label="record"
                      title="record"
                      onclick={() => onInspectTask(action.task)}
                    >
                      <ChevronRight size={16} strokeWidth={1.7} />
                    </button>
                  </div>
                  {#if action.task.run?.digest}
                    <p class="action-copy">{action.task.run.digest}</p>
                  {/if}
                </section>
              {/if}
            {/each}
          </div>
        {/if}
      </section>
    {/if}
  </div>

  <div class="composer-zone">
    <div class="composer-inner">
      <form
        class="composer"
        onsubmit={(event) => {
          event.preventDefault();
          submit();
        }}
      >
        <div class="composer-row">
          {#if mode === "encounter"}
            <button
              class="bye"
              type="button"
              aria-label="结束这次会面"
              disabled={pending || health.turnInFlight}
              onclick={onBye}
            >
              bye
            </button>
          {/if}
          <textarea
            bind:this={textarea}
            bind:value={draft}
            rows="1"
            placeholder={mode === "ambient" ? "说一件新的事…" : "接着说…"}
            spellcheck="false"
            disabled={pending || health.turnInFlight}
            onkeydown={(event) => {
              if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
                event.preventDefault();
                submit();
              }
            }}
          ></textarea>
          <button class="send" type="submit" aria-label="发送" disabled={!canSend}>
            <ArrowUp size={17} strokeWidth={2.1} />
          </button>
        </div>
      </form>
    </div>
  </div>
</section>

<style>
  .talk-view {
    display: flex;
    height: 100%;
    min-height: 0;
    flex-direction: column;
    overflow: hidden;
  }

  .talk-scroll {
    flex: 1;
    min-height: 0;
    overflow-x: hidden;
    overflow-y: auto;
  }

  .ambient,
  .encounter {
    width: min(100%, 860px);
    min-height: 100%;
    margin: 0 auto;
    padding: 54px 34px 30px;
  }

  .ambient {
    display: flex;
    justify-content: flex-start;
    flex-direction: column;
    padding-top: clamp(48px, 9vh, 92px);
    padding-bottom: 90px;
  }

  .ambient.quiet {
    justify-content: center;
    padding-top: 54px;
  }

  .presence-turn {
    max-width: min(88%, 720px);
    margin: 0;
    animation: turn-in 260ms cubic-bezier(0.2, 0.7, 0.2, 1) both;
  }

  .presence-turn :global(.markdown) {
    color: var(--ink);
    font-size: clamp(26px, 4.2vw, 40px);
    font-weight: 420;
    letter-spacing: -0.03em;
    line-height: 1.4;
  }

  .loops {
    margin-top: 50px;
    border-top: 1px solid var(--line);
  }

  .loop-row {
    display: grid;
    width: 100%;
    min-height: 78px;
    grid-template-columns: 8px minmax(0, 1fr) 20px;
    align-items: center;
    gap: 15px;
    padding: 14px 5px;
    border-bottom: 1px solid var(--line);
    text-align: left;
  }

  .loop-copy {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 5px;
  }

  .loop-title {
    overflow: hidden;
    color: var(--ink-soft);
    font-family: var(--serif);
    font-size: 20px;
    font-weight: 450;
    letter-spacing: -0.018em;
    line-height: 1.35;
    text-overflow: ellipsis;
    white-space: nowrap;
    transition: color 150ms ease;
  }

  .loop-row:hover .loop-title {
    color: var(--ink);
  }

  .loop-arrow {
    color: var(--faint);
    opacity: 0;
    transform: translateX(-4px);
    transition:
      opacity 160ms ease,
      transform 160ms ease,
      color 160ms ease;
  }

  .loop-row:hover .loop-arrow,
  .loop-row:focus-visible .loop-arrow {
    color: var(--ink-soft);
    opacity: 1;
    transform: none;
  }

  .background-line {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 20px 0 0;
    color: var(--faint);
    font-size: 10.5px;
  }

  .stillness {
    display: grid;
    width: 34px;
    height: 34px;
    place-items: center;
    border: 1px solid var(--line);
    border-radius: 50%;
  }

  .stillness span {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--accent);
    box-shadow: 0 0 0 5px rgba(189, 96, 66, 0.07);
    animation: dot-breathe 2.8s ease-out infinite;
  }

  .encounter {
    display: flex;
    max-width: 820px;
    justify-content: flex-end;
    flex-direction: column;
    padding-top: 42px;
  }

  .earlier-mark {
    margin: 0 0 24px;
    color: var(--faint);
    font-size: 16px;
    text-align: center;
  }

  .eja-turn {
    margin: 0 0 32px;
    color: var(--ink);
    font-family: var(--serif);
    font-size: clamp(20px, 2.7vw, 25px);
    font-weight: 400;
    letter-spacing: -0.018em;
    line-height: 1.62;
    animation: turn-in 240ms cubic-bezier(0.2, 0.7, 0.2, 1) both;
  }

  .user-turn {
    display: flex;
    justify-content: flex-end;
    margin: 0 0 26px;
    animation: turn-in 220ms cubic-bezier(0.2, 0.7, 0.2, 1) both;
  }

  .user-turn p {
    max-width: min(78%, 590px);
    margin: 0;
    padding: 9px 14px;
    border: 1px solid rgba(36, 35, 31, 0.065);
    border-radius: 16px 16px 5px 16px;
    background: #eae6dc;
    color: var(--ink-soft);
    font-size: 13.5px;
    line-height: 1.62;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .pending-turn {
    opacity: 0.56;
  }

  .thinking,
  .work-line {
    display: flex;
    align-items: center;
    gap: 5px;
    margin: 2px 0 28px;
    color: var(--muted);
    font-size: 10.5px;
  }

  .thinking i {
    width: 4px;
    height: 4px;
    border-radius: 50%;
    background: var(--accent);
    animation: thinking 1.15s ease-in-out infinite;
  }

  .thinking i:nth-child(2) {
    animation-delay: 120ms;
  }

  .thinking i:nth-child(3) {
    animation-delay: 240ms;
  }

  .work-line {
    gap: 8px;
  }

  .action-stack {
    display: flex;
    flex-direction: column;
    gap: 16px;
    margin: 2px 0 24px;
  }

  .action-panel {
    padding: 18px 0 2px;
    border-top: 1px solid var(--line);
    animation: turn-in 220ms cubic-bezier(0.2, 0.7, 0.2, 1) both;
  }

  .action-panel.decision {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    align-items: center;
    gap: 16px;
    padding: 18px 20px 17px;
    border: 1px solid rgba(189, 96, 66, 0.18);
    border-radius: 17px;
    background:
      radial-gradient(100% 120% at 0 0, rgba(255, 255, 255, 0.58), transparent 58%),
      linear-gradient(135deg, #fbf3ee, #f7ece6);
  }

  .action-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }

  .action-panel h2 {
    margin: 0;
    color: var(--ink);
    font-family: var(--serif);
    font-size: 24px;
    font-weight: 450;
    letter-spacing: -0.025em;
    line-height: 1.3;
  }

  .decision .action-copy {
    margin: 0;
    font-size: 19px;
  }

  .action-copy {
    max-width: 65ch;
    margin: 8px 0 0;
    color: var(--ink-soft);
    font-family: var(--serif);
    font-size: 17px;
    line-height: 1.58;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .approve,
  .inspect {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: 50%;
    color: var(--muted);
    transition:
      background 150ms ease,
      color 150ms ease,
      transform 150ms ease;
  }

  .approve {
    width: 34px;
    height: 34px;
    background: var(--ink);
    color: #fff;
  }

  .approve:hover:not(:disabled) {
    background: #12120f;
    transform: translateY(-1px);
  }

  .inspect {
    width: 30px;
    height: 30px;
    flex: 0 0 30px;
  }

  .inspect:hover {
    background: var(--surface-2);
    color: var(--ink);
  }

  .approve:disabled {
    opacity: 0.45;
  }

  .composer-zone {
    flex: 0 0 auto;
    padding: 14px 24px calc(13px + env(safe-area-inset-bottom)) 58px;
    background: linear-gradient(180deg, rgba(248, 247, 242, 0), rgba(248, 247, 242, 0.92) 24%, var(--paper) 58%);
  }

  .composer-inner {
    width: min(100%, 780px);
    margin: 0 auto;
  }

  .composer {
    overflow: hidden;
    border: 1px solid rgba(36, 35, 31, 0.14);
    border-radius: 21px;
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
    border-color: rgba(189, 96, 66, 0.31);
    box-shadow:
      0 14px 44px rgba(36, 35, 31, 0.09),
      0 0 0 3px rgba(189, 96, 66, 0.05);
  }

  .composer-row {
    display: flex;
    align-items: flex-end;
    gap: 5px;
    padding: 8px 9px;
  }

  .bye {
    height: 34px;
    flex: 0 0 auto;
    padding: 0 8px;
    border-radius: 10px;
    color: var(--faint);
    font-family: var(--mono);
    font-size: 10px;
    transition:
      color 150ms ease,
      background 150ms ease;
  }

  .bye:hover:not(:disabled) {
    background: var(--surface-2);
    color: var(--ink-soft);
  }

  .bye:disabled {
    opacity: 0.35;
  }

  textarea {
    min-height: 24px;
    max-height: 132px;
    flex: 1;
    resize: none;
    border: 0;
    outline: 0;
    background: transparent;
    padding: 6px 2px;
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

  textarea:disabled {
    opacity: 0.58;
  }

  .send {
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

  .send:hover:not(:disabled) {
    background: #11110e;
    transform: translateY(-1px);
  }

  .send:disabled {
    background: #d7d3c8;
    opacity: 0.72;
  }

  @keyframes turn-in {
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
    .ambient,
    .encounter {
      padding: 36px 20px 22px;
    }

    .ambient {
      padding-bottom: 54px;
    }

    .loops {
      margin-top: 32px;
    }

    .loop-title {
      font-size: 18px;
    }

    .composer-zone {
      padding: 11px 12px calc(10px + env(safe-area-inset-bottom)) 54px;
    }

    .eja-turn {
      font-size: 20px;
    }
  }

  @media (max-width: 480px) {
    .ambient,
    .encounter {
      padding-inline: 17px;
    }

    .loop-row {
      grid-template-columns: 8px minmax(0, 1fr);
    }

    .loop-arrow {
      display: none;
    }

    .user-turn p {
      max-width: 88%;
    }

    .composer-zone {
      padding-left: 12px;
      padding-right: 12px;
    }

    .bye {
      padding-inline: 6px;
    }
  }
</style>
