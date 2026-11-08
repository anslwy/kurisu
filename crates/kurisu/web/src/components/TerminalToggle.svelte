<script lang="ts">
  let { open = false, ontoggle }: { open?: boolean; ontoggle: () => void } = $props();
</script>

<button class="terminal-toggle" class:pressed={open} title="Terminal" aria-label="Terminal" aria-expanded={open} onclick={ontoggle}>
  <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true">
    <path d="M14 4h5a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2h-5Z" fill="currentColor" stroke="none" />
    <rect x="3" y="4" width="18" height="16" rx="2" /><path d="M14 4v16" />
  </svg>
</button>

<style>
  .terminal-toggle { display: inline-grid; place-items: center; width: 36px; height: 36px; flex: none; margin-left: auto; padding: 0; border: 0; border-radius: 6px; background: transparent; color: var(--muted); cursor: pointer; }
  .terminal-toggle:hover { background: var(--hover); color: var(--fg); }
  .terminal-toggle.pressed { color: var(--accent); background: color-mix(in srgb, var(--accent) 12%, transparent); }
  .terminal-toggle:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
</style>
