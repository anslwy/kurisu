<script lang="ts">
  import { clock, initials, preview } from "./format";
  import { chat } from "./state.svelte";
  import { liveState, type Session } from "./types";

  let { onpick }: { onpick?: () => void } = $props();

  function pick(s: Session) {
    if (s.id !== chat.currentId) void chat.select(s.id);
    onpick?.();
  }
</script>

<aside class="side">
  <header>
    <h1>Kurisu</h1>
    <label class="toggle">
      <input type="checkbox" bind:checked={chat.showArchived} /> archived
    </label>
  </header>
  <ul class="list">
    {#each chat.rows as s (s.id)}
      <li>
        <button
          type="button"
          class="row"
          class:on={s.id === chat.currentId}
          class:arch={s.lifecycle === "archived"}
          aria-current={s.id === chat.currentId}
          onclick={() => pick(s)}
        >
          <span class="avatar">{initials(s.name)}</span>
          <span class="who">
            <span class="top"><span class="name">{s.name}</span><span class="time">{clock(s.last?.at)}</span></span>
            <span class="preview">{preview(s)}</span>
          </span>
          <span class="dot {liveState(s)}" title={liveState(s)}></span>
        </button>
      </li>
    {/each}
  </ul>
  {#if !chat.rows.length}
    <div class="empty">No sessions yet.</div>
  {/if}
</aside>
