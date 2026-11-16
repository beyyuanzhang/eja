<script lang="ts">
  import { onMount } from "svelte";
  import { AlertCircle, Settings2, X } from "lucide-svelte";
  import { fly } from "svelte/transition";
  import RawDialog from "./lib/RawDialog.svelte";
  import SettingsDrawer from "./lib/SettingsDrawer.svelte";
  import TalkView from "./lib/TalkView.svelte";
  import TaskSheet from "./lib/TaskSheet.svelte";
  import UnfoldView from "./lib/UnfoldView.svelte";
  import {
    fetchRaw,
    fetchSettings,
    fetchState,
    friendlyError,
    saveSettings,
    sendTurn,
  } from "./lib/api";
  import { leadTask } from "./lib/format";
  import type {
    AppState,
    ComposerSeed,
    OptimisticMessage,
    RawRecord,
    SettingsView,
    Task,
    ViewName,
  } from "./lib/types";

  const READ_KEY = "eja.readThrough";
  const VIEW_KEY = "eja.view";

  let appState = $state<AppState | null>(null);
  let view = $state<ViewName>("talk");
  let initialLoading = $state(true);
  let fatal = $state("");
  let pending = $state(false);
  let optimistic = $state<OptimisticMessage | null>(null);
  let focus = $state<number | null>(null);
  let composerSeed = $state<ComposerSeed | null>(null);
  let dismissedProposalIds = $state<Set<number>>(new Set());
  let selectedTaskId = $state<number | null>(null);
  let raw = $state<RawRecord | null>(null);
  let settingsOpen = $state(false);
  let settings = $state<SettingsView | null>(null);
  let settingsLoading = $state(false);
  let settingsLoadError = $state("");
  let lastRead = $state(readStoredNumber(READ_KEY));
  let toastText = $state("");
  let toastError = $state(false);
  let toastTimer: ReturnType<typeof setTimeout> | null = null;
  let stateFetch: Promise<void> | null = null;

  const activeProposals = $derived(
    appState?.proposals.filter((proposal) => !dismissedProposalIds.has(proposal.id)) ?? [],
  );
  const selectedTask = $derived(
    selectedTaskId == null
      ? null
      : (appState?.tasks.find((task) => task.id === selectedTaskId) ?? null),
  );
  const unreadCount = $derived(
    appState
      ? appState.messages.filter((message) => message.role === "eja" && message.id > lastRead).length
      : 0,
  );
  const pollDelay = $derived(pending || appState?.health.turnInFlight ? 2000 : 5000);

  const ambient = $derived.by(() => {
    if (!appState) return { tone: "open", text: "正在靠近" };
    if (!appState.health.binary) return { tone: "failed", text: "没有找到 eja" };
    if (!appState.health.model) return { tone: "waiting", text: "模型未接上" };
    if (pending || appState.health.turnInFlight) return { tone: "running", text: "它正在办" };
    if (activeProposals.length) return { tone: "waiting", text: "等你决定" };
    const running = appState.tasks.filter((task) => task.status === "running");
    if (running.length) {
      return {
        tone: "running",
        text: `${leadTask(running)?.title ?? "一件事"}${running.length > 1 ? ` +${running.length - 1}` : ""}`,
      };
    }
    const waiting = appState.tasks.filter((task) => task.status === "waiting");
    if (waiting.length) return { tone: "waiting", text: `${waiting.length} 件等收尾` };
    return { tone: "open", text: "它在" };
  });

  function readStoredNumber(key: string): number {
    try {
      const value = Number(localStorage.getItem(key) || "0");
      return Number.isInteger(value) && value > 0 ? value : 0;
    } catch {
      return 0;
    }
  }

  function writeStored(key: string, value: string) {
    try {
      localStorage.setItem(key, value);
    } catch {
      // Private browsing can reject storage; read state remains in memory.
    }
  }

  function showToast(message: string, error = false) {
    if (toastTimer) clearTimeout(toastTimer);
    toastText = error ? friendlyError(message) : message;
    toastError = error;
    toastTimer = setTimeout(
      () => {
        toastText = "";
        toastTimer = null;
      },
      error ? 6500 : 2800,
    );
  }

  function closeToast() {
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = null;
    toastText = "";
  }

  async function refreshState(options: { quiet?: boolean } = {}): Promise<void> {
    if (stateFetch) {
      await stateFetch;
      return refreshState({ quiet: true });
    }
    const quiet = options.quiet ?? false;
    stateFetch = (async () => {
      try {
        appState = await fetchState();
        fatal = "";
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        if (!appState) fatal = friendlyError(message);
        else if (!quiet) showToast(message, true);
      } finally {
        initialLoading = false;
      }
    })();
    try {
      await stateFetch;
    } finally {
      stateFetch = null;
    }
  }

  function restoreIfUnsent(text: string, after: number, taskFocus: number | null) {
    const recorded = appState?.messages.some(
      (message) =>
        message.id > after && message.role === "user" && message.text.trim() === text.trim(),
    );
    if (recorded) return false;
    focus = taskFocus;
    composerSeed = { text, nonce: Date.now() };
    return true;
  }

  async function transmit(
    text: string,
    options: { approvals?: number[] } = {},
  ): Promise<boolean> {
    if (pending || !appState) return false;
    const before = appState.lastEvent;
    const turnFocus = focus;
    focus = null;
    pending = true;
    optimistic = { text, at: Math.floor(Date.now() / 1000) };
    try {
      const result = await sendTurn(text, {
        focus: turnFocus,
        approvals: options.approvals ?? [],
      });
      await refreshState({ quiet: true });
      if (result.ok === false) {
        restoreIfUnsent(text, before, turnFocus);
        showToast(result.error || result.stderr || "这一轮没有走完。", true);
        return false;
      }
      return true;
    } catch (error) {
      await refreshState({ quiet: true });
      restoreIfUnsent(text, before, turnFocus);
      showToast(error instanceof Error ? error.message : String(error), true);
      return false;
    } finally {
      pending = false;
      optimistic = null;
    }
  }

  function setView(next: ViewName) {
    view = next;
    writeStored(VIEW_KEY, next);
  }

  function markRead(id: number) {
    if (!Number.isInteger(id) || id <= lastRead) return;
    lastRead = id;
    writeStored(READ_KEY, String(id));
  }

  function approveProposal(id: number) {
    focus = null;
    dismissedProposalIds = new Set(
      [...dismissedProposalIds].filter((proposalId) => proposalId !== id),
    );
    void transmit("可以，就这么定。", { approvals: [id] });
  }

  function dismissProposal(id: number) {
    dismissedProposalIds = new Set([...dismissedProposalIds, id]);
  }

  function openTask(id: number) {
    selectedTaskId = id;
  }

  async function openRaw(address: string) {
    try {
      raw = await fetchRaw(address);
    } catch (error) {
      showToast(error instanceof Error ? error.message : String(error), true);
    }
  }

  async function openSettings() {
    settingsOpen = true;
    settingsLoading = true;
    settingsLoadError = "";
    settings = null;
    try {
      settings = await fetchSettings();
    } catch (error) {
      settingsLoadError = friendlyError(
        error instanceof Error ? error.message : String(error),
      );
    } finally {
      settingsLoading = false;
    }
  }

  async function persistSettings(body: unknown): Promise<boolean> {
    try {
      settings = await saveSettings(body);
      await refreshState({ quiet: true });
      showToast("设置已保存");
      return true;
    } catch (error) {
      showToast(error instanceof Error ? error.message : String(error), true);
      return false;
    }
  }

  function askAbout(task: Task) {
    selectedTaskId = null;
    focus = task.id;
    composerSeed = {
      text: `关于「${task.title}」，`,
      nonce: Date.now(),
    };
    setView("talk");
  }

  function handleKeydown(event: KeyboardEvent) {
    if ((event.metaKey || event.ctrlKey) && event.key === ",") {
      event.preventDefault();
      void openSettings();
      return;
    }
    if (event.key !== "Escape") return;
    if (raw) {
      raw = null;
      return;
    }
    if (selectedTaskId != null) {
      selectedTaskId = null;
      return;
    }
    if (settingsOpen) settingsOpen = false;
  }

  onMount(() => {
    try {
      const storedView = localStorage.getItem(VIEW_KEY);
      if (storedView === "unfold" || storedView === "talk") view = storedView;
    } catch {
      // The in-memory default remains talk.
    }
    void refreshState();
    const onVisibility = () => {
      if (!document.hidden) void refreshState({ quiet: true });
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      document.removeEventListener("visibilitychange", onVisibility);
      if (toastTimer) clearTimeout(toastTimer);
    };
  });

  $effect(() => {
    const delay = pollDelay;
    const timer = setInterval(() => {
      if (!document.hidden) void refreshState({ quiet: true });
    }, delay);
    return () => clearInterval(timer);
  });
</script>

<svelte:window onkeydown={handleKeydown} />

<div class="app-shell">
  <header class="topbar">
    <div class="brand-side">
      <div class="brand">
        <span class="brand-mark" aria-hidden="true"><i></i></span>
        <span class="wordmark">eja</span>
      </div>
    </div>

    <nav class="view-switch" aria-label="视图">
      <button
        type="button"
        class:active={view === "talk"}
        aria-current={view === "talk" ? "page" : undefined}
        title="交流"
        onclick={() => setView("talk")}
      >
        <span>talk</span>
        {#if unreadCount > 0 && view !== "talk"}
          <i class="unread-dot" aria-label={`${unreadCount} 条新消息`}></i>
        {/if}
      </button>
      <button
        type="button"
        class:active={view === "unfold"}
        aria-current={view === "unfold" ? "page" : undefined}
        title="展开工作"
        onclick={() => setView("unfold")}
      >
        <span>unfold</span>
      </button>
    </nav>

    <div class="top-tools">
      <span class="ambient-status" title={ambient.text} aria-label={ambient.text}>
        <i class="status-dot {ambient.tone}"></i>
      </span>
    </div>
  </header>

  {#if appState?.health.journalError}
    <div class="journal-warning" role="alert">
      <AlertCircle size={13} />
      <span>日志里有一段读不了。已经读到的事实仍然保留。</span>
      <button type="button" onclick={() => void refreshState()}>重读</button>
    </div>
  {/if}

  <main class="stage">
    {#if initialLoading && !appState}
      <div class="loading-stage" aria-live="polite">
        <div class="loading-presence"><span></span></div>
        <p>正在把日志展开</p>
      </div>
    {:else if fatal && !appState}
      <div class="fatal-stage">
        <div class="fatal-mark"><AlertCircle size={18} strokeWidth={1.6} /></div>
        <h1>没连上。</h1>
        <p>{fatal}</p>
        <pre>cargo run -- serve --dir .eja --project .</pre>
        <div class="fatal-actions">
          <button class="primary-button" type="button" onclick={() => void refreshState()}>
            再试一次
          </button>
          <span>确认运行时已在 8787 端口启动</span>
        </div>
      </div>
    {:else if appState}
      <div class="view-stage">
        <div class="view-pane" class:active={view === "talk"} aria-hidden={view !== "talk"}>
          <TalkView
            messages={appState.messages}
            proposals={activeProposals}
            tasks={appState.tasks}
            health={appState.health}
            {pending}
            {optimistic}
            visible={view === "talk"}
            seed={composerSeed}
            onSend={(text) => void transmit(text)}
            onApprove={approveProposal}
            onDismiss={dismissProposal}
            onRaw={(address) => void openRaw(address)}
            onRead={markRead}
          />
        </div>

        <div class="view-pane" class:active={view === "unfold"} aria-hidden={view !== "unfold"}>
          <UnfoldView
            tasks={appState.tasks}
            relations={appState.relations}
            counts={{ running: appState.counts.running, waiting: appState.counts.waiting }}
            now={appState.now}
            visible={view === "unfold"}
            selectedTask={selectedTaskId}
            onSelect={openTask}
          />
        </div>
      </div>
    {/if}
  </main>

  <button
    class="settings-trigger"
    type="button"
    aria-label="打开设置中心"
    title="设置"
    onclick={() => void openSettings()}
  >
    <Settings2 size={16} strokeWidth={1.65} />
  </button>

  {#if toastText}
    <div class="toast" class:error={toastError} role="status" transition:fly={{ y: 8, duration: 180 }}>
      {#if toastError}
        <AlertCircle size={14} />
      {:else}
        <span class="toast-mark"></span>
      {/if}
      <span>{toastText}</span>
      <button type="button" aria-label="关闭提示" onclick={closeToast}>
        <X size={13} />
      </button>
    </div>
  {/if}
</div>

{#if selectedTask}
  <TaskSheet
    task={selectedTask}
    tasks={appState?.tasks ?? []}
    onClose={() => (selectedTaskId = null)}
    onAsk={askAbout}
    onRaw={(address) => void openRaw(address)}
    onSelectRelated={openTask}
  />
{/if}

<SettingsDrawer
  open={settingsOpen}
  {settings}
  loading={settingsLoading}
  loadError={settingsLoadError}
  onClose={() => (settingsOpen = false)}
  onSave={persistSettings}
/>

{#if raw}
  <RawDialog {raw} onClose={() => (raw = null)} />
{/if}

<style>
  .app-shell {
    position: relative;
    display: flex;
    width: 100%;
    height: 100dvh;
    flex-direction: column;
    overflow: hidden;
  }

  .topbar {
    position: relative;
    z-index: 30;
    display: grid;
    min-height: 62px;
    flex: 0 0 auto;
    grid-template-columns: minmax(0, 1fr) auto minmax(0, 1fr);
    align-items: center;
    padding: 0 17px 0 20px;
    border-bottom: 1px solid var(--line);
    background: color-mix(in srgb, var(--paper) 86%, transparent);
    backdrop-filter: blur(20px) saturate(1.1);
    -webkit-backdrop-filter: blur(20px) saturate(1.1);
  }

  .brand-side {
    display: flex;
    min-width: 0;
    align-items: center;
  }

  .brand {
    display: inline-flex;
    height: 38px;
    align-items: center;
    gap: 10px;
    border-radius: 10px;
    color: var(--ink);
  }

  .brand-mark {
    display: grid;
    width: 18px;
    height: 18px;
    place-items: center;
    border: 1px solid rgba(36, 35, 31, 0.14);
    border-radius: 50%;
    background: rgba(255, 254, 250, 0.64);
  }

  .brand-mark i {
    width: 5px;
    height: 5px;
    border-radius: 50%;
    background: var(--accent);
    box-shadow: 0 0 0 3px rgba(189, 96, 66, 0.07);
  }

  .wordmark {
    padding-top: 1px;
    font-family: var(--serif);
    font-size: 21px;
    font-weight: 470;
    letter-spacing: -0.035em;
    line-height: 1;
  }

  .view-switch {
    display: inline-flex;
    height: 36px;
    align-items: center;
    gap: 2px;
    padding: 3px;
    border: 1px solid rgba(36, 35, 31, 0.075);
    border-radius: 999px;
    background: rgba(230, 227, 218, 0.46);
    box-shadow: 0 1px 1px rgba(255, 255, 255, 0.7) inset;
  }

  .view-switch button {
    position: relative;
    display: inline-flex;
    height: 28px;
    align-items: center;
    justify-content: center;
    padding: 0 15px;
    border-radius: 999px;
    color: var(--muted);
    font-size: 10.5px;
    font-weight: 540;
    letter-spacing: 0.025em;
    transition:
      color 180ms ease,
      background 180ms ease,
      box-shadow 180ms ease;
  }

  .view-switch button:hover {
    color: var(--ink-soft);
  }

  .view-switch button.active {
    background: rgba(255, 254, 250, 0.96);
    color: var(--ink);
    box-shadow:
      0 1px 2px rgba(36, 35, 31, 0.08),
      0 5px 14px rgba(36, 35, 31, 0.045);
  }

  .unread-dot {
    position: absolute;
    top: 5px;
    right: 7px;
    width: 4px;
    height: 4px;
    border-radius: 50%;
    background: var(--accent);
  }

  .top-tools {
    display: flex;
    min-width: 0;
    align-items: center;
    justify-content: flex-end;
    gap: 5px;
  }

  .ambient-status {
    display: inline-flex;
    width: 30px;
    height: 30px;
    align-items: center;
    justify-content: center;
    border-radius: 999px;
  }

  .ambient-status .status-dot {
    width: 6px;
    height: 6px;
    flex-basis: 6px;
  }

  .journal-warning {
    position: relative;
    z-index: 28;
    display: flex;
    min-height: 34px;
    flex: 0 0 auto;
    align-items: center;
    gap: 8px;
    padding: 0 20px;
    border-bottom: 1px solid rgba(167, 70, 54, 0.13);
    background: color-mix(in srgb, var(--danger-wash) 72%, var(--paper));
    color: var(--danger);
    font-size: 10.5px;
  }

  .journal-warning span {
    flex: 1;
  }

  .journal-warning button {
    color: var(--danger);
    font-size: 10.5px;
    text-decoration: underline;
    text-underline-offset: 3px;
  }

  .stage {
    position: relative;
    flex: 1;
    min-height: 0;
  }

  .view-stage,
  .view-pane {
    position: absolute;
    inset: 0;
    min-height: 0;
  }

  .view-pane {
    visibility: hidden;
    opacity: 0;
    pointer-events: none;
    transform: translateY(5px);
    transition:
      opacity 200ms ease,
      transform 260ms cubic-bezier(0.2, 0.7, 0.2, 1),
      visibility 0s linear 260ms;
  }

  .view-pane.active {
    visibility: visible;
    opacity: 1;
    pointer-events: auto;
    transform: translateY(0);
    transition:
      opacity 220ms ease,
      transform 300ms cubic-bezier(0.2, 0.7, 0.2, 1),
      visibility 0s;
  }

  .loading-stage,
  .fatal-stage {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    flex-direction: column;
    padding: 30px;
    text-align: center;
  }

  .loading-presence {
    display: grid;
    width: 46px;
    height: 46px;
    place-items: center;
    border: 1px solid var(--line);
    border-radius: 50%;
    background: rgba(255, 254, 250, 0.62);
    box-shadow: var(--shadow-sm);
  }

  .loading-presence span {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--accent);
    animation: dot-breathe 2.2s ease-out infinite;
  }

  .loading-stage p {
    margin: 15px 0 0;
    color: var(--muted);
    font-size: 11px;
    letter-spacing: 0.035em;
  }

  .fatal-stage {
    max-width: 560px;
    margin: auto;
  }

  .fatal-mark {
    display: grid;
    width: 42px;
    height: 42px;
    place-items: center;
    border: 1px solid rgba(167, 70, 54, 0.13);
    border-radius: 50%;
    background: var(--danger-wash);
    color: var(--danger);
  }

  .fatal-stage h1 {
    margin: 17px 0 0;
    font-family: var(--serif);
    font-size: 34px;
    font-weight: 450;
    letter-spacing: -0.035em;
  }

  .fatal-stage p {
    max-width: 44ch;
    margin: 7px 0 0;
    color: var(--muted);
    font-size: 12px;
  }

  .fatal-stage pre {
    margin: 22px 0 0;
    padding: 13px 18px;
    border: 1px solid var(--line);
    border-radius: 12px;
    background: rgba(255, 254, 250, 0.72);
    box-shadow: var(--shadow-xs);
    color: var(--ink-soft);
    font-family: var(--mono);
    font-size: 10.5px;
    text-align: left;
  }

  .fatal-actions {
    display: flex;
    align-items: center;
    gap: 13px;
    margin-top: 18px;
  }

  .fatal-actions span {
    color: var(--faint);
    font-size: 9.5px;
  }

  .settings-trigger {
    position: fixed;
    bottom: max(15px, env(safe-area-inset-bottom));
    left: 16px;
    z-index: 35;
    display: grid;
    width: 34px;
    height: 34px;
    place-items: center;
    border: 1px solid rgba(36, 35, 31, 0.1);
    border-radius: 999px;
    background: color-mix(in srgb, var(--surface) 80%, transparent);
    color: var(--muted);
    font-size: 10px;
    box-shadow: 0 4px 15px rgba(36, 35, 31, 0.045);
    backdrop-filter: blur(12px);
    -webkit-backdrop-filter: blur(12px);
    transition:
      color 150ms ease,
      border-color 150ms ease,
      background 150ms ease,
      transform 150ms ease;
  }

  .settings-trigger:hover {
    border-color: rgba(36, 35, 31, 0.16);
    background: var(--surface);
    color: var(--ink);
    transform: translateY(-1px);
  }

  .toast {
    position: fixed;
    bottom: max(21px, env(safe-area-inset-bottom));
    left: 50%;
    z-index: 100;
    display: flex;
    max-width: min(520px, calc(100vw - 120px));
    min-height: 38px;
    align-items: center;
    gap: 8px;
    padding: 7px 9px 7px 13px;
    border: 1px solid rgba(255, 255, 255, 0.08);
    border-radius: 999px;
    background: rgba(36, 35, 31, 0.94);
    color: rgba(255, 255, 255, 0.91);
    font-size: 10.5px;
    box-shadow: 0 14px 44px rgba(24, 23, 20, 0.22);
    transform: translateX(-50%);
    backdrop-filter: blur(12px);
  }

  .toast.error {
    background: #40312c;
  }

  .toast > span:nth-child(2) {
    line-height: 1.55;
  }

  .toast-mark {
    width: 6px;
    height: 6px;
    flex: 0 0 6px;
    border-radius: 50%;
    background: #d98a6d;
  }

  .toast button {
    display: grid;
    width: 26px;
    height: 26px;
    flex: 0 0 26px;
    place-items: center;
    border-radius: 50%;
    color: rgba(255, 255, 255, 0.56);
  }

  .toast button:hover {
    background: rgba(255, 255, 255, 0.08);
    color: #fff;
  }

  @media (max-width: 760px) {
    .topbar {
      min-height: 56px;
      grid-template-columns: 50px minmax(0, 1fr) 44px;
      padding: 0 10px;
    }

    .brand {
      gap: 4px;
    }

    .brand-mark {
      width: 16px;
      height: 16px;
    }

    .wordmark {
      display: inline;
      font-size: 18px;
    }

    .view-switch {
      justify-self: center;
    }

    .view-switch button {
      min-width: 70px;
      padding: 0 13px;
    }

    .ambient-status {
      display: none;
    }

    .settings-trigger {
      bottom: max(12px, env(safe-area-inset-bottom));
      left: 12px;
    }

    .journal-warning {
      padding: 7px 12px;
      line-height: 1.45;
    }

    .toast {
      bottom: max(14px, env(safe-area-inset-bottom));
      max-width: calc(100vw - 24px);
      border-radius: 13px;
    }
  }
</style>
