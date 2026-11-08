<script lang="ts">
  import { onMount } from "svelte";
  import Composer from "./Composer.svelte";
  import Messages from "./Messages.svelte";
  import Sidebar from "./Sidebar.svelte";
  import { chat } from "./state.svelte";
  import { statusLabel } from "./types";

  import TerminalPanel from "../components/TerminalPanel.svelte";
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
    return () => chat.close();
  });
</script>

<div class="app" class:open={drawer}>
  <Sidebar terminalOpen={terminalOpen} onterminal={() => { terminalOpen = !terminalOpen; drawer = false; }} onpick={() => (drawer = false)} />
  <div class="scrim" role="presentation" onclick={() => (drawer = false)}></div>

  <main class="main">
    <header class="topbar">
      <button class="menu" aria-label="Sessions" onclick={() => (drawer = true)}>☰</button>
      <div class="titles">
        <div class="title">{chat.current?.name ?? "Kurisu"}</div>
        <div class="sub">{sub}</div>
      </div>
      {#if notice}<div class="net on">{notice}</div>{/if}
    </header>

    <Messages />

    {#if chat.error}<div class="err">{chat.error}</div>{/if}

    <Composer />
  </main>
  {#if terminalOpen}<TerminalPanel endpoint="/_web/terminal" session={chat.currentId} onclose={() => terminalOpen = false} />{/if}
</div>
