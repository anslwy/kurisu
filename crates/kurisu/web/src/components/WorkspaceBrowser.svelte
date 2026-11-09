<script lang="ts">
  import { onDestroy } from "svelte";
  type Entry = { name: string; path: string; directory: boolean };
  type Change = { path: string; status: string };
  type Content = { kind: "text" | "large" | "binary"; text?: string; size?: number };
  let { endpoint, token, session, mode, onchange }: {
    endpoint: string; token?: string; session?: string | null; mode: "files" | "git"; onchange: (count: number) => void;
  } = $props();
  let tree = $state<Record<string, Entry[]>>({});
  let expanded = $state<Record<string, boolean>>({});
  let changes = $state<Change[]>([]);
  let hidden = $state(false);
  let selected = $state<string | null>(null);
  let content = $state<Content | null>(null);
  let error = $state("");
  let loading = $state(false);
  let controller = new AbortController();
  onDestroy(() => { ++generation; ++request; controller.abort(); });
  let generation = 0;
  let request = 0;
  async function api<T>(action: string, path = ""): Promise<T> {
    if (!session) throw new Error("Select a session to inspect its workspace.");
    const base = endpoint.endsWith("/_web/terminal") ? endpoint.replace(/\/terminal$/, "/api") : endpoint.replace(/\/terminal$/, "");
    const url = `${base}/sessions/${encodeURIComponent(session)}/workspace/${action}?${new URLSearchParams({ path, hidden: String(hidden) })}`;
    const res = await fetch(url, { credentials: "same-origin", cache: "no-store", signal: controller.signal, headers: token ? { Authorization: `Bearer ${token}` } : {} });
    const data = await res.json();
    if (!res.ok || data.error) throw new Error(data.error || `Request failed (${res.status})`);
    return data.ok;
  }
  // Helper to build download URL
  function downloadUrl(path: string) {
    if (!session) return "";
    const base = endpoint.endsWith("/_web/terminal") ? endpoint.replace(/\/terminal$/, "/api") : endpoint.replace(/\/terminal$/, "");
    return `${base}/sessions/${encodeURIComponent(session)}/workspace/download?${new URLSearchParams({ path })}`;
  }
  async function download(entryPath: string, filename: string) {
    const url = downloadUrl(entryPath);
    const resp = await fetch(url, { credentials: "same-origin", headers: token ? { Authorization: `Bearer ${token}` } : {} });
    if (!resp.ok) { error = `Download failed (${resp.status})`; return; }
    const blob = await resp.blob();
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = filename;
    a.click();
    URL.revokeObjectURL(a.href);
  }
  async function refresh() {
    controller.abort(); controller = new AbortController();
    const version = ++generation;
    ++request; selected = null; content = null; error = ""; loading = true; tree = {}; expanded = {};
    try {
      if (mode === "files") {
        const result = await api<{ entries: Entry[] }>("list-dir");
        if (version === generation) tree = { "": result.entries };
      } else {
        const result = await api<{ entries: Change[] }>("git-status");
        if (version === generation) { changes = result.entries; onchange(changes.length); }
      }
    } catch (e) { if (version === generation) error = String(e); }
    finally { if (version === generation) loading = false; }
  }
  $effect(() => { session; mode; hidden; void refresh(); });
  async function expand(entry: Entry) {
    expanded[entry.path] = !expanded[entry.path];
    if (!expanded[entry.path] || tree[entry.path]) return;
    const version = generation;
    try {
      const result = await api<{ entries: Entry[] }>("list-dir", entry.path);
      if (version === generation) tree[entry.path] = result.entries;
    } catch (e) { if (version === generation) error = String(e); }
  }
  async function open(path: string) {
    const version = ++request; const workspace = generation;
    selected = path; content = null; error = ""; loading = true;
    try {
      const result = await api<Content>(mode === "files" ? "read-file" : "git-diff", path);
      if (workspace === generation && version === request) content = result;
    } catch (e) { if (workspace === generation && version === request) error = String(e); }
    finally { if (workspace === generation && version === request) loading = false; }
  }
</script>

<div class="browser">
  <div class="toolbar">
    {#if selected !== null}<button onclick={() => { ++request; selected = null; content = null; error = ""; loading = false; }}>← Back</button><span class="path" title={selected}>{selected}</span>
    {:else}<span>{mode === "files" ? "Session workspace" : `${changes.length} changed files`}</span>{/if}
    <button class="refresh" onclick={refresh} aria-label="Refresh workspace">Refresh</button>
  </div>
  {#if mode === "files" && selected === null}<label><input type="checkbox" bind:checked={hidden} /> Show hidden build directories</label>{/if}
  {#if error}<p role="alert">{error}</p>{/if}
  {#if loading}<p role="status">Loading…</p>{/if}
  {#if selected !== null}
    {#if content?.kind === "large"}<p>file too large ({content.size} bytes)</p>
    {:else if content?.kind === "binary"}<p>binary file ({content.size} bytes)</p>
    {:else if content?.kind === "text"}
      <!-- svelte-ignore a11y_no_noninteractive_tabindex (keyboard users must scroll the read-only viewer) -->
      <div class="viewer" tabindex="0" role="region" aria-label={mode === "git" ? "File diff" : "File contents"}>
        <pre>{#each (content.text || "").split("\n") as line, i}<span class="line" class:added={mode === "git" && line.startsWith("+")} class:removed={mode === "git" && line.startsWith("-")}><span class="number" aria-hidden="true">{i + 1}</span><span>{line || " "}</span></span>{/each}</pre>
      </div>
    {/if}
  {:else if mode === "files"}
    <div class="entries">{@render directory("", 0)}</div>
  {:else}
    <div class="entries">
      {#each changes as entry}<button class="entry" onclick={() => open(entry.path)}><span class="status">{entry.status}</span><span>{entry.path}</span></button>{/each}
      {#if !loading && !error && changes.length === 0}<p>no changes</p>{/if}
    </div>
  {/if}
</div>

{#snippet directory(path: string, depth: number)}
  {#each tree[path] || [] as entry}
    <div class="row">
      <button class="entry" style:padding-left={`${10 + depth * 16}px`} aria-expanded={entry.directory ? !!expanded[entry.path] : undefined}
        onclick={() => entry.directory ? expand(entry) : open(entry.path)}><span class="marker">{entry.directory ? (expanded[entry.path] ? "▾" : "▸") : "·"}</span><span>{entry.name}</span></button>
      <button class="download" title={`Download ${entry.name}`} aria-label={`Download ${entry.name}`} onclick={() => download(entry.path, entry.name)}>⬇</button>
    </div>
    {#if entry.directory && expanded[entry.path]}{@render directory(entry.path, depth + 1)}{/if}
  {/each}
  {#if tree[path]?.length === 0}<p>Empty directory</p>{/if}
{/snippet}

<style>
  .browser { display: flex; flex-direction: column; flex: 1; min-height: 0; min-width: 0; overflow: hidden; font-size: 13px; }
  .toolbar { display: flex; align-items: center; gap: 8px; padding: 8px; border-bottom: 1px solid var(--line); flex: none; }
  .toolbar > span { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .refresh { margin-left: auto; flex: none; }
  button { color: var(--fg); background: var(--bg); border: 1px solid var(--line); border-radius: 6px; padding: 7px 10px; min-height: 36px; cursor: pointer; }
  button:hover { background: var(--hover); }
  button:focus-visible, .viewer:focus-visible { outline: 2px solid var(--accent); outline-offset: -2px; }
  label { display: flex; gap: 6px; align-items: center; padding: 8px; color: var(--muted); }
  .entries { overflow: auto; flex: 1; min-height: 0; }
  .row { display: flex; align-items: stretch; }
  .row:hover { background: var(--hover); }
  .entry { display: flex; align-items: baseline; gap: 8px; flex: 1; min-width: 0; text-align: left; border: 0; border-radius: 0; background: transparent; font-family: monospace; overflow-wrap: anywhere; }
  .download { flex: none; display: flex; align-items: center; border: 0; background: transparent; color: var(--muted); padding: 0 10px; min-height: 0; font-size: 14px; }
  .download:hover { color: var(--fg); background: transparent; }
  .marker, .status { flex: none; color: var(--muted); min-width: 14px; }
  .status { color: var(--accent); min-width: 24px; }
  .viewer { flex: 1; min-height: 0; overflow: auto; overscroll-behavior: contain; }
  pre { margin: 0; padding: 8px 0; font: 12px/1.6 monospace; min-width: max-content; }
  .line { display: flex; white-space: pre; padding-right: 12px; }
  .number { flex: none; width: 4.5em; text-align: right; padding-right: 12px; color: var(--muted); user-select: none; }
  .added { background: color-mix(in srgb, green 12%, transparent); }
  .removed { background: color-mix(in srgb, red 12%, transparent); }
  p { margin: 12px; color: var(--muted); overflow-wrap: anywhere; }
  [role="alert"] { color: var(--err); }
</style>
