<script lang="ts">
  import { onMount } from "svelte";
  import { AlertCircle, Settings2, X } from "lucide-svelte";
  import { fly } from "svelte/transition";
  import RawDialog from "./lib/RawDialog.svelte";
  import SettingsDrawer from "./lib/SettingsDrawer.svelte";
  import TalkView from "./lib/TalkView.svelte";
  import TaskSheet from "./lib/TaskSheet.svelte";
  import {
    fetchRaw,
    fetchSettings,
    fetchState,
    friendlyError,
    saveSettings,
    sendTurn,
  } from "./lib/api";
  import {
    parkedMatches,
    readParkedEncounter,
    storeEncounter,
  } from "./lib/encounter";
  import type {
    AppState,
    ComposerSeed,
    Encounter,
    OpenLoop,
    OptimisticMessage,
    RawRecord,
    SettingsView,
  } from "./lib/types";

  let appState = $state<AppState | null>(null);
  let initialLoading = $state(true);
  let fatal = $state("");
  let pending = $state(false);
  let optimistic = $state<OptimisticMessage | null>(null);
  let composerSeed = $state<ComposerSeed | null>(null);
  let parkedEncounter = $state<Encounter | null>(null);
  let encounter = $state<Encounter | null>(null);
  let restoredParked = $state(false);
  let selectedTaskId = $state<number | null>(null);
  let raw = $state<RawRecord | null>(null);
  let settingsOpen = $state(false);
  let settings = $state<SettingsView | null>(null);
  let settingsLoading = $state(false);
  let settingsLoadError = $state("");
  let toastText = $state("");
  let toastError = $state(false);
  let toastTimer: ReturnType<typeof setTimeout> | null = null;
  let stateFetch: Promise<void> | null = null;

  const busy = $derived(Boolean(pending || appState?.health.turnInFlight));
  const presenceMessage = $derived(encounter ? null : (appState?.presence ?? null));
  const loops = $derived(appState?.loops ?? []);
  const actions = $derived(encounter ? (appState?.loops ?? []) : []);
  const encounterMessages = $derived(appState?.spoken ?? []);
  const selectedTask = $derived(
    selectedTaskId == null
      ? null
      : (appState?.tasks.find((task) => task.id === selectedTaskId) ?? null),
  );
  const pollDelay = $derived(pending || appState?.health.turnInFlight ? 2000 : 5000);

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
        appState = await fetchState({ busy, encounter });
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

  function restoreIfUnsent(text: string, after: number) {
    const recorded = appState?.events.some(
      (event) =>
        event.id > after && event.kind === "user" && event.text.trim() === text.trim(),
    );
    if (recorded) return false;
    composerSeed = { text, nonce: Date.now() };
    return true;
  }

  function resumeParked() {
    if (!parkedEncounter) return false;
    restoredParked = false;
    encounter = parkedEncounter;
    parkedEncounter = null;
    storeEncounter(encounter);
    return true;
  }

  function beginEncounter(source: Partial<Encounter> = {}) {
    if (!appState) return false;
    restoredParked = false;
    encounter = {
      anchor: source.anchor ?? appState.lastEvent,
      taskId: source.taskId ?? null,
      proposalId: source.proposalId ?? null,
    };
    parkedEncounter = null;
    storeEncounter(encounter);
    return true;
  }

  function openLoop(loop: OpenLoop) {
    if (!appState) return;
    if (parkedEncounter && parkedMatches(loop, parkedEncounter)) {
      resumeParked();
      return;
    }
    if (loop.kind === "decision") {
      beginEncounter({ proposalId: loop.proposal.id });
      return;
    }
    beginEncounter({ taskId: loop.task.id });
  }

  function leaveEncounter() {
    if (!encounter || pending || appState?.health.turnInFlight) return;
    encounter = null;
    parkedEncounter = null;
    storeEncounter(null);
    void gesture("leave");
  }

  async function transmit(
    text: string,
    options: {
      approvals?: number[];
      denials?: number[];
      focus?: number | null;
      restore?: boolean;
      message?: boolean;
      gesture?: "return" | "leave" | "alone";
    } = {},
  ): Promise<boolean> {
    if (pending || !appState) return false;
    if (!options.gesture) {
      if (!encounter && !beginEncounter()) return false;
      if (!encounter) return false;
    }
    const before = appState.lastEvent;
    const turnFocus =
      options.focus === undefined ? (encounter?.taskId ?? null) : options.focus;
    pending = true;
    optimistic =
      options.message === false || options.gesture
        ? null
        : { text, at: Math.floor(Date.now() / 1000) };
    try {
      const result = await sendTurn(text, {
        focus: turnFocus,
        approvals: options.approvals ?? [],
        denials: options.denials ?? [],
        gesture: options.gesture,
      });
      await refreshState({ quiet: true });
      if (result.ok === false) {
        if (options.restore !== false) restoreIfUnsent(text, before);
        showToast(result.error || result.stderr || "这一轮没有走完。", true);
        return false;
      }
      return true;
    } catch (error) {
      await refreshState({ quiet: true });
      if (options.restore !== false) restoreIfUnsent(text, before);
      showToast(error instanceof Error ? error.message : String(error), true);
      return false;
    } finally {
      pending = false;
      optimistic = null;
    }
  }

  async function gesture(kind: "return" | "leave" | "alone") {
    if (!appState || pending || appState.health.turnInFlight) return;
    await transmit("", { gesture: kind, restore: false, message: false, focus: null });
  }

  function approveProposal(id: number) {
    void transmit("", {
      approvals: [id],
      restore: false,
      message: false,
    });
  }

  function denyProposal(id: number) {
    void transmit("", {
      denials: [id],
      restore: false,
      message: false,
    });
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
    parkedEncounter = readParkedEncounter();
    restoredParked = parkedEncounter != null;
    void refreshState().then(() => {
      if (!encounter && !busy) void gesture("return");
    });
    const onVisibility = () => {
      if (document.hidden) return;
      void refreshState({ quiet: true }).then(() => {
        if (!encounter && !busy) void gesture("return");
      });
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      document.removeEventListener("visibilitychange", onVisibility);
      if (toastTimer) clearTimeout(toastTimer);
    };
  });

  $effect(() => {
    if (!appState || !parkedEncounter || encounter || !restoredParked) return;
    const source = appState.loops.some((loop) => parkedMatches(loop, parkedEncounter!));
    if (!source) {
      parkedEncounter = null;
      storeEncounter(null);
      restoredParked = false;
    }
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
          <span>确认运行时已启动</span>
        </div>
      </div>
    {:else if appState}
      <TalkView
        mode={encounter ? "encounter" : "ambient"}
        {loops}
        {actions}
        messages={encounterMessages}
        presence={presenceMessage}
        tasks={appState.tasks}
        health={appState.health}
        {pending}
        {optimistic}
        seed={composerSeed}
        onOpenLoop={openLoop}
        onSend={(text) => void transmit(text)}
        onApprove={approveProposal}
        onDeny={denyProposal}
        onInspectTask={(task) => openTask(task.id)}
        onBye={leaveEncounter}
      />
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
