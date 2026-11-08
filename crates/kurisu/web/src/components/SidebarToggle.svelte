<script lang="ts">
  import IconButton from "./IconButton.svelte";
  // Hides or shows the left sessions sidebar, at the far left of the header.
  // Highlighted means the sidebar is visible, exactly like the terminal
  // toggle; the title says what pressing it will do. The phone layout has its
  // own ☰ drawer, so this button is hidden below 760px.
  let { collapsed = false, ontoggle }: { collapsed?: boolean; ontoggle: () => void } = $props();
  const label = $derived(collapsed ? "Show sessions" : "Hide sessions");
</script>

<span class="sidebar-toggle">
  <IconButton title={label} pressed={!collapsed} expanded={!collapsed} onpress={ontoggle}>
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true">
      <path d="M9 4H5a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h4Z" fill="currentColor" stroke="none" />
      <rect x="3" y="4" width="18" height="16" rx="2" /><path d="M9 4v16" />
    </svg>
  </IconButton>
</span>

<style>
  .sidebar-toggle { display: inline-flex; flex: none; }
  @media (max-width: 760px) { .sidebar-toggle { display: none; } }
</style>
