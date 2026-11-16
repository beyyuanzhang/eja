<script lang="ts">
  import {
    Braces,
    Check,
    ChevronRight,
    Copy,
    ExternalLink,
    MessageCircle,
    X,
  } from "lucide-svelte";
  import { fade, fly } from "svelte/transition";
  import { dateTime, duration, pretty, relative, taskStatus } from "./format";
  import type { Task } from "./types";

  interface Props {
    task: Task;
    tasks: Task[];
    onClose: () => void;
    onAsk: (task: Task) => void;
    onRaw: (address: string) => void;
    onSelectRelated: (id: number) => void;
  }

  let { task, tasks, onClose, onAsk, onRaw, onSelectRelated }: Props = $props();

  let copied = $state("");
  const status = $derived(taskStatus(task));
  const isGroup = $derived(task.kind === "group");
  const children = $derived(
    (task.children || [])
      .map((id) => tasks.find((candidate) => candidate.id === id))
      .filter((candidate): candidate is Task => Boolean(candidate)),
  );
  const events = $derived(task.event_ids?.slice(0, 12) ?? []);
  const run = $derived(task.run);
  const evidence = $derived(run?.evidence);

  const milestones = $derived(
    [
      { label: "开始", at: task.created_at },
      { label: "交给 AJA", at: task.assigned_at },
      { label: "回报", at: task.reported_at },
      { label: "判断", at: task.closed_at },
    ].filter((item): item is { label: string; at: number } => Boolean(item.at)),
  );

  async function copy(value: string, key: string) {
    try {
      await navigator.clipboard.writeText(value);
      copied = key;
      setTimeout(() => {
        if (copied === key) copied = "";
      }, 1500);
    } catch {
      copied = "";
    }
  }
</script>

<div class="sheet-layer">
  <button class="sheet-backdrop" type="button" aria-label="关闭详情" onclick={onClose} transition:fade={{ duration: 160 }}></button>
  <aside
    class="task-sheet"
    aria-label="工作详情"
    transition:fly={{ x: 28, duration: 240, opacity: 1 }}
  >
    <div class="sheet-scroll">
      <header class="sheet-head">
        <div class="kicker">
          <span>{isGroup ? "GROUP" : "AJA"} · #{task.id}</span>
          {#if task.origin}
            <span class="origin">来自 · {task.origin}</span>
          {/if}
        </div>
        <div class="title-row">
          <h2>{task.title}</h2>
          <button class="icon-button close" type="button" aria-label="关闭" onclick={onClose}>
            <X size={17} />
          </button>
        </div>
        <div class="status-row">
          <span class="status-pill {status.tone}">
            <i class="status-dot {status.tone}"></i>
            {status.label}
          </span>
          <span>{relative(task.created_at)}</span>
          {#if task.closed_at}
            <span class="dot-separator"></span>
            <span>{duration(task.closed_at - task.created_at)}</span>
          {/if}
        </div>
      </header>

      <section class="milestones" aria-label="过程">
        {#each milestones as milestone, index}
          <div class="milestone">
            <span class="milestone-dot" class:current={index === milestones.length - 1}></span>
            <strong>{milestone.label}</strong>
            <time>{dateTime(milestone.at)}</time>
          </div>
        {/each}
      </section>

      {#if !isGroup}
        <section class="section">
          <div class="section-title">
            <span>交代</span>
          </div>
          <p class="copy">{task.request || "没有另写委托。"}</p>
        </section>

        <section class="section">
          <div class="section-title">
            <span>完成条件</span>
          </div>
          <p class="copy">{task.acceptance || "没有单独写验收条件。"}</p>
        </section>

        <section class="section">
          <div class="section-title">
            <span>三种声音</span>
            <small>不是把过程混成一句结论</small>
          </div>
          <div class="voices">
            <article class="voice claim">
              <div class="voice-head">
                <span>它说</span>
                <i></i>
              </div>
              <p>{run?.digest || "还没有回报。"}</p>
            </article>

            <article class="voice seen">
              <div class="voice-head">
                <span>宿主看见</span>
                {#if run?.evidence_id}
                  <em class="verified"><Check size={10} />已核验</em>
                {:else}
                  <em>未核验</em>
                {/if}
              </div>
              {#if evidence}
                <details class="evidence">
                  <summary>
                    <Braces size={12} />
                    验收契约
                    <ChevronRight size={12} class="chevron" />
                  </summary>
                  <pre>{pretty(evidence)}</pre>
                </details>
                {#if run?.evidence_id}
                  <div class="observed">
                    <span>实际观察</span>
                    <pre>{pretty(run.evidence_result)}</pre>
                  </div>
                {:else}
                  <p class="voice-note">宿主还没有看到契约要求的证据。</p>
                {/if}
              {:else}
                <p class="voice-note">这件工作没有要求宿主另外核验。</p>
              {/if}
            </article>

            <article class="voice judged">
              <div class="voice-head">
                <span>eja 判</span>
                {#if task.close}
                  <em class:negative={!task.close.accepted}>
                    {task.close.accepted ? "收下" : "退回"}
                  </em>
                {:else}
                  <em>还没判</em>
                {/if}
              </div>
              <p>{task.close?.note || (task.close ? "没有另写理由。" : "等前面的声音都到齐。")}</p>
            </article>
          </div>
        </section>

        {#if run?.reconcile}
          <section class="section">
            <div class="section-title"><span>世界</span></div>
            <pre class="json">{pretty(run.reconcile)}</pre>
          </section>
        {/if}
      {:else}
        <section class="section">
          <div class="section-title">
            <span>组内节点</span>
            <small>组只聚合，不执行</small>
          </div>
          <div class="children">
            {#each children as child (child.id)}
              {@const childStatus = taskStatus(child)}
              <button type="button" onclick={() => onSelectRelated(child.id)}>
                <i class="status-dot {childStatus.tone}"></i>
                <span>{child.title}</span>
                <small>{childStatus.label}</small>
                <ChevronRight size={13} />
              </button>
            {:else}
              <p class="voice-note">还没有节点放进来。</p>
            {/each}
          </div>
        </section>
      {/if}

      <section class="section address-section">
        <div class="section-title">
          <span>可复查的地址</span>
          <small>每句话都能落回原件</small>
        </div>
        <div class="addresses">
          <button class="chip" type="button" onclick={() => onRaw(`log:task:${task.id}`)}>
            log:task:{task.id}
          </button>
          {#each events as id}
            <button class="chip" type="button" onclick={() => onRaw(`log:event:${id}`)}>
              event:{id}
            </button>
          {/each}
          {#if !isGroup}
            <button class="chip" type="button" onclick={() => onRaw(`record:${task.id}/aja.json`)}>
              record:{task.id}/aja.json
            </button>
            <button class="chip" type="button" onclick={() => onRaw(`record:${task.id}/context.jsonl`)}>
              context.jsonl
            </button>
          {/if}
        </div>
      </section>
    </div>

    <footer class="sheet-foot">
      <button class="primary-button ask" type="button" onclick={() => onAsk(task)}>
        <MessageCircle size={14} />
        问这个
      </button>
      <button class="secondary-button" type="button" onclick={() => onRaw(`log:task:${task.id}`)}>
        <ExternalLink size={13} />
        原始记录
      </button>
      <button
        class="icon-button copy-address"
        type="button"
        title={copied === "id" ? "已复制" : "复制节点编号"}
        aria-label="复制节点编号"
        onclick={() => copy(`#${task.id} ${task.title}`, "id")}
      >
        {#if copied === "id"}
          <Check size={14} />
        {:else}
          <Copy size={14} />
        {/if}
      </button>
    </footer>
  </aside>
</div>

<style>
  .sheet-layer {
    position: fixed;
    inset: 0;
    z-index: 60;
    display: flex;
    justify-content: flex-end;
  }

  .sheet-backdrop {
    position: absolute;
    inset: 0;
    background: rgba(36, 35, 31, 0.18);
    backdrop-filter: blur(2px);
  }

  .task-sheet {
    position: relative;
    display: flex;
    width: min(474px, 100vw);
    height: 100%;
    flex-direction: column;
    overflow: hidden;
    border-left: 1px solid rgba(36, 35, 31, 0.11);
    background: #fbfaf6;
    box-shadow: -28px 0 80px rgba(36, 35, 31, 0.12);
  }

  .sheet-scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 30px 30px 42px;
  }

  .sheet-head {
    padding-bottom: 18px;
  }

  .kicker {
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--faint);
    font-family: var(--mono);
    font-size: 8.5px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
  }

  .origin {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    text-transform: none;
  }

  .origin::before {
    margin-right: 8px;
    content: "·";
  }

  .title-row {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
    margin-top: 7px;
  }

  h2 {
    margin: 0;
    padding-top: 2px;
    color: var(--ink);
    font-family: var(--serif);
    font-size: 29px;
    font-weight: 480;
    letter-spacing: -0.03em;
    line-height: 1.22;
  }

  .close {
    margin: -3px -7px 0 0;
  }

  .status-row {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 12px;
    color: var(--muted);
    font-size: 10.5px;
  }

  .dot-separator {
    width: 2px;
    height: 2px;
    border-radius: 50%;
    background: var(--faint);
  }

  .status-pill {
    display: inline-flex;
    height: 24px;
    align-items: center;
    gap: 6px;
    padding: 0 9px;
    border: 1px solid var(--line);
    border-radius: 999px;
    background: var(--surface);
    color: var(--ink-soft);
    font-size: 10px;
  }

  .milestones {
    display: flex;
    overflow-x: auto;
    gap: 0;
    margin: 1px 0 3px;
    padding: 13px 0 17px;
    border-top: 1px solid var(--line);
    border-bottom: 1px solid var(--line);
  }

  .milestone {
    position: relative;
    display: flex;
    min-width: 92px;
    flex: 1 0 auto;
    align-items: flex-start;
    flex-direction: column;
    gap: 2px;
    padding-left: 14px;
  }

  .milestone::before {
    position: absolute;
    top: 5px;
    right: 0;
    left: 14px;
    height: 1px;
    background: var(--line);
    content: "";
  }

  .milestone-dot {
    position: relative;
    z-index: 1;
    width: 7px;
    height: 7px;
    margin: 2px 0 3px;
    border: 1px solid var(--line-strong);
    border-radius: 50%;
    background: var(--surface);
  }

  .milestone-dot.current {
    border-color: var(--accent);
    background: var(--accent);
  }

  .milestone strong {
    color: var(--ink-soft);
    font-size: 9.5px;
    font-weight: 600;
  }

  .milestone time {
    color: var(--faint);
    font-family: var(--mono);
    font-size: 8px;
  }

  .section {
    padding: 21px 0;
    border-bottom: 1px solid var(--line);
  }

  .section:last-child {
    border-bottom: 0;
  }

  .section-title {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 14px;
    margin-bottom: 10px;
  }

  .section-title span {
    color: var(--muted);
    font-size: 9.5px;
    font-weight: 640;
    letter-spacing: 0.11em;
    text-transform: uppercase;
  }

  .section-title small {
    color: var(--faint);
    font-size: 8.5px;
  }

  .copy {
    margin: 0;
    color: var(--ink-soft);
    font-size: 12.5px;
    line-height: 1.68;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .voices {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .voice {
    padding: 13px 14px;
    border: 1px solid var(--line);
    border-radius: 13px;
    background: color-mix(in srgb, var(--surface) 66%, transparent);
  }

  .voice.claim {
    border-color: rgba(189, 96, 66, 0.16);
    background: rgba(246, 232, 225, 0.54);
  }

  .voice.seen {
    background: rgba(255, 254, 250, 0.82);
  }

  .voice.judged {
    background: rgba(244, 242, 236, 0.65);
  }

  .voice-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 7px;
  }

  .voice-head > span {
    color: var(--ink-soft);
    font-size: 9.5px;
    font-weight: 650;
    letter-spacing: 0.07em;
  }

  .voice-head em {
    color: var(--faint);
    font-size: 8.5px;
    font-style: normal;
  }

  .voice-head em.negative {
    color: var(--danger);
  }

  .voice-head em.verified {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    color: var(--ink-soft);
  }

  .voice p {
    margin: 0;
    color: var(--ink-soft);
    font-size: 12px;
    line-height: 1.62;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .voice-note {
    color: var(--muted) !important;
    font-size: 11px !important;
  }

  .evidence {
    border-top: 1px solid var(--line);
    margin-top: 2px;
    padding-top: 7px;
  }

  .evidence summary {
    display: flex;
    align-items: center;
    gap: 5px;
    color: var(--muted);
    cursor: pointer;
    font-size: 10px;
    list-style: none;
  }

  .evidence summary::-webkit-details-marker {
    display: none;
  }

  .evidence .chevron {
    margin-left: auto;
    transition: transform 150ms ease;
  }

  pre {
    margin: 8px 0 0;
    padding: 10px 11px;
    overflow: auto;
    border: 1px solid var(--line);
    border-radius: 9px;
    background: rgba(243, 241, 235, 0.8);
    color: var(--ink-soft);
    font-family: var(--mono);
    font-size: 9.5px;
    line-height: 1.55;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .observed {
    margin-top: 8px;
  }

  .observed > span {
    color: var(--faint);
    font-size: 8.5px;
  }

  .json {
    max-height: 210px;
  }

  .children {
    display: flex;
    flex-direction: column;
    gap: 5px;
  }

  .children button {
    display: flex;
    width: 100%;
    min-height: 36px;
    align-items: center;
    gap: 8px;
    padding: 0 9px;
    border: 1px solid var(--line);
    border-radius: 9px;
    background: rgba(255, 254, 250, 0.55);
    text-align: left;
  }

  .children button:hover {
    border-color: var(--line-strong);
    background: var(--surface);
  }

  .children button > span {
    overflow: hidden;
    flex: 1;
    color: var(--ink-soft);
    font-size: 11.5px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .children button > small {
    color: var(--faint);
    font-size: 9px;
  }

  .addresses {
    display: flex;
    flex-wrap: wrap;
    gap: 5px;
  }

  .sheet-foot {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 13px 30px calc(13px + env(safe-area-inset-bottom));
    border-top: 1px solid var(--line);
    background: rgba(251, 250, 246, 0.92);
    backdrop-filter: blur(14px);
  }

  .sheet-foot .ask {
    gap: 6px;
  }

  .copy-address {
    margin-left: auto;
  }

  @media (max-width: 560px) {
    .task-sheet {
      width: 100%;
    }

    .sheet-scroll {
      padding: 24px 21px 34px;
    }

    .sheet-foot {
      padding-inline: 21px;
    }

    h2 {
      font-size: 27px;
    }
  }
</style>
