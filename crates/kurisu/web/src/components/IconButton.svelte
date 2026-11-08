<script lang="ts">
  import type { Snippet } from "svelte";
  // The one header icon-button in both chat headers: a 36px transparent button
  // that takes the theme's muted colour and only fills in on hover/pressed.
  // Callers supply the 20px, stroke-width 1.6 glyph as children.
  let {
    title,
    label,
    pressed = false,
    expanded,
    onpress,
    children,
  }: {
    title: string;
    label?: string;
    pressed?: boolean;
    expanded?: boolean;
    onpress: () => void;
    children: Snippet;
  } = $props();
</script>

<button class="icon-button" class:pressed title={title} aria-label={label ?? title} aria-expanded={expanded} onclick={onpress}>
  {@render children()}
</button>

<style>
  .icon-button { display: inline-grid; place-items: center; width: 36px; height: 36px; flex: none; padding: 0; border: 0; border-radius: 6px; background: transparent; color: var(--muted); cursor: pointer; }
  .icon-button:hover { background: var(--hover); color: var(--fg); }
  .icon-button.pressed { color: var(--accent); background: color-mix(in srgb, var(--accent) 12%, transparent); }
  .icon-button:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
</style>
