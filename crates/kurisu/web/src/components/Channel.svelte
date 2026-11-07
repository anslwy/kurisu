<script lang="ts">
  import { tick } from "svelte";
  import { app, stateOf, initials, hhmm, dayLabel } from "../lib/state.svelte";
  import { md } from "../lib/md";
  import type { AuditRow, Message } from "../lib/api";

  let feed: HTMLDivElement | undefined = $state();
  let input = $state("");
  let ta: HTMLTextAreaElement | undefined = $state();

  const s = $derived(app.current);
  const st = $derived(s ? stateOf(s) : ["idle", ""]);
  const blocked = $derived(!s || s.lifecycle !== "active");

  type Item =
    | { kind: "msg"; at: number; m: Message; pending?: { state: string; error?: string } }
    | { kind: "sys"; at: number; text: string };

  function eventText(a: AuditRow): string | null {
    const who = a.actor === "user" ? "you" : a.actor_name;
    if (a.field.startsWith("event:")) {
      const ev = a.field.slice(6);
      if (ev === "created") return `${who} created this session ${a.new || ""}`.trim();
      if (ev === "started") return `${who} started the CLI`;
      if (ev === "resumed") return "the CLI resumed";
      if (ev === "stopped") return `${who} stopped the CLI`;
      return null; // failures already show as error messages
    }
    if (a.field === "task") return `${who} set the task: ${a.new}`;
    if (a.field === "lifecycle") return `${who} ${a.new === "active" ? "reactivated" : a.new} this session`;
    if (a.field === "name") return `${who} renamed this session to #${a.new}`;
    if (a.field === "description") return `${who} set the topic: ${a.new || "(none)"}`;
    return `${who} changed ${a.field} to ${a.new || "(default)"}`;
  }

  const items = $derived.by(() => {
    const out: Item[] = [];
    for (const m of app.messages) out.push({ kind: "msg", at: m.at, m });
    for (const a of app.audit) { const t = eventText(a); if (t) out.push({ kind: "sys", at: a.at, text: t }); }
    out.sort((x, y) => x.at - y.at || (x.kind === "sys" ? -1 : 1));
    for (const p of app.pending) out.push({ kind: "msg", at: p.at, m: { seq: -p.key, session: "", role: "user", text: p.text, at: p.at }, pending: { state: p.state, error: p.error } });
    return out;
  });

  type Sender = { key: string; name: string; kind: "you" | "self" | "peer" | "error" };
  function senderOf(m: Message): Sender {
    if (m.role === "user") return { key: "you", name: "you", kind: "you" };
    if (m.role === "error") return { key: "error", name: "error", kind: "error" };
    if (m.role.startsWith("peer:")) { const n = m.role.slice(5); return { key: "peer:" + n, name: n, kind: "peer" }; }
    return { key: "self", name: s?.name ?? "agent", kind: "self" };
  }
  function color(snd: Sender) {
    if (snd.kind === "you") return "var(--you)";
    if (snd.kind === "error") return "var(--err)";
    const p = app.byName(snd.name);
    if (p?.role === "lead") return "var(--lead)";
    return snd.kind === "self" ? "var(--worker)" : "var(--peer)";
  }

  // Precompute day separators and grouping (consecutive messages from one sender within 5 min).
  const rows = $derived.by(() => {
    let lastDay = "", last: { key: string; at: number } | null = null;
    return items.map((it) => {
      const day = dayLabel(it.at);
      const sep = day !== lastDay ? day : null;
      if (sep) { lastDay = day; last = null; }
      if (it.kind === "sys") { last = null; return { it, sep, first: false, snd: null as Sender | null }; }
      const snd = senderOf(it.m);
      const first = !(last && last.key === snd.key && it.at - last.at < 300) || snd.kind === "error";
      last = { key: snd.key, at: it.at };
      return { it, sep, first, snd };
    });
  });

  let stick = true;
  function onScroll() { if (feed) stick = feed.scrollHeight - feed.scrollTop - feed.clientHeight < 160; }
  $effect(() => {
    rows.length; app.selected;
    tick().then(() => { if (feed && stick) feed.scrollTop = feed.scrollHeight; });
  });
  $effect(() => { if (s) app.markSeen(s); });
  $effect(() => { app.selected; stick = true; input = ""; if (ta && window.innerWidth > 760) ta.focus(); });

  function autosize() { if (ta) { ta.style.height = "auto"; ta.style.height = Math.min(200, ta.scrollHeight) + "px"; } }
  function submit() {
    const text = input.replace(/\s+$/, "");
    if (!text.trim() || blocked) return;
    input = ""; stick = true; tick().then(autosize);
    app.send(text);
  }
  function onKey(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); submit(); }
  }
</script>

<main>
  <div class="chead">
    <button class="hbtn menu" aria-label="Sessions" onclick={() => (app.navOpen = true)}>☰</button>
    {#if s}
      <div class="title">
        <h1><span class="nm"># {s.name}</span><span class="chip mdl">{s.model}{s.effort ? " · " + s.effort : ""}</span><span class="chip st-{st[0]}">{st[1]}</span></h1>
        <div class="topic">{s.description || (s.role === "lead" ? "Lead: talks to you and delegates to peers" : "no topic")}</div>
      </div>
      <button class="hbtn" class:on={app.detailsOpen} onclick={() => (app.detailsOpen = !app.detailsOpen)}>Details</button>
    {:else}
      <div class="title"><h1>Kurisu</h1><div class="topic">your agent team</div></div>
    {/if}
  </div>

  {#if !s}
    <div class="welcome"><div><h2>Welcome to Kurisu</h2>Pick a session on the left, or press <b>＋</b> to give the Lead a task.<br />
      <span style="font-size:12px">Every session is a real CLI on this machine. Sessions talk to each other as peers.</span></div></div>
  {:else}
    <div class="feed" bind:this={feed} onscroll={onScroll} aria-live="polite">
      <div class="intro">
        <h2># {s.name}</h2>
        <p>{s.description || "No topic yet."} Created by {s.created_by_name ? "#" + s.created_by_name : "you"}. This is the start of the #{s.name} session ({s.harness}, {s.model}).</p>
      </div>
      {#each rows as r, i (i)}
        {#if r.sep}<div class="daysep">{r.sep}</div>{/if}
        {#if r.it.kind === "sys"}
          <div class="sysline">{r.it.text} · {hhmm(r.it.at)}</div>
        {:else}
          {@const m = r.it.m}
          {@const snd = r.snd!}
          <div class="msg" class:first={r.first} class:error={snd.kind === "error"} class:pending={r.it.pending?.state === "sending"} class:failed={r.it.pending?.state === "failed"}>
            {#if r.first}
              <div class="av" style="background:{color(snd)}">{snd.kind === "error" ? "!" : initials(snd.name)}</div>
            {:else}
              <div class="gut">{hhmm(m.at)}</div>
            {/if}
            <div>
              {#if r.first}
                <div class="who">
                  <b>{snd.name}</b><span class="t">{hhmm(m.at)}</span>
                  {#if snd.kind === "peer" && app.byName(snd.name)}
                    <button class="src" title="Open #{snd.name}" onclick={() => app.open(app.byName(snd.name)!.id)}>↗ #{snd.name}</button>
                  {/if}
                  {#if r.it.pending}<span class="state">{r.it.pending.state === "failed" ? "✗ not delivered: " + r.it.pending.error : r.it.pending.state === "sent" ? "sent" : "sending…"}</span>{/if}
                </div>
              {/if}
              <div class="text">{@html md(m.text)}</div>
            </div>
          </div>
        {/if}
      {/each}
    </div>
    <div class="working">
      {#if st[0] === "busy"}<i></i><i></i><i></i>{s.name} is {s.status === "starting" ? "starting…" : "working…"}{/if}
    </div>
    <div class="composer">
      <textarea bind:this={ta} bind:value={input} rows="1" oninput={autosize} onkeydown={onKey} disabled={blocked}
        placeholder={blocked ? `#${s.name} is ${s.lifecycle}. Resume or unarchive it in Details to send.` : `Message #${s.name}`} aria-label="Message"></textarea>
      <div class="ctools"><span>Enter to send · Shift+Enter for a new line</span><button class="sendbtn" disabled={blocked || !input.trim()} onclick={submit}>Send</button></div>
    </div>
  {/if}
</main>

<style>
  main { display: flex; flex-direction: column; min-width: 0; background: var(--bg); height: 100dvh; }
  .chead { display: flex; align-items: center; gap: 10px; padding: 8px 14px; border-bottom: 1px solid var(--line); min-height: 54px; }
  .title { min-width: 0; flex: 1; }
  h1 { font-size: 16px; margin: 0; display: flex; align-items: center; gap: 8px; white-space: nowrap; }
  .nm { overflow: hidden; text-overflow: ellipsis; }
  .topic { font-size: 12.5px; color: var(--muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .chip { font-size: 11.5px; background: var(--chip); color: var(--muted); border-radius: 10px; padding: 1px 8px; font-weight: 500; flex: none; }
  .chip.st-busy { color: var(--busy); } .chip.st-error { color: var(--err); } .chip.st-paused { color: var(--warn); } .chip.st-approval { color: var(--appr); }
  .menu { display: none; }
  .feed { flex: 1; overflow-y: auto; padding: 8px 0 12px; }
  .intro { padding: 22px 20px 12px; border-bottom: 1px solid var(--line); margin-bottom: 8px; }
  .intro h2 { margin: 0 0 4px; font-size: 22px; }
  .intro p { margin: 0; color: var(--muted); }
  .daysep { display: flex; align-items: center; gap: 10px; color: var(--muted); font-size: 12px; font-weight: 600; margin: 14px 20px 6px; }
  .daysep::before, .daysep::after { content: ""; flex: 1; border-top: 1px solid var(--line); }
  .msg { display: grid; grid-template-columns: 40px 1fr; gap: 0 10px; padding: 3px 20px; }
  .msg:hover { background: var(--hover); }
  .msg.first { margin-top: 8px; padding-top: 6px; }
  .av { width: 36px; height: 36px; border-radius: 8px; display: grid; place-items: center; color: #fff; font-weight: 700; font-size: 13px; }
  .gut { font-size: 10.5px; color: transparent; text-align: right; padding-top: 3px; }
  .msg:hover .gut { color: var(--muted); }
  .who { display: flex; align-items: baseline; gap: 8px; flex-wrap: wrap; }
  .t { font-size: 11.5px; color: var(--muted); }
  .src { border: 0; background: none; padding: 0; font-size: 12px; color: var(--link); }
  .src:hover { text-decoration: underline; }
  .msg.error { background: var(--err-bg); } .msg.error .text { color: var(--err); }
  .msg.pending .text { opacity: .6; }
  .msg.failed .text, .msg.failed .state { color: var(--err); }
  .state { font-size: 11px; color: var(--muted); }
  .sysline { padding: 2px 20px 2px 70px; color: var(--sys); font-size: 12.5px; }
  .sysline::before { content: "→ "; opacity: .6; }
  .working { height: 22px; padding: 0 20px; font-size: 12.5px; color: var(--muted); }
  .working i { display: inline-block; width: 5px; height: 5px; margin-right: 2px; border-radius: 50%; background: var(--busy); animation: blink 1.2s infinite; vertical-align: middle; }
  .working i:nth-child(2) { animation-delay: .2s; } .working i:nth-child(3) { animation-delay: .4s; margin-right: 6px; }
  @keyframes blink { 0%, 80%, 100% { opacity: .25; } 40% { opacity: 1; } }
  .composer { margin: 0 16px 14px; border: 1px solid var(--line); border-radius: 8px; background: var(--bg); }
  .composer:focus-within { border-color: var(--muted); }
  textarea { width: 100%; resize: none; border: 0; outline: none; background: transparent; padding: 10px 12px 2px; max-height: 200px; min-height: 40px; line-height: 1.4; display: block; }
  .ctools { display: flex; align-items: center; justify-content: space-between; padding: 4px 8px 6px 12px; font-size: 11.5px; color: var(--muted); }
  .sendbtn { border: 0; background: var(--accent); color: var(--accent-fg); border-radius: 6px; padding: 4px 12px; font-weight: 600; }
  .sendbtn:disabled { opacity: .45; cursor: default; }
  .welcome { flex: 1; display: grid; place-items: center; text-align: center; color: var(--muted); padding: 24px; }
  .welcome h2 { color: var(--fg); margin: 0 0 6px; }
  @media (max-width: 760px) {
    .menu { display: inline-block; }
    .msg { padding: 3px 12px; } .sysline { padding-left: 62px; }
    .composer { margin: 0 8px 8px; }
    .chip.mdl { display: none; }
  }
</style>
