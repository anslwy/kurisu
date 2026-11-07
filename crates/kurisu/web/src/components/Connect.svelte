<script lang="ts">
  import { app, storage } from "../lib/state.svelte";
  let base = $state(storage.get("kurisu.base") || "");
  let token = $state("");
  let showAdvanced = $state(!!app.authError);
  let busy = $state(false);
  async function go() {
    busy = true;
    await app.connect(base.trim(), token.trim());
    busy = false;
  }
</script>

<div class="wrap">
  <div class="card">
    <h2>Open the URL printed by <code>ku web</code></h2>
    <p class="muted">This page needs the access token from that URL (the part after <code>#token=</code>).
      Run <code>ku web</code> in a terminal and open the exact link it prints.</p>
    {#if app.authError}<p class="err">{app.authError.includes("token") ? "The saved or provided token is wrong." : app.authError}</p>{/if}
    {#if !showAdvanced}
      <button class="link" onclick={() => (showAdvanced = true)}>Connect manually (remote server or app)</button>
    {:else}
      <form class="form" onsubmit={(e) => { e.preventDefault(); go(); }}>
        <label>Server URL <input bind:value={base} placeholder="(this server) or https://kurisu.example.ts.net" /></label>
        <label>Token <input bind:value={token} placeholder="token printed by ku web" autocomplete="off" /></label>
        <button class="btn" disabled={busy || !token.trim()}>Connect</button>
      </form>
    {/if}
  </div>
</div>

<style>
  .wrap { height: 100%; display: grid; place-items: start center; padding: 16vh 16px 16px; }
  .card { max-width: 520px; width: 100%; padding: 28px; border: 1px solid var(--line); border-radius: 12px; text-align: center; }
  h2 { margin: 0 0 8px; }
  .err { color: var(--err); font-size: 13px; }
  .link { border: 0; background: none; color: var(--link); text-decoration: underline; font-size: 13px; }
  .form { display: flex; flex-direction: column; gap: 10px; text-align: left; margin-top: 12px; }
  .form label { font-size: 12px; color: var(--muted); display: flex; flex-direction: column; gap: 3px; }
  .form input { border: 1px solid var(--line); background: var(--bg2); border-radius: 6px; padding: 7px 10px; outline: none; }
</style>
