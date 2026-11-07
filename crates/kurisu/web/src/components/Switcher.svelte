<script lang="ts">
  import { app, stateOf } from "../lib/state.svelte";
  let q = $state(""), idx = $state(0);
  let el: HTMLInputElement | undefined = $state();
  $effect(() => { el?.focus(); });
  /** Fuzzy score: substring beats subsequence; name beats description; 0 = no match. */
  function fuzzy(text: string, needle: string): number {
    text = text.toLowerCase();
    if (!needle) return 1;
    const i = text.indexOf(needle);
    if (i >= 0) return 100 - Math.min(i, 50) + (i === 0 ? 50 : 0);
    let t = 0, gaps = 0, last = -1;
    for (const c of needle) {
      const j = text.indexOf(c, t);
      if (j < 0) return 0;
      if (last >= 0) gaps += j - last - 1;
      last = j; t = j + 1;
    }
    return Math.max(1, 40 - gaps);
  }
  const items = $derived.by(() => {
    const n = q.trim().toLowerCase().replace(/^#/, "");
    return app.sessions
      .map((s) => ({ s, score: Math.max(fuzzy(s.name, n) * 2, fuzzy(s.description || "", n), fuzzy(s.note || "", n) / 2) }))
      .filter((x) => x.score > 0)
      .sort((a, b) => Number(a.s.lifecycle === "archived") - Number(b.s.lifecycle === "archived") || b.score - a.score || a.s.name.localeCompare(b.s.name))
      .slice(0, 30)
      .map((x) => x.s);
  });
  function pick(id: string) { app.switcherOpen = false; app.open(id); }
  function onKey(e: KeyboardEvent) {
    if (e.key === "ArrowDown") { idx = Math.min(idx + 1, items.length - 1); e.preventDefault(); }
    if (e.key === "ArrowUp") { idx = Math.max(idx - 1, 0); e.preventDefault(); }
    if (e.key === "Enter" && items[idx]) pick(items[idx].id);
  }
</script>

<div class="overlay" role="presentation" onclick={(e) => { if (e.target === e.currentTarget) app.switcherOpen = false; }}>
  <div class="modal" role="dialog" aria-modal="true" aria-label="Jump to a session">
    <input bind:this={el} bind:value={q} oninput={() => (idx = 0)} onkeydown={onKey} placeholder="Jump to a session (name or description)…" autocomplete="off" />
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
