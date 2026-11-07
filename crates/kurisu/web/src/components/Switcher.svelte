<script lang="ts">
  import { app, stateOf } from "../lib/state.svelte";
  let q = $state(""), idx = $state(0);
  let el: HTMLInputElement | undefined = $state();
  $effect(() => { el?.focus(); });
  const items = $derived(app.sessions
    .filter((s) => !q.trim() || s.name.includes(q.trim().toLowerCase()) || (s.description || "").toLowerCase().includes(q.trim().toLowerCase()))
    .sort((a, b) => Number(a.lifecycle === "archived") - Number(b.lifecycle === "archived") || a.name.localeCompare(b.name))
    .slice(0, 30));
  function pick(id: string) { app.switcherOpen = false; app.open(id); }
  function onKey(e: KeyboardEvent) {
    if (e.key === "ArrowDown") { idx = Math.min(idx + 1, items.length - 1); e.preventDefault(); }
    if (e.key === "ArrowUp") { idx = Math.max(idx - 1, 0); e.preventDefault(); }
    if (e.key === "Enter" && items[idx]) pick(items[idx].id);
  }
</script>

<div class="overlay" role="presentation" onclick={(e) => { if (e.target === e.currentTarget) app.switcherOpen = false; }}>
  <div class="modal" role="dialog" aria-modal="true" aria-label="Jump to a session">
    <input bind:this={el} bind:value={q} oninput={() => (idx = 0)} onkeydown={onKey} placeholder="Jump to a session…" autocomplete="off" />
    <div class="list">
      {#each items as s, i (s.id)}
        <button class="qs" class:on={i === idx} onclick={() => pick(s.id)} onmouseenter={() => (idx = i)}>
          <span class="dot {stateOf(s)[0]}"></span><b>#{s.name}</b><span class="d">{s.lifecycle === "archived" ? "archived · " : ""}{s.description}</span>
        </button>
      {:else}<div class="muted">No matching session</div>{/each}
    </div>
  </div>
</div>

<style>
  .list { max-height: 50vh; overflow-y: auto; display: flex; flex-direction: column; }
  .qs { padding: 6px 10px; border-radius: 6px; display: flex; gap: 8px; align-items: center; border: 0; background: none; text-align: left; }
  .qs.on { background: var(--accent); color: var(--accent-fg); }
  .d { opacity: .7; font-size: 12px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
</style>
