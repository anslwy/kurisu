<script lang="ts">
  import { onMount } from "svelte";
  import { app, storage } from "./lib/state.svelte";
  import Connect from "./components/Connect.svelte";
  import Sidebar from "./components/Sidebar.svelte";
  import Channel from "./components/Channel.svelte";
  import Details from "./components/Details.svelte";
  import AddModal from "./components/AddModal.svelte";
  import Switcher from "./components/Switcher.svelte";

  let booting = $state(true);

  onMount(async () => {
    const h = new URLSearchParams(location.hash.slice(1));
    const token = h.get("token") || storage.get("kurisu.token") || "";
    const base = h.get("server") || storage.get("kurisu.base") || "";
    if (h.get("token")) {
      // Keep the token out of the address bar and history once it is stored.
      h.delete("token");
      history.replaceState(null, "", location.pathname + (h.toString() ? "#" + h.toString() : ""));
    }
    if (token) await app.connect(base, token);
    booting = false;
    if (app.api) {
      app.sessions = await app.api.sessions().catch(() => []);
      const want = h.get("chat");
      const s = want && app.sessions.find((x) => x.id === want || x.name === want || x.id.startsWith(want));
      if (s) { await app.open(s.id); if (h.get("info")) app.detailsOpen = true; }
      if (h.get("switcher")) app.switcherOpen = true;
    }
    // Back/forward and edited URLs: open the channel named in #chat=.
    window.addEventListener("hashchange", () => {
      const want = new URLSearchParams(location.hash.slice(1)).get("chat");
      const s = want && app.sessions.find((x) => x.id === want || x.name === want);
      if (s && s.id !== app.selected) app.open(s.id);
    });
  });

  function onKey(e: KeyboardEvent) {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") { e.preventDefault(); app.switcherOpen = true; }
    if (e.key === "Escape") { app.switcherOpen = false; app.addOpen = false; app.navOpen = false; if (window.innerWidth <= 1100) app.detailsOpen = false; }
  }
</script>

<svelte:window onkeydown={onKey} />

{#if booting}
  <div class="boot muted">Connecting…</div>
{:else if !app.api}
  <Connect />
{:else}
  <div class="layout" class:navopen={app.navOpen} class:withdetails={app.detailsOpen && !!app.current}>
    <Sidebar />
    <button class="scrim" aria-label="Close menu" onclick={() => (app.navOpen = false)}></button>
    <Channel />
    {#if app.detailsOpen && app.current}<Details />{/if}
  </div>
  {#if app.addOpen}<AddModal />{/if}
  {#if app.switcherOpen}<Switcher />{/if}
{/if}

<style>
  .boot { display: grid; place-items: center; height: 100%; }
  .layout { display: grid; grid-template-columns: 260px minmax(0, 1fr); height: 100dvh; overflow: hidden; }
  .layout.withdetails { grid-template-columns: 260px minmax(0, 1fr) 340px; }
  .scrim { display: none; }
  @media (max-width: 1100px) { .layout.withdetails { grid-template-columns: 260px minmax(0, 1fr); } }
  @media (max-width: 760px) {
    .layout, .layout.withdetails { grid-template-columns: minmax(0, 1fr); }
    .layout.navopen .scrim { display: block; position: fixed; inset: 0; background: rgba(0,0,0,.4); z-index: 14; border: 0; }
  }
</style>
