<script lang="ts">
  import IconButton from "./IconButton.svelte";
  let { open = false, ontoggle }: { open?: boolean; ontoggle: () => void } = $props();
</script>

<IconButton title="Terminal" pressed={open} expanded={open} onpress={ontoggle}>
  <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true">
    <path d="M12 4h7a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2h-5Z" fill="currentColor" stroke="none" />
    <rect x="3" y="4" width="18" height="16" rx="2" /><path d="M12 4v16" />
  </svg>
</IconButton>
