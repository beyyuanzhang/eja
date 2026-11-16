<script lang="ts">
  import DOMPurify from "dompurify";
  import { Marked } from "marked";

  let { text }: { text: string } = $props();

  const markdown = new Marked({
    gfm: true,
    breaks: true,
  });

  const html = $derived(
    DOMPurify.sanitize(markdown.parse(text || "") as string, {
      USE_PROFILES: { html: true },
      FORBID_TAGS: ["style", "iframe", "form", "input", "button"],
      FORBID_ATTR: ["style"],
    }),
  );
</script>

<div class="markdown">{@html html}</div>

<style>
  .markdown {
    color: var(--ink-soft);
    font-family: var(--serif);
    font-size: 18px;
    line-height: 1.75;
    overflow-wrap: anywhere;
  }

  .markdown :global(p) {
    margin: 0 0 0.9em;
  }

  .markdown :global(p:last-child) {
    margin-bottom: 0;
  }

  .markdown :global(h1),
  .markdown :global(h2),
  .markdown :global(h3),
  .markdown :global(h4) {
    margin: 1.2em 0 0.45em;
    color: var(--ink);
    font-family: var(--sans);
    font-size: 15px;
    font-weight: 650;
    letter-spacing: -0.01em;
  }

  .markdown :global(h1:first-child),
  .markdown :global(h2:first-child),
  .markdown :global(h3:first-child) {
    margin-top: 0;
  }

  .markdown :global(ul),
  .markdown :global(ol) {
    margin: 0.2em 0 1em;
    padding-left: 1.35em;
  }

  .markdown :global(li) {
    margin: 0.25em 0;
    padding-left: 0.15em;
  }

  .markdown :global(li::marker) {
    color: var(--faint);
  }

  .markdown :global(strong) {
    color: var(--ink);
    font-weight: 650;
  }

  .markdown :global(a) {
    color: var(--accent-deep);
    text-decoration-color: color-mix(in srgb, var(--accent) 35%, transparent);
    text-underline-offset: 3px;
  }

  .markdown :global(code) {
    border: 1px solid var(--line);
    border-radius: 5px;
    background: var(--surface-2);
    padding: 0.1em 0.36em;
    font-family: var(--mono);
    font-size: 0.78em;
    color: var(--ink-soft);
  }

  .markdown :global(pre) {
    margin: 0.7em 0 1em;
    padding: 14px 16px;
    border: 1px solid var(--line);
    border-radius: 12px;
    background: #f3f1eb;
    overflow-x: auto;
  }

  .markdown :global(pre code) {
    border: 0;
    background: transparent;
    padding: 0;
    font-size: 12px;
    line-height: 1.65;
  }

  .markdown :global(blockquote) {
    margin: 1em 0;
    padding: 2px 0 2px 15px;
    border-left: 2px solid var(--line-strong);
    color: var(--muted);
  }

  @media (max-width: 640px) {
    .markdown {
      font-size: 17px;
      line-height: 1.72;
    }
  }
</style>
