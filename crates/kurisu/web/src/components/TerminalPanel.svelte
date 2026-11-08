<script lang="ts">
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
      fit = new globals.FitAddon.FitAddon(); term.loadAddon(fit); term.open(host); term.onData(send);
      observer = new ResizeObserver(size); observer.observe(host); reconnect = start; start();
    })().catch((e) => { if (!stopped) error = String(e); });
    return () => { stopped = true; ws?.close(); observer?.disconnect(); term?.dispose(); };
  });
</script>

<svelte:window onkeydown={(e) => { if (e.key === "Escape") onclose(); }} />

<aside class="terminal-panel" aria-label="Terminal">
  <header><b>TERMINAL</b><button aria-label="Close terminal" onclick={onclose}>Close ×</button></header>
  <form onsubmit={(e) => { e.preventDefault(); reconnect(); }}>
    <input aria-label="Terminal directory" placeholder="Session workspace (default)" bind:value={dir} />
    <button>Open</button>
  </form>
  {#if error}<p role="status">{error}</p>{/if}
  <div class="terminal-host" bind:this={host}></div>
  <footer>{#each [["Esc", "\x1b"], ["Tab", "\t"], ["Ctrl C", "\x03"], ["↑", "\x1b[A"], ["↓", "\x1b[B"]] as [label, key]}<button onclick={() => send(key)}>{label}</button>{/each}</footer>
</aside>

<style>
  .terminal-panel { width: clamp(340px, 42vw, 900px); min-width: 0; min-height: 0; height: 100%; display: flex; flex-direction: column; overflow: hidden; background: #111; color: #eee; border-left: 1px solid #555; }
  header, form, footer { display: flex; align-items: center; gap: 8px; padding: 8px; flex: none; }
  header { justify-content: space-between; }
  button { background: #333; color: #eee; border: 1px solid #777; border-radius: 5px; padding: 7px; min-height: 44px; }
  input { min-width: 0; flex: 1; background: #222; color: #eee; border: 1px solid #777; padding: 7px; }
  p { margin: 4px 8px; font-size: 13px; }
  .terminal-host { flex: 1; min-height: 0; min-width: 0; overflow: hidden; padding: 4px; }
  footer { padding-bottom: max(8px, env(safe-area-inset-bottom)); }
  @media (max-width: 1100px) { .terminal-panel { position: fixed; inset: 0; width: 100%; height: 100dvh; z-index: 30; padding-top: env(safe-area-inset-top); } }
</style>
