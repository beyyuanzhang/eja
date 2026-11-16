<script lang="ts">
  import { PencilLine, ShieldCheck, Sparkles, Target } from "lucide-svelte";
  import type { Proposal } from "./types";

  let {
    proposal,
    pending = false,
    onApprove,
    onDismiss,
  }: {
    proposal: Proposal;
    pending?: boolean;
    onApprove: (id: number) => void;
    onDismiss: (id: number) => void;
  } = $props();

  const kinds: Record<string, { label: string; Icon: typeof Target }> = {
    goal: { label: "目标", Icon: Target },
    commitment: { label: "规矩", Icon: ShieldCheck },
    correction: { label: "更正", Icon: PencilLine },
  };

  const presentation = $derived(kinds[proposal.kind] ?? { label: "变更", Icon: Sparkles });
</script>

<article class="proposal">
  <header>
    <span class="kind">
      <presentation.Icon size={13} strokeWidth={1.8} />
      {presentation.label}
    </span>
    <span class="prompt">需要你确认</span>
  </header>
  <p>{proposal.text}</p>
  <footer>
    <button class="primary-button" type="button" disabled={pending} onclick={() => onApprove(proposal.id)}>
      批准并继续
    </button>
    <button class="text-button" type="button" disabled={pending} onclick={() => onDismiss(proposal.id)}>
      先放着
    </button>
  </footer>
</article>

<style>
  .proposal {
    position: relative;
    overflow: hidden;
    padding: 18px 20px 17px;
    border: 1px solid rgba(189, 96, 66, 0.2);
    border-radius: 18px;
    background:
      radial-gradient(100% 120% at 0 0, rgba(255, 255, 255, 0.6), transparent 58%),
      linear-gradient(135deg, #fbf3ee, #f6e9e2);
    box-shadow: 0 7px 24px rgba(115, 63, 43, 0.06);
  }

  .proposal::before {
    position: absolute;
    inset: 0 auto 0 0;
    width: 3px;
    background: linear-gradient(180deg, #d07859, #b75438);
    content: "";
  }

  header {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 9px;
  }

  .kind {
    display: inline-flex;
    height: 22px;
    align-items: center;
    gap: 5px;
    padding: 0 8px;
    border: 1px solid rgba(150, 69, 45, 0.14);
    border-radius: 999px;
    background: rgba(255, 255, 255, 0.54);
    color: var(--accent-deep);
    font-size: 10px;
    font-weight: 650;
    letter-spacing: 0.04em;
  }

  .prompt {
    color: rgba(127, 67, 48, 0.69);
    font-size: 10.5px;
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }

  p {
    max-width: 58ch;
    margin: 0;
    color: #3b302b;
    font-family: var(--serif);
    font-size: 18px;
    line-height: 1.6;
    white-space: pre-wrap;
  }

  footer {
    display: flex;
    align-items: center;
    gap: 13px;
    margin-top: 15px;
  }

  footer .primary-button {
    height: 34px;
    padding: 0 15px;
    font-size: 12px;
  }
</style>
