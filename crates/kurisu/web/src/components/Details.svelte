<script lang="ts">
  import { untrack } from "svelte";
  import { app, stateOf } from "../lib/state.svelte";

  const s = $derived(app.current!);
  let modelOptions = $state<{ id: string; label: string }[]>([]);
  let effortOptions = $state<string[]>([]);
  $effect(() => {
    const harness = s.harness, selectedModel = model.trim();
    let cancelled = false;
    if (harness === "opencode") {
      app.api?.models(harness, selectedModel || undefined).then((r) => {
        if (cancelled) return;
        modelOptions = r.models;
        effortOptions = r.config_options?.find((c) => c.id === "effort")?.options?.map((o) => o.value) || [];
      }).catch(() => { if (!cancelled) { modelOptions = []; effortOptions = []; } });
    } else {
      effortOptions = harness === "agy" ? ["low", "medium", "high"] : harness === "codex" || harness === "fx" ? ["low", "medium", "high", "xhigh"] : ["low", "medium", "high", "xhigh", "max"];
      app.api?.models(harness).then((r) => { if (!cancelled) modelOptions = r.models; }).catch(() => { if (!cancelled) modelOptions = []; });
    }
    return () => { cancelled = true; };
  });
  let desc = $state(""), model = $state(""), effort = $state("");
  let fb = $state(""), ok = $state(true), showAudit = $state(false);
  let renaming = $state(false), newName = $state("");
  let nameEl: HTMLInputElement | undefined = $state();
  // Reset the form only when another session is opened: live updates of this
  // session (status, activity, cost) must not wipe what the user is typing.
  $effect(() => {
    app.selected;
    untrack(() => { desc = s.description || ""; model = s.model; effort = s.effort || ""; fb = ""; showAudit = false; renaming = false; });
  });
  const running = $derived(s.lifecycle === "active" && !["new", "stopped"].includes(s.status));

  async function update(body: Record<string, string>) {
    try {
      const r = await app.api!.update(s.id, body);
      const ch = r.changes.map((c) => `${c.field} → ${c.new || "(default)"}`).join(", ");
      fb = (ch ? "Updated " + ch : "Nothing changed") + (r.warning ? " — warning: " + r.warning : ""); ok = !r.warning;
      app.sessions = await app.api!.sessions();
      app.syncHash();
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
  function startRename() { newName = s.name; renaming = true; queueMicrotask(() => { nameEl?.focus(); nameEl?.select(); }); }
  async function commitRename() {
    const n = newName.trim();
    if (!n || n === s.name) { renaming = false; return; }
    await update({ name: n });
    if (ok) renaming = false;
  }
  function renameKey(e: KeyboardEvent) {
    if (e.key === "Enter") { e.preventDefault(); commitRename(); }
    if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); renaming = false; }
  }
  function pause() { update({ lifecycle: s.lifecycle === "paused" ? "active" : "paused" }); }
  function archive() {
    if (s.lifecycle === "archived") return update({ lifecycle: "active" });
    if (confirm(`Archive #${s.name}? Its CLI stops and it moves to Archived.`)) update({ lifecycle: "archived" });
  }
  async function stop() {
    if (!running) return;
    if (!confirm(`Stop the CLI of #${s.name}? Its transcript is kept, and it resumes on its next message.`)) return;
    try { const r = await app.api!.stop(s.id); fb = r.was_running ? "CLI stopped" : "It was not running"; ok = true; }
    catch (e: any) { fb = "✗ " + e.message; ok = false; }
  }
</script>

<aside>
  <div class="dhead"><b>Details</b><button class="hbtn" aria-label="Close" onclick={() => (app.detailsOpen = false)}>✕</button></div>
  <div class="dbody">
    <div class="kv">
      <div><span class="k">Session:</span>
        {#if renaming}
          <span class="rename">#<input bind:this={nameEl} bind:value={newName} onkeydown={renameKey} aria-label="New name"
            pattern="[a-z][a-z0-9-]*" maxlength="32" /><button class="btn" onclick={commitRename}>Save</button><button class="btn ghost" onclick={() => (renaming = false)}>Cancel</button></span>
          <div class="hint">lowercase letters, digits and '-'; the id stays the same</div>
        {:else}#{s.name} <button class="a" onclick={startRename}>rename</button>{/if}
      </div>
      <div><span class="k">Role:</span> {s.role} · {s.harness}</div>
      <div><span class="k">Status:</span> {stateOf(s)[1]}</div>
      <div><span class="k">Created by:</span>
        {#if s.created_by_session}<button class="a" onclick={() => app.open(s.created_by_session!)}>#{s.created_by_name ?? s.created_by_session.slice(0, 8)}</button>{:else}you{/if}</div>
      <div><span class="k">Children:</span>
        {#each s.children ?? [] as c, i (c.id)}{i ? ", " : ""}<button class="a" onclick={() => app.open(c.id)}>#{c.name}</button>{:else}<span class="muted">none</span>{/each}</div>
      <div><span class="k">Folder:</span> {s.cwd}</div>
      <div><span class="k">ID:</span> {s.id}</div>
      {#if s.note}<div><span class="k">Status note:</span> {s.note}</div>{/if}
    </div>
    <div class="field"><label for="dDesc">Topic / description</label><textarea id="dDesc" bind:value={desc}></textarea></div>
    <div class="field"><label for="dModel">Model</label><input id="dModel" list="models" bind:value={model} /></div>
    <datalist id="models">{#each modelOptions as m (m.id)}<option value={m.id}>{m.label}</option>{/each}</datalist>
    <div class="field"><label for="dEffort">Effort</label>
      <select id="dEffort" bind:value={effort}><option value="">(default)</option>{#each effortOptions as e (e)}<option>{e}</option>{/each}</select></div>
    <div class="actions"><button class="btn" onclick={save}>Save changes</button></div>
    <div class="feedback" class:ok class:bad={!ok}>{fb}</div>
    <h4>Actions</h4>
    <div class="actions">
      <button class="btn ghost" onclick={pause} disabled={s.lifecycle === "archived"}
        title={s.lifecycle === "paused" ? "Allow messages again" : "Stop its CLI and refuse messages until resumed"}>{s.lifecycle === "paused" ? "Resume" : "Pause"}</button>
      <button class="btn ghost" onclick={stop} disabled={!running}
        title={running ? "Stop the CLI process now; it resumes on its next message" : s.lifecycle !== "active" ? `Its CLI is not running (session ${s.lifecycle})` : "Its CLI is not running"}>Stop CLI</button>
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
  .rename { display: inline-flex; gap: 4px; align-items: center; }
  .rename input { width: 130px; padding: 2px 6px; }
  .rename .btn { padding: 2px 8px; font-size: 12px; }
  .hint { font-size: 11px; color: var(--muted); }
  .field label { display: block; font-size: 12px; color: var(--muted); margin-bottom: 3px; font-weight: 600; }
  .field textarea { min-height: 56px; resize: vertical; }
  h4 { margin: 4px 0 0; font-size: 12px; text-transform: uppercase; letter-spacing: .04em; color: var(--muted); }
  .audit { font-size: 12px; color: var(--muted); display: flex; flex-direction: column; gap: 3px; }
  @media (max-width: 1100px) { aside { position: fixed; right: 0; top: 0; bottom: 0; width: 340px; z-index: 15; box-shadow: -8px 0 24px rgba(0,0,0,.2); } }
  @media (max-width: 760px) { aside { width: 100%; } }
</style>
