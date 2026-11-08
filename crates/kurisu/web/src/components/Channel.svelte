<script lang="ts">
  import { tick } from "svelte";
  import { app, stateOf, initials, hhmm, dayLabel } from "../lib/state.svelte";
  import { md } from "../lib/md";
  import type { Approval, AuditRow, Message } from "../lib/api";

  let feed: HTMLDivElement | undefined = $state();
  let input = $state("");
  let ta: HTMLTextAreaElement | undefined = $state();

  const s = $derived(app.current);
  const st = $derived(s ? stateOf(s) : ["idle", ""]);
  const blocked = $derived(!s || s.lifecycle !== "active");

  type Item =
    | { kind: "msg"; at: number; m: Message; pending?: { state: string; error?: string } }
    | { kind: "sys"; at: number; text: string }
    | { kind: "appr"; at: number; id: string; title: string };

  // Approval cards: "[approval:<id>] <title>" messages, with live state from the store.
  const apprRe = /^\[approval:([0-9a-f]+)\] ?(.*)$/s;
  function approvalOf(id: string): Approval | undefined {
    return app.approvals.find((a) => a.id === id) ?? app.approvalLog[id];
  }
  let clock = $state(Date.now() / 1000);
  $effect(() => { const t = setInterval(() => (clock = Date.now() / 1000), 5000); return () => clearInterval(t); });
  function left(a: Approval) { const d = Math.max(0, a.expires_at - clock); return d >= 90 ? `${Math.round(d / 60)} min` : `${Math.round(d)} s`; }
  function detail(a: Approval | undefined, title: string) {
    const i = a?.input ?? {};
    if (typeof i.command === "string") return i.command;
    if (typeof i.file_path === "string") return i.file_path + (typeof i.content === "string" ? `\n(${i.content.length} characters)` : "");
    if (a && i && Object.keys(i).length) return JSON.stringify(i, null, 1).slice(0, 600);
    return title;
  }
  function verdict(a: Approval) {
    const by = a.decided_by === "user" ? "you" : a.decided_by?.startsWith("rule#") ? "rule #" + a.decided_by.slice(5) : a.decided_by ?? "";
    if (a.status === "approved") return `Allowed by ${by}`;
    if (a.status === "denied") return `Denied by ${by}${a.reason ? ": " + a.reason : ""}`;
    if (a.status === "expired") return "Expired: nobody answered in time, so it was denied";
    return `Cancelled${a.reason ? " (" + a.reason + ")" : ""}`;
  }
  let busyAppr = $state("");
  // Question cards: picked labels per question index.
  let picks: Record<string, Record<number, string[]>> = $state({});
  function pick(a: Approval, qi: number, label: string) {
    const q = a.questions![qi];
    const cur = picks[a.id]?.[qi] ?? [];
    const next = q.multiSelect ? (cur.includes(label) ? cur.filter((x) => x !== label) : [...cur, label]) : [label];
    picks[a.id] = { ...(picks[a.id] ?? {}), [qi]: next };
    if (a.questions!.length === 1 && !q.multiSelect) sendAnswers(a);
  }
  async function sendAnswers(a: Approval) {
    const answers: Record<string, string[]> = {};
    a.questions!.forEach((_, i) => (answers[String(i)] = picks[a.id]?.[i] ?? []));
    busyAppr = a.id; await app.answer(a.id, answers); busyAppr = "";
  }
  async function answer(id: string, allow: boolean, always = false) {
    busyAppr = id;
    if (allow) await app.approve(id, always); else await app.deny(id);
    busyAppr = "";
  }

  function eventText(a: AuditRow): string | null {
    const who = a.actor === "user" ? "you" : a.actor_name;
    if (a.field.startsWith("event:")) {
      const ev = a.field.slice(6);
      if (ev === "created") return `${who} created this session ${a.new || ""}`.trim();
      if (ev === "started") return `${who} started the CLI`;
      if (ev === "resumed") return "the CLI resumed";
      if (ev === "stopped") return `${who} stopped the CLI`;
      if ((ev === "approval_approved" || ev === "approval_denied") && a.actor.startsWith("rule#"))
        return `rule #${a.actor.slice(5)} ${ev === "approval_approved" ? "allowed" : "denied"} ${(a.new || "").replace(/^[0-9a-f]+ /, "")}`;
      if (ev === "rule_added") return `${who} added approval rule ${a.new || ""}`;
      if (ev === "rule_removed") return `${who} removed approval rule ${a.new || ""}`;
      return null; // failures show as error messages; approvals as cards
    }
    if (a.field === "task") return `${who} set the task: ${a.new}`;
    if (a.field === "lifecycle") return `${who} ${a.new === "active" ? "reactivated" : a.new} this session`;
    if (a.field === "name") return `${who} renamed this session to #${a.new}`;
    if (a.field === "description") return `${who} set the topic: ${a.new || "(none)"}`;
    return `${who} changed ${a.field} to ${a.new || "(default)"}`;
  }

  const items = $derived.by(() => {
    const out: Item[] = [];
    for (const m of app.messages) {
      const am = m.role === "approval" ? apprRe.exec(m.text) : null;
      if (am) out.push({ kind: "appr", at: m.at, id: am[1], title: am[2] });
      else out.push({ kind: "msg", at: m.at, m });
    }
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
      if (it.kind !== "msg") { last = null; return { it, sep, first: false, snd: null as Sender | null }; }
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
        <p>{s.description ? s.description.replace(/\s+$/, "") + (/[.!?]$/.test(s.description.trim()) ? "" : ".") : "No topic yet."}
          Created by {s.created_by_name ? "#" + s.created_by_name : "you"}. This is the start of the #{s.name} session ({s.harness}, {s.model}).</p>
      </div>
      {#if app.loadError && !app.loaded}
        <div class="loaderr">Could not load the history ({app.loadError}). <button class="src" onclick={() => app.loadChannel(s.id)}>Retry</button></div>
      {/if}
      {#each rows as r, i (i)}
        {#if r.sep}<div class="daysep">{r.sep}</div>{/if}
        {#if r.it.kind === "sys"}
          <div class="sysline">{r.it.text} · {hhmm(r.it.at)}</div>
        {:else if r.it.kind === "appr"}
          {@const a = approvalOf(r.it.id)}
          {@const open = a?.status === "pending"}
          <div class="appr" class:open role="group" aria-label="Permission request">
            <div class="ahead">{#if a?.kind === "question"}<span class="tag">Question</span><b>#{s.name}</b> asks you{:else}<span class="tag">Permission</span><b>#{s.name}</b> wants to use <b>{a?.tool ?? r.it.title.split(":")[0]}</b>{/if}
              <span class="t">{hhmm(r.it.at)} · {r.it.id}</span></div>
            {#if a?.kind === "question" && a.questions}
              {#each a.questions as q, qi (qi)}
                <div class="qq"><b>{q.question}</b>{q.multiSelect ? " (pick any)" : ""}</div>
                {#if open}
                  <div class="abtns">
                    {#each q.options as o (o.label)}
                      <button class="btn ghost" class:on={picks[a.id]?.[qi]?.includes(o.label)} title={o.description ?? ""}
                        disabled={busyAppr === a.id} onclick={() => pick(a, qi, o.label)}>{o.label}</button>
                    {/each}
                  </div>
                {/if}
              {/each}
              {#if open}
                <div class="abtns">
                  {#if a.questions.length > 1 || a.questions.some((q) => q.multiSelect)}<button class="btn" disabled={busyAppr === a.id} onclick={() => sendAnswers(a)}>Send answers</button>{/if}
                  <button class="btn danger" disabled={busyAppr === a.id} onclick={() => answer(a.id, false)}>Dismiss</button>
                  <span class="exp">expires in {left(a)}</span>
                </div>
              {:else}
                <div class="averdict v-{a.status}">{a.answer ? "Answered: " + Object.values(a.answer).join("; ") : verdict(a)}</div>
              {/if}
            {:else}
            <pre>{detail(a, r.it.title)}</pre>
            {#if a?.cwd}<div class="acwd">in {a.cwd}</div>{/if}
            {#if open && a}
              <div class="abtns">
                <button class="btn" disabled={busyAppr === a.id} onclick={() => answer(a.id, true)}>Allow once</button>
                <button class="btn ghost" disabled={busyAppr === a.id} onclick={() => answer(a.id, true, true)}
                  title="Allow now, and add a rule: {a.tool} is allowed for #{s.name} inside its folder">Always allow in its folder</button>
                <button class="btn danger" disabled={busyAppr === a.id} onclick={() => answer(a.id, false)}>Deny</button>
                <span class="exp">expires in {left(a)}</span>
              </div>
            {:else if a}
              <div class="averdict v-{a.status}">{verdict(a)}{a.decided_at ? " · " + hhmm(a.decided_at) : ""}</div>
            {/if}
            {/if}
          </div>
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
      {#if st[0] === "busy"}<i></i><i></i><i></i>{s.name} is {s.status === "starting" ? "starting…" : "working…"}
      {:else if st[0] === "approval"}<b class="wa">{s.name} is waiting for your approval</b>{/if}
    </div>
    <div class="composer">
      <textarea bind:this={ta} bind:value={input} rows="1" oninput={autosize} onkeydown={onKey} disabled={blocked}
        placeholder={blocked ? `#${s.name} is ${s.lifecycle}. Resume or unarchive it in Details to send.` : `Message #${s.name}`} aria-label="Message"></textarea>
      <div class="ctools"><span>Enter to send · Shift+Enter for a new line</span><button class="sendbtn" disabled={blocked || !input.trim()} onclick={submit}>Send</button></div>
    </div>
  {/if}
</main>

<style>
  main { display: flex; flex-direction: column; min-width: 0; background: var(--bg); height: 100dvh; min-height: 0; overflow: hidden; }
  .chead { flex-shrink: 0; user-select: none; display: flex; align-items: center; gap: 10px; padding: 8px 14px; border-bottom: 1px solid var(--line); min-height: 54px; }
  .title { min-width: 0; flex: 1; }
  h1 { font-size: 16px; margin: 0; display: flex; align-items: center; gap: 8px; white-space: nowrap; }
  .nm { overflow: hidden; text-overflow: ellipsis; }
  .topic { font-size: 12.5px; color: var(--muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .chip { font-size: 11.5px; background: var(--chip); color: var(--muted); border-radius: 10px; padding: 1px 8px; font-weight: 500; flex: none; }
  .chip.st-busy { color: var(--busy); } .chip.st-error { color: var(--err); } .chip.st-paused { color: var(--warn); } .chip.st-approval { color: var(--appr); }
  .menu { display: none; }
  .feed { flex: 1; min-height: 0; overscroll-behavior: contain; overflow-y: auto; padding: 8px 0 12px; }
  .intro { overflow-wrap: anywhere; padding: 22px 20px 12px; border-bottom: 1px solid var(--line); margin-bottom: 8px; }
  .intro h2 { margin: 0 0 4px; font-size: 22px; }
  .intro p { margin: 0; color: var(--muted); }
  .daysep { display: flex; align-items: center; gap: 10px; color: var(--muted); font-size: 12px; font-weight: 600; margin: 14px 20px 6px; }
  .daysep::before, .daysep::after { content: ""; flex: 1; border-top: 1px solid var(--line); }
  .msg { display: grid; grid-template-columns: 40px minmax(0, 1fr); gap: 0 10px; padding: 3px 20px; }
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
  .sysline { overflow-wrap: anywhere; padding: 2px 20px 2px 70px; color: var(--sys); font-size: 12.5px; }
  .sysline::before { content: "→ "; opacity: .6; }
  .working { height: 22px; padding: 0 20px; font-size: 12.5px; color: var(--muted); }
  .working i { display: inline-block; width: 5px; height: 5px; margin-right: 2px; border-radius: 50%; background: var(--busy); animation: blink 1.2s infinite; vertical-align: middle; }
  .working i:nth-child(2) { animation-delay: .2s; } .working i:nth-child(3) { animation-delay: .4s; margin-right: 6px; }
  @keyframes blink { 0%, 80%, 100% { opacity: .25; } 40% { opacity: 1; } }
  .composer { flex-shrink: 0; margin: 0 16px 14px; border: 1px solid var(--line); border-radius: 8px; background: var(--bg); }
  .composer:focus-within { border-color: var(--muted); }
  textarea { width: 100%; resize: none; border: 0; outline: none; background: transparent; padding: 10px 12px 2px; max-height: 200px; min-height: 40px; line-height: 1.4; display: block; }
  .ctools { display: flex; align-items: center; justify-content: space-between; padding: 4px 8px 6px 12px; font-size: 11.5px; color: var(--muted); }
  .sendbtn { border: 0; background: var(--accent); color: var(--accent-fg); border-radius: 6px; padding: 4px 12px; font-weight: 600; }
  .sendbtn:disabled { opacity: .45; cursor: default; }
  .loaderr { margin: 8px 20px; padding: 8px 12px; border-radius: 6px; background: var(--err-bg); color: var(--err); font-size: 13px; }
  .appr { margin: 10px 20px 6px 70px; border: 1px solid var(--line); border-left: 4px solid var(--muted); border-radius: 8px; padding: 10px 12px; background: var(--bg2); max-width: 720px; }
  .appr.open { border-left-color: var(--appr); }
  .ahead { display: flex; gap: 6px; align-items: baseline; flex-wrap: wrap; font-size: 13.5px; }
  .tag { font-size: 10.5px; text-transform: uppercase; letter-spacing: .05em; font-weight: 700; color: var(--appr); }
  .appr pre { margin: 6px 0 2px; padding: 6px 8px; background: var(--chip); border-radius: 6px; white-space: pre-wrap; overflow-wrap: anywhere; font-size: 12.5px; max-height: 220px; overflow: auto; }
  .acwd { font-size: 11.5px; color: var(--muted); }
  .abtns { display: flex; gap: 6px; align-items: center; flex-wrap: wrap; margin-top: 8px; }
  .exp { font-size: 11.5px; color: var(--muted); margin-left: 4px; }
  .averdict { margin-top: 6px; font-size: 12.5px; color: var(--muted); }
  .averdict.v-approved { color: var(--ok); } .averdict.v-denied, .averdict.v-expired { color: var(--err); }
  .qq { margin-top: 6px; font-size: 13.5px; }
  .btn.ghost.on { background: var(--accent); color: var(--accent-fg); border-color: var(--accent); }
  .wa { color: var(--appr); font-weight: 600; }
  .welcome { flex: 1; display: grid; place-items: center; text-align: center; color: var(--muted); padding: 24px; }
  .welcome h2 { color: var(--fg); margin: 0 0 6px; }
  @media (max-width: 760px) {
    .menu { display: inline-block; }
    .msg { padding: 3px 12px; } .sysline { padding-left: 62px; }
    .appr { margin: 10px 12px 6px; }
    .composer { margin: 0 8px 8px; }
    .chip.mdl { display: none; }
  }
</style>
