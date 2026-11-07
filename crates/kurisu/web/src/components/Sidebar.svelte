<script lang="ts">
  import { app, stateOf } from "../lib/state.svelte";
  import type { Session } from "../lib/api";

  type Row = { s: Session; depth: number; kids: boolean };
  const live = $derived(app.sessions.filter((s) => s.lifecycle !== "archived"));
  const archived = $derived(app.sessions.filter((s) => s.lifecycle === "archived"));
  const treeRows = $derived.by(() => {
    const ids = new Set(live.map((s) => s.id));
    const kids = (id: string) => live.filter((s) => s.created_by_session === id).sort((a, b) => a.name.localeCompare(b.name));
    const roots = live.filter((s) => !s.created_by_session || !ids.has(s.created_by_session))
      .sort((a, b) => Number(b.role === "lead") - Number(a.role === "lead") || a.name.localeCompare(b.name));
    const out: Row[] = [];
    const walk = (s: Session, depth: number) => {
      const k = kids(s.id);
      out.push({ s, depth, kids: k.length > 0 });
      if (!app.closed.includes(s.id)) k.forEach((c) => walk(c, depth + 1));
    };
    roots.forEach((r) => walk(r, 0));
    return out;
  });
  const leads = $derived(live.filter((s) => s.role === "lead").map((s) => ({ s, depth: 0, kids: false })));
  const workers = $derived(live.filter((s) => s.role !== "lead").map((s) => ({ s, depth: 0, kids: false })));
  const archivedRows = $derived(archived.map((s) => ({ s, depth: 0, kids: false })));
  const rss = $derived(app.serverRss ? `${Math.round(app.serverRss / 102.4) / 10} MB` : "");
</script>

{#snippet channel(r: Row)}
  {@const [dot, label] = stateOf(r.s)}
  {@const n = app.unread(r.s)}
  <div class="ch" class:sel={r.s.id === app.selected} class:unread={n > 0} style="padding-left:{6 + r.depth * 14}px"
       title="{r.s.name} · {label}{r.s.description ? ' · ' + r.s.description : ''}"
       role="button" tabindex="0" onclick={() => app.open(r.s.id)} onkeydown={(e) => e.key === "Enter" && app.open(r.s.id)}>
    {#if r.kids}
      <button class="tw" tabindex="-1" aria-label={app.closed.includes(r.s.id) ? "Expand" : "Collapse"}
              onclick={(e) => { e.stopPropagation(); app.toggle(r.s.id); }}>{app.closed.includes(r.s.id) ? "▸" : "▾"}</button>
    {:else}<span class="tw" aria-hidden="true"></span>{/if}
    <span class="hash">#</span><span class="cname">{r.s.name}</span>
    {#if r.s.role === "lead"}<span class="role">lead</span>{/if}
    {#if n}<span class="mbadge">{n > 99 ? "99+" : n}</span>{/if}
    <span class="dot {dot}" title={label}></span>
  </div>
{/snippet}

{#snippet section(key: string, title: string, rows: Row[], opt: boolean)}
  <div class="section">
    <div class="shead" class:closed={app.closed.includes(key)} role="button" tabindex="0"
         onclick={() => app.toggle(key)} onkeydown={(e) => e.key === "Enter" && app.toggle(key)}>
      <span class="caret">▾</span><span class="grow">{title}</span>
      {#if opt}
        <span class="seg" role="radiogroup" aria-label="Group sessions">
          <button class="opt" role="radio" aria-checked={app.groupBy === "tree"} class:on={app.groupBy === "tree"}
                  onclick={(e) => { e.stopPropagation(); app.setGroup("tree"); }}>Tree</button><button class="opt" role="radio"
                  aria-checked={app.groupBy === "role"} class:on={app.groupBy === "role"}
                  onclick={(e) => { e.stopPropagation(); app.setGroup("role"); }}>Role</button>
        </span>
      {/if}
    </div>
    {#if !app.closed.includes(key)}{#each rows as r (r.s.id)}{@render channel(r)}{/each}{/if}
  </div>
{/snippet}

<nav class:open={app.navOpen}>
  <div class="ws">
    <b>Kurisu<small>{live.length} sessions{rss ? " · server " + rss : ""}{app.connected ? "" : " · reconnecting…"}</small></b>
    <button class="sbtn" title="Add a session or give the Lead a task" aria-label="Add" onclick={() => (app.addOpen = true)}>＋</button>
  </div>
  <button class="jump" onclick={() => (app.switcherOpen = true)}>Jump to… <kbd>Ctrl K</kbd></button>
  {#if app.approvals.length}
    <button class="apprs" onclick={() => app.open(app.approvals[0].session)}
      title={app.approvals.map((a) => `#${a.session_name}: ${a.title}`).join("\n")}>
      {app.approvals.length} approval{app.approvals.length > 1 ? "s" : ""} waiting · #{app.approvals[0].session_name}</button>
  {/if}
  <div class="channels">
    {#if app.groupBy === "tree"}
      {@render section("__sessions", "Sessions", treeRows, true)}
    {:else}
      {@render section("__leads", "Leads", leads, true)}
      {@render section("__workers", "Workers", workers, false)}
    {/if}
    {#if !live.length}
      <div class="side-empty">{archived.length ? "No active sessions. Press ＋ to start, or open Archived below." : "No sessions yet. Press ＋ to give the Lead a task."}</div>
    {/if}
    {#if archived.length}{@render section("__archived", `Archived (${archived.length})`, archivedRows, false)}{/if}
  </div>
</nav>

<style>
  nav { background: var(--side); color: var(--side-fg); display: flex; flex-direction: column; min-width: 0; height: 100dvh; }
  .ws { display: flex; align-items: center; gap: 8px; padding: 12px 14px; border-bottom: 1px solid rgba(255,255,255,.07); }
  .ws b { font-size: 16px; flex: 1; color: #fff; }
  .ws small { display: block; font-size: 11px; color: var(--side-muted); font-weight: 400; }
  .sbtn { border: 0; background: transparent; color: var(--side-fg); width: 28px; height: 28px; border-radius: 6px; font-size: 17px; }
  .sbtn:hover { background: var(--side-hover); }
  .apprs { margin: 6px 12px 0; border: 0; border-radius: 6px; padding: 5px 10px; background: var(--appr); color: #fff; font-size: 12.5px; font-weight: 600; text-align: left; }
  .jump { margin: 10px 12px 4px; border: 1px solid rgba(255,255,255,.12); background: transparent; border-radius: 6px; padding: 5px 10px; color: var(--side-muted); font-size: 13px; display: flex; justify-content: space-between; text-align: left; }
  .jump:hover { background: var(--side-hover); }
  .jump kbd { font: 11px ui-monospace, monospace; opacity: .8; }
  .channels { overflow-y: auto; flex: 1; padding: 6px 0 12px; }
  .section { margin-top: 10px; }
  .shead { display: flex; align-items: center; gap: 4px; padding: 2px 14px; color: var(--side-muted); font-size: 12.5px; font-weight: 600; cursor: pointer; user-select: none; }
  .shead:hover { color: var(--side-fg); }
  .caret { width: 12px; display: inline-block; transition: transform .1s; }
  .shead.closed .caret { transform: rotate(-90deg); }
  .grow { flex: 1; }
  .seg { display: inline-flex; border: 1px solid var(--side-muted); border-radius: 5px; overflow: hidden; }
  .opt { border: 0; background: none; color: inherit; font-weight: 400; font-size: 11px; opacity: .75; padding: 0 6px; line-height: 16px; }
  .opt.on { background: var(--side-muted); color: var(--side); opacity: 1; font-weight: 600; }
  .opt:not(.on):hover { opacity: 1; }
  .ch { display: flex; align-items: center; gap: 6px; padding: 3px 10px 3px 0; margin: 1px 8px; border-radius: 6px; cursor: pointer; color: var(--side-fg); white-space: nowrap; }
  .ch:hover { background: var(--side-hover); }
  .ch.sel { background: var(--side-sel); color: var(--side-sel-fg); }
  .ch.unread .cname { font-weight: 700; color: #fff; }
  .tw { width: 14px; flex: none; text-align: center; font-size: 10px; color: var(--side-muted); border: 0; background: none; padding: 0; }
  .hash { opacity: .7; flex: none; }
  .cname { overflow: hidden; text-overflow: ellipsis; flex: 1; }
  .role { font-size: 10px; opacity: .65; flex: none; text-transform: uppercase; }
  .mbadge { background: #e5484d; color: #fff; border-radius: 9px; font-size: 11px; font-weight: 700; padding: 0 6px; min-width: 18px; text-align: center; flex: none; }
  .side-empty { padding: 10px 16px; color: var(--side-muted); font-size: 13px; }
  @media (max-width: 760px) {
    nav { position: fixed; left: 0; top: 0; bottom: 0; width: 82%; max-width: 300px; z-index: 16; transform: translateX(-100%); transition: transform .15s; }
    nav.open { transform: none; }
  }
</style>
