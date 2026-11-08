<script lang="ts">
  import { onMount } from "svelte";
  import BuildBanner from "./BuildBanner.svelte";
  import { checkBuild } from "./build-warning";
  import Composer from "./Composer.svelte";
  import Messages from "./Messages.svelte";
  import Sidebar from "./Sidebar.svelte";
  import { chat } from "./state.svelte";
  import { statusLabel } from "./types";

  import TerminalToggle from "../components/TerminalToggle.svelte";
  import TerminalPanel from "../components/TerminalPanel.svelte";
  let buildWarning = $state("");
  let terminalOpen = $state(false);
  let drawer = $state(false);

  const sub = $derived(
    chat.current
      ? [chat.current.role, chat.current.harness, chat.current.model, statusLabel(chat.current)]
          .filter(Boolean)
          .join(" · ")
      : "",
  );

  const notice = $derived(
    chat.connection === "closed"
      ? "This page was closed. Send /web in Telegram for a new link."
      : chat.connection === "reconnecting"
        ? "Reconnecting…"
        : "",
  );

  onMount(() => {
    void chat.boot();
    const bundle = document.querySelector<HTMLMetaElement>('meta[name="ku-bundle"]')?.content;
    const sha = document.querySelector<HTMLMetaElement>('meta[name="ku-git-sha"]')?.content;
    let stopped = false;
    const check = async () => {
      if (bundle && sha) {
        const warning = await checkBuild(bundle, sha);
        if (!stopped) buildWarning = warning;
      }
    };
    void check();
    const timer = setInterval(() => void check(), 30000);
    window.addEventListener("focus", check);
    return () => { stopped = true; clearInterval(timer); window.removeEventListener("focus", check); chat.close(); };
  });
</script>

<div class="app" class:open={drawer}>
  <Sidebar onpick={() => (drawer = false)} />
  <div class="scrim" role="presentation" onclick={() => (drawer = false)}></div>

  <main class="main">
    <header class="topbar">
      <button class="menu" aria-label="Sessions" onclick={() => (drawer = true)}>☰</button>
      <div class="titles">
        <div class="title">{chat.current?.name ?? "Kurisu"}</div>
        <div class="sub">{sub}</div>
      </div>
      {#if notice}<div class="net on">{notice}</div>{/if}
      <TerminalToggle open={terminalOpen} ontoggle={() => { terminalOpen = !terminalOpen; drawer = false; }} />
    </header>

    <BuildBanner warning={buildWarning} />
    <Messages />

    {#if chat.error}<div class="err">{chat.error}</div>{/if}

    <Composer />
  </main>
  {#if terminalOpen}<TerminalPanel endpoint="/_web/terminal" session={chat.currentId} onclose={() => terminalOpen = false} />{/if}
</div>
