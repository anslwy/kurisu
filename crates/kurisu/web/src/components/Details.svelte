<script lang="ts">
  import { app, stateOf } from "../lib/state.svelte";

  const s = $derived(app.current!);
  let desc = $state(""), model = $state(""), effort = $state("");
  let fb = $state(""), ok = $state(true), showAudit = $state(false);
  $effect(() => { app.selected; desc = s.description || ""; model = s.model; effort = s.effort || ""; fb = ""; showAudit = false; });

  async function update(body: Record<string, string>) {
    try {
      const r = await app.api!.update(s.id, body);
      const ch = r.changes.map((c) => `${c.field} → ${c.new || "(default)"}`).join(", ");
      fb = (ch ? "Updated " + ch : "Nothing changed") + (r.warning ? " — warning: " + r.warning : ""); ok = !r.warning;
      app.sessions = await app.api!.sessions();
    } catch (e: any) { fb = "✗ " + e.message; ok = false; }
  }
  function save() {
    const body: Record<string, string> = {};
    if (desc.trim() !== (s.description || "")) body.description = desc.trim();
    if (model.trim() && model.trim() !== s.model) body.model = model.trim();
    if (effort && effort !== (s.effort || "")) body.effort = effort;
    if (!Object.keys(body).length) { fb = "Nothing changed"; ok = false; return; }
    update(body);
  }
  function rename() { const n = prompt("New name (lowercase letters, digits, '-')", s.name); if (n && n.trim() !== s.name) update({ name: n.trim() }); }
  function pause() { update({ lifecycle: s.lifecycle === "paused" ? "active" : "paused" }); }
  function archive() {
    if (s.lifecycle === "archived") return update({ lifecycle: "active" });
    if (confirm(`Archive #${s.name}? Its CLI stops and it moves to Archived.`)) update({ lifecycle: "archived" });
  }
  async function stop() {
    if (!confirm(`Stop the CLI of #${s.name}? It resumes on its next message.`)) return;
    try { const r = await app.api!.stop(s.id); fb = r.was_running ? "CLI stopped" : "It was not running"; ok = true; }
    catch (e: any) { fb = "✗ " + e.message; ok = false; }
  }
</script>

<aside>
  <div class="dhead"><b>Details</b><button class="hbtn" aria-label="Close" onclick={() => (app.detailsOpen = false)}>✕</button></div>
  <div class="dbody">
    <div class="kv">
      <div><span class="k">Session:</span> #{s.name}</div>
      <div><span class="k">Role:</span> {s.role} · {s.harness}</div>
      <div><span class="k">Status:</span> {stateOf(s)[1]}</div>
      <div><span class="k">Created by:</span>
        {#if s.created_by_session}<button class="a" onclick={() => app.open(s.created_by_session!)}>#{s.created_by_name ?? s.created_by_session.slice(0, 8)}</button>{:else}you{/if}</div>
      <div><span class="k">Created:</span>
        {#each s.children ?? [] as c, i (c.id)}{i ? ", " : ""}<button class="a" onclick={() => app.open(c.id)}>#{c.name}</button>{:else}none{/each}</div>
      <div><span class="k">Folder:</span> {s.cwd}</div>
      <div><span class="k">ID:</span> {s.id}</div>
      {#if s.note}<div><span class="k">Status note:</span> {s.note}</div>{/if}
    </div>
    <div class="field"><label for="dDesc">Topic / description</label><textarea id="dDesc" bind:value={desc}></textarea></div>
    <div class="field"><label for="dModel">Model</label><input id="dModel" list="models" bind:value={model} /></div>
    <datalist id="models"><option>claude-haiku-5-5</option><option>claude-sonnet-5-5</option><option>claude-opus-5-5</option><option>claude-fable-5-1</option></datalist>
    <div class="field"><label for="dEffort">Effort</label>
      <select id="dEffort" bind:value={effort}><option value="">(default)</option><option>low</option><option>medium</option><option>high</option><option>xhigh</option><option>max</option></select></div>
    <div class="actions"><button class="btn" onclick={save}>Save changes</button></div>
    <div class="feedback" class:ok class:bad={!ok}>{fb}</div>
    <h4>Actions</h4>
    <div class="actions">
      <button class="btn ghost" onclick={rename}>Rename</button>
      <button class="btn ghost" onclick={pause} disabled={s.lifecycle === "archived"}>{s.lifecycle === "paused" ? "Resume" : "Pause"}</button>
      <button class="btn ghost" onclick={stop}>Stop CLI</button>
      <button class="btn danger" onclick={archive}>{s.lifecycle === "archived" ? "Unarchive" : "Archive"}</button>
    </div>
    <h4>Audit log</h4>
    <div><button class="btn ghost" onclick={() => (showAudit = !showAudit)}>{showAudit ? "Hide audit log" : "Show audit log"}</button></div>
    {#if showAudit}
      <div class="audit">
        {#each app.audit as a (a.seq)}
          <div>{new Date(a.at * 1000).toLocaleString()} · {a.actor_name} · {a.field.replace("event:", "")}{a.new ? " → " + a.new : ""}</div>
        {:else}<div>no entries</div>{/each}
      </div>
    {/if}
  </div>
</aside>

<style>
  aside { border-left: 1px solid var(--line); background: var(--bg); display: flex; flex-direction: column; min-width: 0; height: 100dvh; }
  .dhead { display: flex; align-items: center; justify-content: space-between; padding: 12px 14px; border-bottom: 1px solid var(--line); min-height: 54px; }
  .dhead b { font-size: 15px; }
  .dbody { overflow-y: auto; padding: 14px; display: flex; flex-direction: column; gap: 14px; }
  .kv { font-size: 13px; overflow-wrap: anywhere; } .kv div { padding: 2px 0; } .k { color: var(--muted); }
  .a { border: 0; background: none; padding: 0; color: var(--link); }
  .a:hover { text-decoration: underline; }
  .field label { display: block; font-size: 12px; color: var(--muted); margin-bottom: 3px; font-weight: 600; }
  .field textarea { min-height: 56px; resize: vertical; }
  h4 { margin: 4px 0 0; font-size: 12px; text-transform: uppercase; letter-spacing: .04em; color: var(--muted); }
  .audit { font-size: 12px; color: var(--muted); display: flex; flex-direction: column; gap: 3px; }
  @media (max-width: 1100px) { aside { position: fixed; right: 0; top: 0; bottom: 0; width: 340px; z-index: 15; box-shadow: -8px 0 24px rgba(0,0,0,.2); } }
  @media (max-width: 760px) { aside { width: 100%; } }
</style>
