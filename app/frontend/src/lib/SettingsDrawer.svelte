<script lang="ts">
  import {
    AlertCircle,
    Cable,
    Check,
    ChevronRight,
    Copy,
    Cpu,
    Eye,
    EyeOff,
    FolderGit2,
    Gauge,
    Globe2,
    KeyRound,
    LoaderCircle,
    RotateCcw,
    Server,
    ShieldCheck,
    SlidersHorizontal,
    Sparkles,
    X,
  } from "lucide-svelte";
  import { fade, fly } from "svelte/transition";
  import NumberField from "./NumberField.svelte";
  import type { SettingsView } from "./types";

  type Section = "model" | "connectors" | "runtime" | "workspace";

  interface Props {
    open: boolean;
    settings: SettingsView | null;
    loading: boolean;
    loadError: string;
    onClose: () => void;
    onSave: (body: unknown) => Promise<boolean>;
  }

  let { open, settings, loading, loadError, onClose, onSave }: Props = $props();

  let section = $state<Section>("model");
  let baseUrl = $state("");
  let apiKey = $state("");
  let model = $state("");
  let ajaModel = $state("");
  let mcpConfig = $state("");
  let slots = $state(1);
  let turnTimeoutMs = $state(0);
  let ajaSteps = $state(0);
  let ajaTimeoutMs = $state(0);
  let binary = $state("");
  let showKey = $state(false);
  let clearKey = $state(false);
  let saving = $state(false);
  let saved = $state(false);
  let dirty = $state(false);
  let copiedPath = $state("");

  const sections = [
    { id: "model" as const, label: "模型", caption: "思考与执行", Icon: Cpu },
    { id: "connectors" as const, label: "连接器", caption: "外部能力", Icon: Cable },
    { id: "runtime" as const, label: "运行", caption: "速度与界限", Icon: Gauge },
    { id: "workspace" as const, label: "工作区", caption: "路径与程序", Icon: FolderGit2 },
  ];

  const headings: Record<Section, { title: string; description: string }> = {
    model: {
      title: "模型",
      description: "eja 用主模型理解你、组织工作；AJA 用它执行具体节点。",
    },
    connectors: {
      title: "连接器",
      description: "给工作接入外部系统。连接器提供能力，不会绕过你的批准。",
    },
    runtime: {
      title: "运行",
      description: "决定它同时能做多少事，以及一次工作最多可以走多远。",
    },
    workspace: {
      title: "工作区",
      description: "日志、项目和 eja 可执行文件的位置。",
    },
  };

  const heading = $derived(headings[section]);

  function hydrate(value: SettingsView | null) {
    if (!value) return;
    baseUrl = value.model.baseUrl;
    apiKey = "";
    clearKey = false;
    model = value.model.model;
    ajaModel = value.model.ajaModel;
    mcpConfig = value.connectors.mcpConfig;
    slots = value.limits.slots;
    turnTimeoutMs = value.limits.turnTimeoutMs;
    ajaSteps = value.limits.ajaSteps;
    ajaTimeoutMs = value.limits.ajaTimeoutMs;
    binary = value.binary;
    dirty = false;
  }

  function markDirty() {
    dirty = true;
    saved = false;
  }

  async function copyPath(value: string, key: string) {
    try {
      await navigator.clipboard.writeText(value);
      copiedPath = key;
      setTimeout(() => {
        if (copiedPath === key) copiedPath = "";
      }, 1500);
    } catch {
      copiedPath = "";
    }
  }

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (!dirty || saving) return;
    saving = true;
    const ok = await onSave({
      model: {
        baseUrl: baseUrl.trim(),
        model: model.trim(),
        ajaModel: ajaModel.trim(),
        ...(apiKey.trim() ? { apiKey } : {}),
        ...(clearKey ? { clearApiKey: true } : {}),
      },
      connectors: { mcpConfig: mcpConfig.trim() },
      limits: {
        slots,
        turnTimeoutMs,
        ajaSteps,
        ajaTimeoutMs,
      },
      binary: binary.trim(),
    });
    saving = false;
    if (ok) {
      dirty = false;
      saved = true;
      apiKey = "";
      clearKey = false;
      showKey = false;
      setTimeout(() => (saved = false), 2200);
    }
  }

  $effect(() => {
    hydrate(settings);
  });
</script>

{#if open}
  <div class="settings-layer">
    <button class="settings-backdrop" type="button" aria-label="关闭设置" onclick={onClose} transition:fade={{ duration: 150 }}></button>
    <aside class="settings" aria-label="设置" transition:fly={{ x: -34, duration: 250, opacity: 1 }}>
      <header class="settings-head">
        <div class="title">
          <span class="eyebrow">EJA · HARNESS</span>
          <h2>设置</h2>
        </div>
        <button class="icon-button" type="button" aria-label="关闭设置" onclick={onClose}>
          <X size={18} />
        </button>
      </header>

      <div class="settings-body">
        <nav aria-label="设置分类">
          <div class="nav-label">偏好</div>
          {#each sections as item}
            <button
              class:active={section === item.id}
              type="button"
              onclick={() => (section = item.id)}
            >
              <span class="nav-icon"><item.Icon size={15} strokeWidth={1.7} /></span>
              <span class="nav-copy">
                <strong>{item.label}</strong>
                <small>{item.caption}</small>
              </span>
              <span class="nav-chevron" aria-hidden="true">
                <ChevronRight size={13} />
              </span>
            </button>
          {/each}

          <div class="status-card">
            <span class="status-title">连接状态</span>
            {#if settings}
              <span class:ok={settings.health.model}>
                <i class="status-dot" class:ok={settings.health.model} class:bad={!settings.health.model}></i>
                模型
              </span>
              <span class:ok={settings.health.binary}>
                <i class="status-dot" class:ok={settings.health.binary} class:bad={!settings.health.binary}></i>
                eja
              </span>
              <span class:ok={settings.health.mcp}>
                <i class="status-dot" class:ok={settings.health.mcp} class:bad={!settings.health.mcp}></i>
                连接器
              </span>
            {/if}
          </div>
        </nav>

        <form id="settings-form" onsubmit={submit}>
          <div class="content-head">
            <div class="content-icon"><SlidersHorizontal size={17} strokeWidth={1.6} /></div>
            <div>
              <h3>{heading.title}</h3>
              <p>{heading.description}</p>
            </div>
          </div>

          {#if loading && !settings}
            <div class="settings-loading">
              <LoaderCircle size={18} class="spin" />
              <span>正在读取设置</span>
            </div>
          {:else if loadError && !settings}
            <div class="inline-error"><AlertCircle size={16} />{loadError}</div>
          {:else}
            {#if section === "model"}
              <div class="field-card">
                <label class="field">
                  <span class="field-icon"><Globe2 size={15} /></span>
                  <span class="field-copy">
                    <strong>接口地址</strong>
                    <small>兼容 OpenAI 的 Base URL</small>
                  </span>
                  <input
                    bind:value={baseUrl}
                    oninput={markDirty}
                    type="url"
                    placeholder={settings?.model.effectiveBaseUrl || "https://api.openai.com/v1"}
                    autocomplete="off"
                  />
                </label>

                <label class="field">
                  <span class="field-icon"><KeyRound size={15} /></span>
                  <span class="field-copy">
                    <strong>API 密钥</strong>
                    <small>{settings?.model.hasApiKey ? `已保存 ${settings.model.apiKeyHint}` : "只保存在本机"}</small>
                  </span>
                  <span class="key-input">
                    <input
                      bind:value={apiKey}
                      oninput={() => {
                        clearKey = false;
                        markDirty();
                      }}
                      type={showKey ? "text" : "password"}
                      placeholder={settings?.model.hasApiKey ? "留空继续使用" : "sk-…"}
                      autocomplete="new-password"
                    />
                    <button type="button" aria-label={showKey ? "隐藏密钥" : "显示密钥"} onclick={() => (showKey = !showKey)}>
                      {#if showKey}<EyeOff size={14} />{:else}<Eye size={14} />{/if}
                    </button>
                  </span>
                </label>
                {#if settings?.model.hasApiKey}
                  <button
                    class="clear-key"
                    type="button"
                    onclick={() => {
                      clearKey = !clearKey;
                      apiKey = "";
                      markDirty();
                    }}
                  >
                    {#if clearKey}<RotateCcw size={12} />保存时保留现有密钥{:else}<X size={12} />保存时清除现有密钥{/if}
                  </button>
                {/if}
              </div>

              <div class="field-card">
                <label class="field">
                  <span class="field-icon"><Sparkles size={15} /></span>
                  <span class="field-copy">
                    <strong>主模型</strong>
                    <small>理解、规划、判断</small>
                  </span>
                  <input
                    bind:value={model}
                    oninput={markDirty}
                    type="text"
                    placeholder={settings?.model.effectiveModel || "gpt-5"}
                    autocomplete="off"
                  />
                </label>
                <label class="field">
                  <span class="field-icon"><Cpu size={15} /></span>
                  <span class="field-copy">
                    <strong>AJA 模型</strong>
                    <small>留空则继承主模型</small>
                  </span>
                  <input
                    bind:value={ajaModel}
                    oninput={markDirty}
                    type="text"
                    placeholder="跟随主模型"
                    autocomplete="off"
                  />
                </label>
              </div>

              <div class="hint-card">
                <ShieldCheck size={16} />
                <p>密钥通过环境变量交给 eja 进程，不进入对话记录。设置文件只写入本机。</p>
              </div>
            {:else if section === "connectors"}
              <div class="field-card">
                <label class="field field-stack">
                  <span class="field-top">
                    <span class="field-icon"><Server size={15} /></span>
                    <span class="field-copy">
                      <strong>MCP 配置文件</strong>
                      <small>给 AJA 增加读取或操作能力</small>
                    </span>
                  </span>
                  <input
                    bind:value={mcpConfig}
                    oninput={markDirty}
                    type="text"
                    placeholder=".eja/mcp.json"
                    autocomplete="off"
                  />
                </label>
              </div>

              <div class="connection-card" class:connected={settings?.health.mcp}>
                <span class="connection-icon">
                  {#if settings?.health.mcp}<Check size={17} />{:else}<Cable size={17} />{/if}
                </span>
                <div>
                  <strong>{settings?.health.mcp ? "连接器已接入" : "还没有连接器"}</strong>
                  <p>
                    {settings?.connectors.effective
                      ? settings.connectors.effective
                      : "没有连接器时，AJA 仍然可以读取记录和完成本地工作。"}
                  </p>
                </div>
              </div>

              <div class="hint-card">
                <ShieldCheck size={16} />
                <p>连接器只扩展能力，不改变权限模型。需要你确认的事仍会回到 talk。</p>
              </div>
            {:else if section === "runtime"}
              <div class="runtime-list">
                <NumberField
                  bind:value={slots}
                  label="并行槽位"
                  description="同时可以运行多少个 AJA"
                  min={1}
                  max={8}
                  step={1}
                  suffix="槽"
                />
                <NumberField
                  bind:value={turnTimeoutMs}
                  label="回合超时"
                  description="0 使用 eja 的默认界限"
                  min={0}
                  max={7_200_000}
                  step={60_000}
                  suffix="ms"
                />
                <NumberField
                  bind:value={ajaSteps}
                  label="AJA 步数"
                  description="一个节点最多走多少步"
                  min={0}
                  max={200}
                  step={1}
                  suffix="步"
                />
                <NumberField
                  bind:value={ajaTimeoutMs}
                  label="AJA 超时"
                  description="单个节点的时间上限"
                  min={0}
                  max={14_400_000}
                  step={60_000}
                  suffix="ms"
                />
              </div>

              <div class="hint-card conceptual">
                <span class="concept-mark"></span>
                <p>并行发生在工作里，不发生在对话里。talk 始终只把眼前这一件讲清楚。</p>
              </div>
            {:else}
              <div class="path-list">
                <div class="path-row">
                  <span class="field-icon"><FolderGit2 size={15} /></span>
                  <span><strong>状态目录</strong><code>{settings?.stateDir}</code></span>
                  <button type="button" aria-label="复制状态目录" onclick={() => copyPath(settings?.stateDir || "", "state")}>
                    {#if copiedPath === "state"}<Check size={13} />{:else}<Copy size={13} />{/if}
                  </button>
                </div>
                <div class="path-row">
                  <span class="field-icon"><FolderGit2 size={15} /></span>
                  <span><strong>项目目录</strong><code>{settings?.projectDir}</code></span>
                  <button type="button" aria-label="复制项目目录" onclick={() => copyPath(settings?.projectDir || "", "project")}>
                    {#if copiedPath === "project"}<Check size={13} />{:else}<Copy size={13} />{/if}
                  </button>
                </div>
              </div>

              <div class="field-card">
                <label class="field field-stack">
                  <span class="field-top">
                    <span class="field-icon"><Cpu size={15} /></span>
                    <span class="field-copy">
                      <strong>eja 可执行文件</strong>
                      <small>留空会自动寻找 target/debug/eja</small>
                    </span>
                  </span>
                  <input
                    bind:value={binary}
                    oninput={markDirty}
                    type="text"
                    placeholder={settings?.effectiveBinary || "target/debug/eja"}
                    autocomplete="off"
                  />
                </label>
              </div>

              <div class="hint-card">
                <AlertCircle size={16} />
                <p>状态目录和项目目录在服务启动时确定。改动这两个位置需要重启 host。</p>
              </div>
            {/if}
          {/if}
        </form>
      </div>

      <footer class="settings-foot">
        <span class="save-state">
          {#if saving}
            <LoaderCircle size={12} class="spin" />保存中
          {:else if saved}
            <Check size={12} />已保存
          {:else if dirty}
            有未保存的改动
          {:else}
            {settings?.health.model ? "已连接" : "等待配置"}
          {/if}
        </span>
        <button class="primary-button" type="submit" form="settings-form" disabled={!dirty || saving || loading}>
          保存设置
        </button>
      </footer>
    </aside>
  </div>
{/if}

<style>
  .settings-layer {
    position: fixed;
    inset: 0;
    z-index: 70;
  }

  .settings-backdrop {
    position: absolute;
    inset: 0;
    background: rgba(36, 35, 31, 0.2);
    backdrop-filter: blur(3px);
  }

  .settings {
    position: relative;
    display: flex;
    width: min(760px, 94vw);
    height: 100%;
    flex-direction: column;
    overflow: hidden;
    border-right: 1px solid rgba(36, 35, 31, 0.11);
    background: #fbfaf6;
    box-shadow: 28px 0 90px rgba(36, 35, 31, 0.13);
  }

  .settings-head {
    display: flex;
    height: 80px;
    flex: 0 0 80px;
    align-items: center;
    justify-content: space-between;
    padding: 0 23px 0 28px;
    border-bottom: 1px solid var(--line);
  }

  .title {
    display: flex;
    flex-direction: column;
    gap: 1px;
  }

  .eyebrow {
    color: var(--faint);
    font-size: 8px;
    letter-spacing: 0.15em;
  }

  h2 {
    margin: 0;
    font-family: var(--serif);
    font-size: 25px;
    font-weight: 480;
    letter-spacing: -0.025em;
  }

  .settings-body {
    display: grid;
    flex: 1;
    min-height: 0;
    grid-template-columns: 190px minmax(0, 1fr);
  }

  nav {
    display: flex;
    min-height: 0;
    flex-direction: column;
    gap: 3px;
    padding: 22px 13px 18px;
    border-right: 1px solid var(--line);
    background: color-mix(in srgb, var(--paper) 72%, white);
  }

  .nav-label {
    padding: 0 10px 8px;
    color: var(--faint);
    font-size: 8.5px;
    letter-spacing: 0.12em;
    text-transform: uppercase;
  }

  nav > button {
    display: grid;
    grid-template-columns: 28px minmax(0, 1fr) 14px;
    align-items: center;
    gap: 8px;
    min-height: 48px;
    padding: 5px 9px;
    border-radius: 11px;
    color: var(--muted);
    text-align: left;
    transition:
      color 150ms ease,
      background 150ms ease;
  }

  nav > button:hover {
    background: rgba(255, 254, 250, 0.54);
    color: var(--ink-soft);
  }

  nav > button.active {
    background: var(--surface);
    color: var(--ink);
    box-shadow:
      0 2px 8px rgba(36, 35, 31, 0.04),
      0 0 0 1px var(--line) inset;
  }

  .nav-icon {
    display: grid;
    width: 28px;
    height: 28px;
    place-items: center;
    border-radius: 8px;
    color: var(--muted);
  }

  nav > button.active .nav-icon {
    background: var(--accent-wash);
    color: var(--accent-deep);
  }

  .nav-copy {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 1px;
  }

  .nav-copy strong {
    font-size: 11.5px;
    font-weight: 570;
  }

  .nav-copy small {
    color: var(--faint);
    font-size: 8.5px;
  }

  .nav-chevron {
    display: grid;
    width: 14px;
    place-items: center;
    color: var(--faint);
    opacity: 0;
    transition: opacity 150ms ease;
  }

  nav > button.active .nav-chevron {
    opacity: 1;
  }

  .status-card {
    display: flex;
    flex-direction: column;
    gap: 7px;
    margin-top: auto;
    padding: 13px 11px;
    border: 1px solid var(--line);
    border-radius: 12px;
    background: rgba(255, 254, 250, 0.48);
  }

  .status-title {
    margin-bottom: 1px;
    color: var(--faint) !important;
    font-family: var(--sans) !important;
    font-size: 8.5px !important;
    letter-spacing: 0.1em;
    text-transform: uppercase;
  }

  .status-card > span {
    display: flex;
    align-items: center;
    gap: 6px;
    color: var(--muted);
    font-family: var(--sans);
    font-size: 9.5px;
  }

  .status-card .status-dot {
    width: 5px;
    height: 5px;
    flex-basis: 5px;
  }

  .status-card .status-dot.ok {
    background: var(--ink-soft);
  }

  .status-card .status-dot.bad {
    background: var(--danger);
  }

  form {
    min-height: 0;
    overflow-y: auto;
    padding: 28px 32px 36px;
  }

  .content-head {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    margin-bottom: 24px;
  }

  .content-icon {
    display: grid;
    width: 34px;
    height: 34px;
    flex: 0 0 34px;
    place-items: center;
    border: 1px solid var(--line);
    border-radius: 10px;
    background: var(--surface);
    color: var(--accent-deep);
    box-shadow: var(--shadow-xs);
  }

  h3 {
    margin: 0;
    font-family: var(--serif);
    font-size: 21px;
    font-weight: 490;
    letter-spacing: -0.02em;
  }

  .content-head p {
    max-width: 48ch;
    margin: 2px 0 0;
    color: var(--muted);
    font-size: 10.5px;
    line-height: 1.45;
  }

  .field-card {
    overflow: hidden;
    margin-bottom: 13px;
    border: 1px solid var(--line);
    border-radius: 14px;
    background: rgba(255, 254, 250, 0.66);
    box-shadow: 0 1px 1px rgba(255, 255, 255, 0.6) inset;
  }

  .field {
    display: grid;
    min-height: 63px;
    grid-template-columns: 30px minmax(0, 1fr) minmax(170px, 0.9fr);
    align-items: center;
    gap: 10px;
    padding: 10px 13px;
  }

  .field + .field {
    border-top: 1px solid var(--line);
  }

  .field-icon {
    display: grid;
    width: 30px;
    height: 30px;
    place-items: center;
    border-radius: 8px;
    background: var(--surface-2);
    color: var(--muted);
  }

  .field-copy {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 1px;
  }

  .field-copy strong {
    color: var(--ink-soft);
    font-size: 11.5px;
    font-weight: 580;
  }

  .field-copy small {
    overflow: hidden;
    color: var(--faint);
    font-size: 9px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .field > input,
  .field-stack > input {
    width: 100%;
    height: 34px;
    padding: 0 10px;
    border: 1px solid var(--line);
    border-radius: 8px;
    outline: 0;
    background: var(--surface);
    color: var(--ink);
    font-size: 10.5px;
    transition:
      border-color 150ms ease,
      box-shadow 150ms ease;
  }

  .field > input:focus,
  .field-stack > input:focus {
    border-color: rgba(189, 96, 66, 0.34);
    box-shadow: 0 0 0 3px rgba(189, 96, 66, 0.055);
  }

  .field-stack {
    display: flex;
    align-items: stretch;
    flex-direction: column;
    gap: 9px;
    padding: 13px;
  }

  .field-top {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .field-stack > input {
    height: 36px;
  }

  .key-input {
    display: flex;
    height: 34px;
    align-items: center;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--surface);
  }

  .key-input:focus-within {
    border-color: rgba(189, 96, 66, 0.34);
    box-shadow: 0 0 0 3px rgba(189, 96, 66, 0.055);
  }

  .key-input input {
    min-width: 0;
    flex: 1;
    border: 0;
    outline: 0;
    background: transparent;
    padding: 0 9px;
    font-size: 10.5px;
  }

  .key-input button {
    display: grid;
    width: 32px;
    height: 32px;
    place-items: center;
    color: var(--faint);
  }

  .key-input button:hover {
    color: var(--ink);
  }

  .clear-key {
    display: flex;
    align-items: center;
    gap: 4px;
    margin: -2px 13px 11px auto;
    color: var(--faint);
    font-size: 8.5px;
  }

  .clear-key:hover {
    color: var(--danger);
  }

  .hint-card,
  .connection-card {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    margin-top: 15px;
    padding: 12px 13px;
    border: 1px solid var(--line);
    border-radius: 12px;
    background: rgba(243, 241, 235, 0.58);
    color: var(--muted);
  }

  .hint-card :global(svg),
  .connection-icon {
    flex: 0 0 auto;
    margin-top: 1px;
    color: var(--accent-deep);
  }

  .hint-card p,
  .connection-card p {
    margin: 0;
    font-size: 9.5px;
    line-height: 1.55;
  }

  .connection-card {
    align-items: center;
    margin-top: 0;
    padding: 15px;
    background: rgba(247, 232, 228, 0.38);
  }

  .connection-card.connected {
    background: rgba(255, 254, 250, 0.66);
  }

  .connection-icon {
    display: grid;
    width: 34px;
    height: 34px;
    place-items: center;
    border-radius: 50%;
    background: var(--surface);
    box-shadow: var(--shadow-xs);
  }

  .connection-card strong {
    display: block;
    margin-bottom: 2px;
    color: var(--ink-soft);
    font-size: 11px;
    font-weight: 600;
  }

  .runtime-list {
    display: grid;
    grid-template-columns: 1fr;
    gap: 9px;
  }

  .hint-card.conceptual {
    align-items: center;
    margin-top: 17px;
  }

  .concept-mark {
    width: 8px;
    height: 8px;
    flex: 0 0 8px;
    border-radius: 50%;
    background: var(--accent);
    box-shadow: 0 0 0 4px rgba(189, 96, 66, 0.075);
  }

  .path-list {
    overflow: hidden;
    margin-bottom: 13px;
    border: 1px solid var(--line);
    border-radius: 14px;
    background: rgba(255, 254, 250, 0.66);
  }

  .path-row {
    display: grid;
    min-height: 58px;
    grid-template-columns: 30px minmax(0, 1fr) 28px;
    align-items: center;
    gap: 10px;
    padding: 9px 10px 9px 13px;
  }

  .path-row + .path-row {
    border-top: 1px solid var(--line);
  }

  .path-row > span:nth-child(2) {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 2px;
  }

  .path-row strong {
    color: var(--ink-soft);
    font-size: 10.5px;
    font-weight: 570;
  }

  .path-row code {
    overflow: hidden;
    color: var(--muted);
    font-family: var(--mono);
    font-size: 8.5px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .path-row button {
    display: grid;
    width: 28px;
    height: 28px;
    place-items: center;
    border-radius: 7px;
    color: var(--faint);
  }

  .path-row button:hover {
    background: var(--surface-2);
    color: var(--ink);
  }

  .settings-loading {
    display: flex;
    min-height: 160px;
    align-items: center;
    justify-content: center;
    gap: 8px;
    color: var(--muted);
    font-size: 11px;
  }

  .inline-error {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 12px;
    border: 1px solid rgba(167, 70, 54, 0.16);
    border-radius: 10px;
    background: var(--danger-wash);
    color: var(--danger);
    font-size: 10.5px;
  }

  .settings-foot {
    display: flex;
    height: 66px;
    flex: 0 0 66px;
    align-items: center;
    justify-content: space-between;
    padding: 0 23px 0 28px;
    border-top: 1px solid var(--line);
    background: rgba(251, 250, 246, 0.94);
    backdrop-filter: blur(12px);
  }

  .save-state {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    color: var(--faint);
    font-size: 9.5px;
  }

  :global(.spin) {
    animation: spin 0.9s linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  @media (max-width: 720px) {
    .settings {
      width: 100%;
    }

    .settings-head {
      height: 70px;
      flex-basis: 70px;
      padding-inline: 20px;
    }

    .settings-body {
      grid-template-columns: 1fr;
      grid-template-rows: auto minmax(0, 1fr);
    }

    nav {
      display: grid;
      grid-template-columns: repeat(4, 1fr);
      gap: 2px;
      padding: 8px 10px;
      border-right: 0;
      border-bottom: 1px solid var(--line);
      background: var(--paper);
    }

    .nav-label,
    .status-card {
      display: none;
    }

    nav > button {
      display: flex;
      min-height: 42px;
      align-items: center;
      justify-content: center;
      padding: 4px 6px;
      text-align: center;
    }

    .nav-icon {
      display: none;
    }

    .nav-copy strong {
      font-size: 10.5px;
    }

    .nav-copy small,
    .nav-chevron {
      display: none;
    }

    form {
      padding: 22px 18px 30px;
    }

    .field {
      display: flex;
      align-items: stretch;
      flex-direction: column;
      gap: 9px;
      padding: 13px;
    }

    .field > .field-icon {
      display: none;
    }

    .field-copy {
      flex-direction: row;
      align-items: baseline;
      justify-content: space-between;
    }

    .settings-foot {
      height: 62px;
      flex-basis: 62px;
      padding-inline: 18px;
      padding-bottom: env(safe-area-inset-bottom);
    }
  }
</style>
