<script lang="ts">
  import { terminalTheme } from "../lib/terminal-theme";
  import { onMount } from "svelte";
  import xtermUrl from "../../../terminal/xterm-5.5.0.js?url&no-inline";
  import fitUrl from "../../../terminal/addon-fit-0.10.0.js?url&no-inline";
  import "../../../terminal/xterm-5.5.0.css";

  let { endpoint, token, session, onclose }: { endpoint: string; token?: string; session?: string | null; onclose: () => void } = $props();
  let host: HTMLDivElement;
  let dir = $state("");
  let error = $state("");
  let reconnect: () => void = () => {};
  let send: (s: string) => void = () => {};
  const load = (src: string) => new Promise<void>((resolve, reject) => {
    const script = document.createElement("script"); script.src = src;
    script.onload = () => { script.remove(); resolve(); }; script.onerror = () => { script.remove(); reject(new Error("Unable to load terminal")); };
    document.head.append(script);
  });
  onMount(() => {
    let stopped = false, ws: WebSocket | undefined, term: any, fit: any, observer: ResizeObserver;
    const scheme = matchMedia("(prefers-color-scheme: dark)");
    const applyTheme = () => {
      if (term) { const css = getComputedStyle(host); term.options.theme = terminalTheme((name) => css.getPropertyValue(name).trim()); }
    };
    scheme.addEventListener("change", applyTheme);
    const size = () => { fit?.fit(); if (ws?.readyState === WebSocket.OPEN) ws.send("\0" + JSON.stringify({ cols: term.cols, rows: term.rows })); };
    const start = () => {
      ws?.close(); error = ""; term.reset();
      const url = new URL(endpoint, location.href); url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
      if (session) url.searchParams.set("session", session);
      if (dir.trim()) url.searchParams.set("dir", dir.trim());
      const conn = new WebSocket(url, token ? ["kurisu", token] : []); ws = conn; conn.binaryType = "arraybuffer";
      conn.onopen = () => { size(); term.focus(); };
      conn.onmessage = (e) => term.write(typeof e.data === "string" ? e.data : new Uint8Array(e.data));
      conn.onclose = () => { if (!stopped && ws === conn) error = "Terminal closed. Reopen to reconnect; check your session or close another attached tab."; };
      conn.onerror = () => { if (!stopped && ws === conn) error = "Unable to open terminal. Check your authenticated session and directory."; };
    };
    send = (s) => { if (ws?.readyState === WebSocket.OPEN) ws.send(s); term?.focus(); };
    void (async () => {
      await load(xtermUrl); await load(fitUrl); if (stopped) return;
      const globals = window as any;
      term = new globals.Terminal({ fontSize: 14, cursorBlink: true, scrollback: 5000 });
      applyTheme();
      fit = new globals.FitAddon.FitAddon(); term.loadAddon(fit); term.open(host); term.onData(send);
      observer = new ResizeObserver(size); observer.observe(host); reconnect = start; start();
    })().catch((e) => { if (!stopped) error = String(e); });
    return () => { stopped = true; ws?.close(); observer?.disconnect(); scheme.removeEventListener("change", applyTheme); term?.dispose(); };
  });
</script>

<svelte:window onkeydown={(e) => { if (e.key === "Escape") onclose(); }} />

<aside class="terminal-panel themed-surface" aria-label="Terminal">
  <header><b>TERMINAL</b><button class="icon-button" title="Close terminal" aria-label="Close terminal" onclick={onclose}>
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true"><path d="m6 6 12 12M18 6 6 18" /></svg>
  </button></header>
  <form onsubmit={(e) => { e.preventDefault(); reconnect(); }}>
    <input aria-label="Terminal directory" placeholder="Session workspace (default)" bind:value={dir} />
    <button class="primary">Open</button>
  </form>
  {#if error}<p role="status">{error}</p>{/if}
  <div class="terminal-host" bind:this={host}></div>
  <footer>{#each [["Esc", "\x1b"], ["Tab", "\t"], ["Ctrl C", "\x03"], ["↑", "\x1b[A"], ["↓", "\x1b[B"]] as [label, key]}<button onclick={() => send(key)}>{label}</button>{/each}</footer>
</aside>

<style>
  .terminal-panel { width: clamp(340px, 42vw, 900px); min-width: 0; min-height: 0; height: 100%; display: flex; flex-direction: column; overflow: hidden; border: 1px solid var(--line); border-radius: 18px; }
  .themed-surface { background: var(--bg2); color: var(--fg); color-scheme: light dark; }
  header, form, footer { display: flex; align-items: center; gap: 8px; padding: 8px; flex: none; }
  header { justify-content: space-between; height: var(--chat-header-height); border-bottom: 1px solid var(--line); padding: 6px 12px; }
  header b { font-size: 12.5px; font-weight: 600; letter-spacing: .06em; color: var(--muted); }
  button { background: var(--bg); color: var(--fg); border: 1px solid var(--line); border-radius: 6px; padding: 7px 12px; min-height: 36px; cursor: pointer; }
  button:hover { background: var(--hover); }
  .icon-button { display: grid; place-items: center; width: 36px; padding: 0; border: 0; background: transparent; color: var(--muted); }
  .icon-button:hover { color: var(--fg); }
  .primary { background: var(--accent); border-color: var(--accent); color: var(--accent-fg); font-weight: 600; }
  .primary:hover { background: var(--accent); filter: brightness(1.1); }
  input { min-width: 0; flex: 1; background: var(--bg2); color: var(--fg); border: 1px solid var(--line); border-radius: 6px; padding: 6px 9px; outline: none; }
  input::placeholder { color: var(--muted); }
  input:focus, button:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
  p { margin: 4px 8px; font-size: 13px; color: var(--err); }
  .terminal-host { flex: 1; min-height: 0; min-width: 0; overflow: hidden; padding: 8px; background: var(--bg2); }
  .terminal-host :global(.xterm-viewport) { scrollbar-color: var(--muted) var(--bg2); scrollbar-width: thin; overscroll-behavior: contain; }
  .terminal-host :global(.xterm-viewport::-webkit-scrollbar) { width: 8px; }
  .terminal-host :global(.xterm-viewport::-webkit-scrollbar-track) { background: var(--bg2); }
  .terminal-host :global(.xterm-viewport::-webkit-scrollbar-thumb) { background: var(--muted); border: 2px solid var(--bg2); border-radius: 6px; }
  footer { border-top: 1px solid var(--line); padding-bottom: max(8px, env(safe-area-inset-bottom)); }
  /* Leave the original chat header (and its single toggle) above the overlay. */
  @media (max-width: 1100px) { .terminal-panel { position: fixed; inset: calc(var(--safe-top) + var(--chat-header-height)) 0 0; width: 100%; height: calc(100dvh - var(--safe-top) - var(--chat-header-height)); z-index: 30; } }
</style>
