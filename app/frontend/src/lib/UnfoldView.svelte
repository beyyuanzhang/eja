<script lang="ts">
  import { Clock3, LocateFixed, Waypoints } from "lucide-svelte";
  import { tick } from "svelte";
  import { axisTime, duration, niceStep, taskEnd, taskStatus, truncate } from "./format";
  import type { Relation, Task } from "./types";

  interface Props {
    tasks: Task[];
    relations: Relation[];
    counts: {
      running: number;
      waiting: number;
    };
    now: number;
    visible: boolean;
    selectedTask: number | null;
    onSelect: (id: number) => void;
  }

  let { tasks, relations, counts, now, visible, selectedTask, onSelect }: Props =
    $props();

  let viewport = $state<HTMLDivElement>();
  let width = $state(1000);
  let measured = false;

  const labelWidth = $derived(width <= 640 ? 112 : width <= 900 ? 138 : 196);

  interface Lane {
    id: string;
    title: string;
    meta: string;
    group: Task | null;
    tasks: Task[];
  }

  interface PlacedCard {
    task: Task;
    x: number;
    y: number;
    width: number;
    start: number;
    end: number;
  }

  interface LaneGeometry {
    lane: Lane;
    height: number;
    top: number;
    cards: PlacedCard[];
  }

  const byId = $derived(new Map(tasks.map((task) => [task.id, task])));

  const lanes = $derived.by<Lane[]>(() => {
    const groups = tasks
      .filter((task) => task.kind === "group")
      .sort((a, b) => a.created_at - b.created_at || a.id - b.id);
    const childIds = new Set(groups.flatMap((group) => group.children || []));
    const roots = tasks
      .filter((task) => task.kind === "aja" && !childIds.has(task.id))
      .sort((a, b) => a.created_at - b.created_at || a.id - b.id);

    const list: Lane[] = [];
    if (roots.length) {
      const active = roots.filter((task) => task.status === "running" || task.status === "waiting").length;
      list.push({
        id: "independent",
        title: "独立工作",
        meta: active ? `${roots.length} 件 · ${active} 未结` : `${roots.length} 件`,
        group: null,
        tasks: roots,
      });
    }

    for (const group of groups) {
      const children = (group.children || [])
        .map((id) => byId.get(id))
        .filter((task): task is Task => Boolean(task))
        .sort((a, b) => a.created_at - b.created_at || a.id - b.id);
      const active = children.filter((task) => task.status === "running" || task.status === "waiting").length;
      const done = children.filter((task) => task.status === "done").length;
      list.push({
        id: `group-${group.id}`,
        title: group.title,
        meta: children.length
          ? `${done}/${children.length} 收好${active ? ` · ${active} 未结` : ""}`
          : "还没有展开",
        group,
        tasks: children,
      });
    }
    return list;
  });

  const geometry = $derived.by(() => {
    const starts = tasks.map((task) => task.created_at).filter(Number.isFinite);
    const ends = tasks.map((task) => taskEnd(task, now)).filter(Number.isFinite);
    if (!starts.length) {
      return {
        lanes: [] as LaneGeometry[],
        ticks: [] as number[],
        min: 0,
        span: 0,
        xOf: (_time: number) => 0,
        totalWidth: 0,
        totalHeight: 0,
        nowX: 0,
        step: 3600,
        rulerHeight: 42,
        cardHeight: 54,
      };
    }

    let min = Math.min(...starts);
    let max = Math.max(...ends, now);
    const baseRange = Math.max(180, max - min);
    const padding = Math.max(150, Math.min(3600, baseRange * 0.07));
    min -= padding;
    max += padding;
    const span = max - min;
    const viewportWidth = Math.max(560, width - labelWidth - 34);
    const trackWidth = Math.max(viewportWidth, Math.min(4600, Math.ceil(span * 0.085)));
    const pxPerSecond = trackWidth / span;
    const xOf = (time: number) => (time - min) * pxPerSecond;

    const rulerHeight = 42;
    const cardMin = width <= 640 ? 152 : 166;
    const gap = 10;
    const rowHeight = 69;
    const topPadding = 15;
    const cardHeight = 54;
    const laneList: LaneGeometry[] = [];
    let top = rulerHeight;

    for (const lane of lanes) {
      const rowEnds: number[] = [];
      const cards: PlacedCard[] = [];
      for (const task of lane.tasks) {
        const start = task.created_at;
        const end = Math.max(taskEnd(task, now), start + 60);
        const x = xOf(start);
        const cardWidth = Math.max(cardMin, (end - start) * pxPerSecond);
        let row = rowEnds.findIndex((rowEnd) => x >= rowEnd + gap);
        if (row === -1) {
          row = rowEnds.length;
          rowEnds.push(0);
        }
        rowEnds[row] = x + cardWidth;
        cards.push({
          task,
          x,
          y: top + topPadding + row * rowHeight,
          width: cardWidth,
          start,
          end,
        });
      }

      const height = Math.max(94, topPadding * 2 + Math.max(1, rowEnds.length) * rowHeight);
      laneList.push({ lane, cards, height, top });
      top += height;
    }

    const step = niceStep(span);
    const firstTick = Math.ceil(min / step) * step;
    const ticks: number[] = [];
    for (let tick = firstTick; tick <= max; tick += step) ticks.push(tick);

    return {
      lanes: laneList,
      ticks,
      min,
      span,
      xOf,
      totalWidth: labelWidth + trackWidth,
      totalHeight: top + 1,
      nowX: labelWidth + xOf(now),
      step,
      rulerHeight,
      cardHeight,
    };
  });

  const edges = $derived.by(() => {
    const positions = new Map<number, PlacedCard>();
    for (const lane of geometry.lanes) {
      for (const card of lane.cards) positions.set(card.task.id, card);
    }
    return relations
      .filter((relation) => relation.kind === "depends_on" || relation.kind === "continues")
      .flatMap((relation) => {
        const from = positions.get(relation.from);
        const to = positions.get(relation.to);
        if (!from || !to) return [];
        const x1 = labelWidth + from.x + from.width;
        const y1 = from.y + geometry.cardHeight / 2;
        const x2 = labelWidth + to.x;
        const y2 = to.y + geometry.cardHeight / 2;
        const bend = Math.max(28, Math.min(92, Math.abs(x2 - x1) * 0.36));
        return [{
          key: `${relation.kind}-${relation.from}-${relation.to}`,
          kind: relation.kind,
          path: `M ${x1} ${y1} C ${x1 + bend} ${y1}, ${x2 - bend} ${y2}, ${x2} ${y2}`,
        }];
      });
  });

  const summary = $derived(
    counts.running
      ? `${counts.running} 个在办 · ${tasks.length} 个节点`
      : counts.waiting
        ? `${counts.waiting} 个等判断 · ${tasks.length} 个节点`
        : `${tasks.length} 个节点 · 都收好了`,
  );

  function scrollToNow(behavior: ScrollBehavior = "smooth") {
    if (!viewport) return;
    viewport.scrollTo({
      left: Math.max(0, geometry.nowX - viewport.clientWidth * 0.66),
      behavior,
    });
  }

  $effect(() => {
    const element = viewport;
    if (!element) return;
    const observer = new ResizeObserver(([entry]) => {
      width = Math.max(320, entry.contentRect.width);
      if (!measured) {
        measured = true;
        void tick().then(() => scrollToNow("auto"));
      }
    });
    observer.observe(element);
    return () => observer.disconnect();
  });
</script>

<section class="unfold-view" aria-label="展开工作">
  <header class="timeline-head">
    <div class="heading">
      <div class="heading-mark"><Waypoints size={16} strokeWidth={1.7} /></div>
      <div>
        <h1>展开</h1>
        <p>{summary}</p>
      </div>
    </div>
    <div class="head-tools">
      <div class="legend" aria-label="状态图例">
        <span><i class="status-dot running"></i>在办</span>
        <span><i class="status-dot waiting"></i>等判断</span>
        <span><i class="status-dot done"></i>已收下</span>
        <span><i class="status-dot failed"></i>未通过</span>
      </div>
      <button class="now-button" type="button" onclick={() => scrollToNow()}>
        <LocateFixed size={13} />
        现在
      </button>
    </div>
  </header>

  {#if !tasks.length}
    <div class="empty">
      <div class="empty-orb"><span></span></div>
      <h2>还没有展开</h2>
    </div>
  {:else}
    <div class="timeline-scroll" bind:this={viewport}>
      <div class="canvas" style:width={`${geometry.totalWidth}px`}>
        <div class="ruler" style:height={`${geometry.rulerHeight}px`}>
          <div class="ruler-corner" style:width={`${labelWidth}px`}>
            <span>工作线</span>
          </div>
          <div class="ruler-track">
            {#each geometry.ticks as tick}
              <span class="tick" style:left={`${geometry.xOf(tick)}px`}>
                {axisTime(tick, geometry.span)}
              </span>
            {/each}
          </div>
        </div>

        <div class="lanes">
          {#each geometry.lanes as lane (lane.lane.id)}
            <div class="lane" style:height={`${lane.height}px`}>
              <div class="lane-label" style:width={`${labelWidth}px`}>
                <div class="lane-title">{lane.lane.title}</div>
                <div class="lane-meta">
                  {#if lane.lane.group}
                    <i class="status-dot {taskStatus(lane.lane.group).tone}"></i>
                  {/if}
                  <span>{lane.lane.meta}</span>
                </div>
              </div>
              <div class="lane-track">
                {#each geometry.ticks as tick}
                  <span class="grid-line" style:left={`${geometry.xOf(tick)}px`}></span>
                {/each}
                {#each lane.cards as card (card.task.id)}
                  {@const status = taskStatus(card.task)}
                  {@const digest = truncate(
                    card.task.run?.digest || card.task.close?.note || card.task.request || status.label,
                    90,
                  )}
                  <button
                    class="task-card {status.tone}"
                    class:selected={selectedTask === card.task.id}
                    type="button"
                    style:left={`${card.x}px`}
                    style:top={`${card.y - lane.top}px`}
                    style:width={`${card.width}px`}
                    onclick={() => onSelect(card.task.id)}
                    title={card.task.title}
                  >
                    <span class="card-head">
                      <i class="status-dot {status.tone}"></i>
                      <strong>{card.task.title}</strong>
                      <span class="card-duration">{duration(card.end - card.start)}</span>
                    </span>
                    <span class="card-meta">{digest}</span>
                    {#if status.tone === "running"}
                      <span class="running-line"></span>
                    {/if}
                  </button>
                {/each}
              </div>
            </div>
          {/each}
        </div>

        <div
          class="now-line"
          style:left={`${geometry.nowX}px`}
          style:top={`${geometry.rulerHeight}px`}
        >
          <span><Clock3 size={10} />现在</span>
        </div>

        <svg
          class="edges"
          width={geometry.totalWidth}
          height={geometry.totalHeight}
          viewBox={`0 0 ${geometry.totalWidth} ${geometry.totalHeight}`}
          aria-hidden="true"
        >
          <defs>
            <marker
              id="eja-edge-arrow"
              viewBox="0 0 8 8"
              refX="7"
              refY="4"
              markerWidth="5.5"
              markerHeight="5.5"
              orient="auto-start-reverse"
            >
              <path d="M0 0L8 4L0 8Z" fill="#c5c0b5" />
            </marker>
          </defs>
          {#each edges as edge (edge.key)}
            <path
              class="edge {edge.kind}"
              d={edge.path}
              marker-end="url(#eja-edge-arrow)"
            />
          {/each}
        </svg>
      </div>
    </div>
  {/if}
</section>

<style>
  .unfold-view {
    display: flex;
    height: 100%;
    min-height: 0;
    flex-direction: column;
    background: color-mix(in srgb, var(--paper) 86%, white);
  }

  .timeline-head {
    display: flex;
    min-height: 74px;
    flex: 0 0 74px;
    align-items: center;
    justify-content: space-between;
    gap: 22px;
    padding: 0 28px;
    border-bottom: 1px solid var(--line);
    background: color-mix(in srgb, var(--paper) 86%, white);
  }

  .heading {
    display: flex;
    align-items: center;
    gap: 12px;
    min-width: 0;
  }

  .heading-mark {
    display: grid;
    width: 34px;
    height: 34px;
    flex: 0 0 34px;
    place-items: center;
    border: 1px solid var(--line);
    border-radius: 11px;
    background: var(--surface);
    color: var(--accent-deep);
    box-shadow: var(--shadow-xs);
  }

  h1 {
    margin: 0;
    font-family: var(--serif);
    font-size: 22px;
    font-weight: 470;
    letter-spacing: -0.025em;
  }

  .heading p {
    margin: 1px 0 0;
    color: var(--muted);
    font-size: 10.5px;
  }

  .head-tools {
    display: flex;
    align-items: center;
    gap: 18px;
  }

  .legend {
    display: flex;
    align-items: center;
    gap: 14px;
    color: var(--muted);
    font-size: 10px;
  }

  .legend span {
    display: inline-flex;
    align-items: center;
    gap: 5px;
  }

  .legend .status-dot {
    width: 6px;
    height: 6px;
    flex-basis: 6px;
  }

  .now-button {
    display: inline-flex;
    height: 31px;
    align-items: center;
    gap: 6px;
    padding: 0 11px;
    border: 1px solid var(--line-strong);
    border-radius: 999px;
    background: var(--surface);
    color: var(--ink-soft);
    font-size: 10.5px;
    box-shadow: var(--shadow-xs);
  }

  .now-button:hover {
    border-color: rgba(36, 35, 31, 0.24);
    background: #fff;
  }

  .timeline-scroll {
    flex: 1;
    min-height: 0;
    overflow: auto;
    overscroll-behavior: contain;
  }

  .canvas {
    position: relative;
    min-height: 100%;
    padding-bottom: 70px;
  }

  .ruler {
    position: sticky;
    top: 0;
    z-index: 12;
    display: flex;
    border-bottom: 1px solid var(--line);
    background: color-mix(in srgb, var(--paper) 88%, white);
    backdrop-filter: blur(14px);
  }

  .ruler-corner {
    position: sticky;
    left: 0;
    z-index: 14;
    display: flex;
    flex: 0 0 auto;
    align-items: center;
    padding: 0 18px;
    border-right: 1px solid var(--line);
    background: color-mix(in srgb, var(--paper) 93%, white);
    color: var(--faint);
    font-size: 9px;
    letter-spacing: 0.12em;
    text-transform: uppercase;
  }

  .ruler-track {
    position: relative;
    flex: 1;
  }

  .tick {
    position: absolute;
    top: 15px;
    padding-left: 5px;
    color: var(--faint);
    font-family: var(--mono);
    font-size: 8.5px;
    white-space: nowrap;
  }

  .lanes {
    position: relative;
    z-index: 3;
  }

  .lane {
    display: flex;
    border-bottom: 1px solid var(--line);
  }

  .lane:last-child {
    border-bottom: 0;
  }

  .lane-label {
    position: sticky;
    left: 0;
    z-index: 10;
    flex: 0 0 auto;
    padding: 18px;
    border-right: 1px solid var(--line);
    background:
      linear-gradient(90deg, color-mix(in srgb, var(--paper) 98%, white) 82%, transparent),
      color-mix(in srgb, var(--paper) 96%, white);
  }

  .lane-title {
    display: -webkit-box;
    overflow: hidden;
    color: var(--ink);
    font-size: 12px;
    font-weight: 620;
    line-height: 1.4;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 2;
    line-clamp: 2;
  }

  .lane-meta {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-top: 7px;
    color: var(--muted);
    font-size: 9.5px;
  }

  .lane-meta .status-dot {
    width: 6px;
    height: 6px;
    flex-basis: 6px;
  }

  .lane-track {
    position: relative;
    flex: 1;
  }

  .grid-line {
    position: absolute;
    top: 0;
    bottom: 0;
    border-left: 1px solid color-mix(in srgb, var(--line) 60%, transparent);
    pointer-events: none;
  }

  .task-card {
    position: absolute;
    display: flex;
    height: 54px;
    flex-direction: column;
    justify-content: center;
    gap: 3px;
    overflow: hidden;
    padding: 7px 11px 7px 12px;
    border: 1px solid var(--line-strong);
    border-radius: 13px;
    background: color-mix(in srgb, var(--surface) 96%, transparent);
    color: var(--ink);
    text-align: left;
    box-shadow:
      0 2px 6px rgba(36, 35, 31, 0.035),
      0 1px 1px rgba(255, 255, 255, 0.8) inset;
    transition:
      border-color 150ms ease,
      box-shadow 150ms ease,
      transform 150ms ease;
  }

  .task-card:hover {
    z-index: 5;
    border-color: rgba(36, 35, 31, 0.25);
    box-shadow: 0 9px 24px rgba(36, 35, 31, 0.09);
    transform: translateY(-1px);
  }

  .task-card.selected {
    border-color: rgba(189, 96, 66, 0.62);
    box-shadow:
      0 0 0 2px rgba(189, 96, 66, 0.1),
      0 9px 24px rgba(36, 35, 31, 0.08);
  }

  .card-head {
    display: flex;
    min-width: 0;
    align-items: center;
    gap: 6px;
  }

  .card-head strong {
    overflow: hidden;
    flex: 1;
    font-size: 11px;
    font-weight: 650;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .card-duration {
    color: var(--faint);
    font-family: var(--mono);
    font-size: 8.5px;
    white-space: nowrap;
  }

  .card-meta {
    overflow: hidden;
    color: var(--muted);
    font-size: 9.5px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .running-line {
    position: absolute;
    right: 11px;
    bottom: 0;
    left: 12px;
    height: 1px;
    overflow: hidden;
    background: rgba(189, 96, 66, 0.13);
  }

  .running-line::after {
    position: absolute;
    top: 0;
    width: 38%;
    height: 1px;
    background: var(--accent);
    content: "";
    animation: running-sweep 2.8s ease-in-out infinite;
  }

  .now-line {
    position: absolute;
    bottom: 0;
    z-index: 8;
    border-left: 1px dashed rgba(189, 96, 66, 0.55);
    pointer-events: none;
  }

  .now-line span {
    position: absolute;
    top: 7px;
    left: 7px;
    display: inline-flex;
    height: 20px;
    align-items: center;
    gap: 4px;
    padding: 0 6px;
    border: 1px solid rgba(189, 96, 66, 0.13);
    border-radius: 5px;
    background: var(--paper);
    color: var(--accent-deep);
    font-size: 8.5px;
    white-space: nowrap;
  }

  .edges {
    position: absolute;
    inset: 0;
    z-index: 5;
    overflow: visible;
    pointer-events: none;
  }

  .edge {
    fill: none;
    stroke: #c9c4b9;
    stroke-width: 1.1;
    opacity: 0.7;
  }

  .edge.continues {
    stroke-dasharray: 5 4;
  }

  .empty {
    display: flex;
    flex: 1;
    align-items: center;
    justify-content: center;
    flex-direction: column;
    padding: 40px;
    text-align: center;
  }

  .empty-orb {
    display: grid;
    width: 52px;
    height: 52px;
    margin-bottom: 15px;
    place-items: center;
    border: 1px solid var(--line);
    border-radius: 50%;
    background: var(--surface);
    box-shadow: var(--shadow-sm);
  }

  .empty-orb span {
    width: 9px;
    height: 9px;
    border: 1px solid var(--line-strong);
    border-radius: 50%;
  }

  .empty h2 {
    margin: 0;
    font-family: var(--serif);
    font-size: 25px;
    font-weight: 450;
    letter-spacing: -0.025em;
  }

  @keyframes running-sweep {
    0% {
      left: -35%;
    }
    60%,
    100% {
      left: 100%;
    }
  }

  @media (max-width: 900px) {
    .timeline-head {
      padding: 0 18px;
    }

    .legend {
      display: none;
    }
  }

  @media (max-width: 640px) {
    .timeline-head {
      min-height: 68px;
      flex-basis: 68px;
      padding: 0 14px;
    }

    .heading-mark {
      display: none;
    }

    h1 {
      font-size: 20px;
    }

    .heading p {
      font-size: 9.5px;
    }

    .lane-label {
      padding: 14px 11px;
    }

    .lane-title {
      font-size: 11px;
    }

    .ruler-corner {
      padding: 0 10px;
      font-size: 8px;
    }
  }
</style>
