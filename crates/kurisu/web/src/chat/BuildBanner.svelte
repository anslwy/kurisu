<script lang="ts">
  let { warning = "" }: { warning?: string } = $props();
</script>
{#if warning}<div class="build-warning" role="status">{warning}</div>{/if}
<style>
  .build-warning { flex-shrink: 0; padding: 6px 14px; color: var(--fg); background: var(--bg2); border-bottom: 1px solid var(--accent); font-size: 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
</style>
