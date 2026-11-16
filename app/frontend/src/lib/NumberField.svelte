<script lang="ts">
  import { Minus, Plus } from "lucide-svelte";

  let {
    label,
    description = "",
    value = $bindable(0),
    min = 0,
    max = 999_999,
    step = 1,
    suffix = "",
    zeroLabel = "默认",
    onchange,
  }: {
    label: string;
    description?: string;
    value: number;
    min?: number;
    max?: number;
    step?: number;
    suffix?: string;
    zeroLabel?: string;
    onchange?: () => void;
  } = $props();

  function setValue(next: number) {
    value = Math.min(max, Math.max(min, Number.isFinite(next) ? next : min));
    onchange?.();
  }
</script>

<div class="number-field">
  <div class="number-copy">
    <strong>{label}</strong>
    {#if description}<span>{description}</span>{/if}
  </div>
  <div class="number-control">
    <button type="button" aria-label={`减少${label}`} onclick={() => setValue(value - step)} disabled={value <= min}>
      <Minus size={13} />
    </button>
    <label>
      <input
        type="number"
        {min}
        {max}
        {step}
        value={value}
        oninput={(event) => setValue(Number(event.currentTarget.value))}
      />
      {#if suffix}<span>{suffix}</span>{/if}
      {#if value === 0 && zeroLabel}<span class="zero">{zeroLabel}</span>{/if}
    </label>
    <button type="button" aria-label={`增加${label}`} onclick={() => setValue(value + step)} disabled={value >= max}>
      <Plus size={13} />
    </button>
  </div>
</div>

<style>
  .number-field {
    display: flex;
    min-height: 66px;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 12px 13px 12px 15px;
    border: 1px solid var(--line);
    border-radius: 12px;
    background: rgba(255, 254, 250, 0.64);
  }

  .number-copy {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 2px;
  }

  .number-copy strong {
    color: var(--ink-soft);
    font-size: 11.5px;
    font-weight: 580;
  }

  .number-copy span {
    color: var(--faint);
    font-size: 9.5px;
    line-height: 1.35;
  }

  .number-control {
    display: flex;
    height: 32px;
    flex: 0 0 auto;
    align-items: center;
    border: 1px solid var(--line);
    border-radius: 9px;
    background: var(--surface);
    box-shadow: var(--shadow-xs);
  }

  .number-control > button {
    display: grid;
    width: 29px;
    height: 30px;
    place-items: center;
    color: var(--muted);
  }

  .number-control > button:hover:not(:disabled) {
    color: var(--ink);
  }

  .number-control > button:disabled {
    opacity: 0.3;
  }

  label {
    position: relative;
    display: flex;
    height: 30px;
    min-width: 62px;
    align-items: center;
    justify-content: center;
    border-right: 1px solid var(--line);
    border-left: 1px solid var(--line);
  }

  input {
    width: 48px;
    border: 0;
    outline: 0;
    background: transparent;
    color: var(--ink);
    font-family: var(--mono);
    font-size: 10px;
    text-align: center;
    appearance: textfield;
    -moz-appearance: textfield;
  }

  input::-webkit-inner-spin-button,
  input::-webkit-outer-spin-button {
    margin: 0;
    appearance: none;
  }

  label > span {
    color: var(--faint);
    font-size: 8px;
  }

  label > .zero {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    background: var(--surface);
    color: var(--muted);
    pointer-events: none;
  }

  @media (max-width: 480px) {
    .number-field {
      align-items: flex-start;
      flex-direction: column;
    }

    .number-control {
      align-self: stretch;
      justify-content: space-between;
    }

    .number-control > button {
      flex: 0 0 36px;
    }

    label {
      flex: 1;
    }
  }
</style>
