<script lang="ts">
  import { app } from "../lib/state.svelte";
  let mode: "lead" | "worker" = $state("lead");
  let task = $state(""), name = $state(""), desc = $state(""), model = $state(""), fb = $state("");
  let taskEl: HTMLTextAreaElement | undefined = $state();
  $effect(() => { taskEl?.focus(); });

  const liveLead = () => app.sessions.find((s) => s.role === "lead" && s.lifecycle !== "archived" && s.name === "lead")
    ?? app.sessions.find((s) => s.role === "lead" && s.lifecycle !== "archived");

  async function go() {
    fb = "";
    try {
      if (mode === "lead") {
        const text = task.trim(); if (!text) { fb = "Type a task first."; return; }
        app.addOpen = false;
        let lead = liveLead();
        if (lead) { await app.open(lead.id); app.send(text); return; }
        const req = app.api!.lead(text); // creates the Lead, then sends the task
        for (let i = 0; i < 20 && !lead; i++) { await new Promise((r) => setTimeout(r, 500)); app.sessions = await app.api!.sessions(); lead = liveLead(); }
        if (lead) await app.open(lead.id);
        await req.catch(() => {}); // errors arrive as error messages in the channel
      } else {
        if (!name.trim()) { fb = "A name is required."; return; }
        const r = await app.api!.create(name.trim(), desc.trim(), model.trim() || undefined);
        app.addOpen = false;
        app.sessions = await app.api!.sessions();
        await app.open(r.id);
        if (r.warning) alert("Warning: " + r.warning);
      }
    } catch (e: any) { fb = "✗ " + e.message; }
  }
</script>

<div class="overlay" role="presentation" onclick={(e) => { if (e.target === e.currentTarget) app.addOpen = false; }}>
  <div class="modal" role="dialog" aria-modal="true" aria-label="Add">
    <h3>Add</h3>
    <div class="tabs">
      <button class:on={mode === "lead"} onclick={() => (mode = "lead")}>Task for the Lead</button>
      <button class:on={mode === "worker"} onclick={() => (mode = "worker")}>New session</button>
    </div>
    {#if mode === "lead"}
      <textarea bind:this={taskEl} bind:value={task} placeholder="What should the Lead do? It can create sessions and delegate to them."></textarea>
    {:else}
      <input bind:value={name} placeholder="name, e.g. researcher" />
      <input bind:value={desc} placeholder="topic / description" />
      <input bind:value={model} placeholder="model (default claude-haiku-5-5)" list="models" />
    {/if}
    <div class="feedback bad">{fb}</div>
    <div class="actions" style="justify-content:flex-end">
      <button class="btn ghost" onclick={() => (app.addOpen = false)}>Cancel</button><button class="btn" onclick={go}>Start</button>
    </div>
  </div>
</div>

<style>
  .tabs { display: flex; gap: 6px; }
  .tabs button { border: 1px solid var(--line); background: transparent; border-radius: 6px; padding: 4px 10px; }
  .tabs button.on { background: var(--accent); color: var(--accent-fg); border-color: var(--accent); }
  textarea { min-height: 90px; resize: vertical; }
</style>
