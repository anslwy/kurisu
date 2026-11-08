<script lang="ts">
  import { tick } from "svelte";
  import Bubble from "./Bubble.svelte";
  import { chat } from "./state.svelte";

  let box = $state<HTMLDivElement | null>(null);
  let atBottom = $state(true);

  // A different session starts at its newest row, not wherever the last one was.
  $effect(() => {
    void chat.currentId;
    atBottom = true;
    void tick().then(() => {
      const el = box;
      if (el) el.scrollTop = el.scrollHeight;
    });
  });

  // Follow new rows (and a pending bubble) only when the reader is at the bottom.
  $effect(() => {
    void chat.messages.length;
    void chat.pending?.text;
    void tick().then(() => {
      const el = box;
      if (el && atBottom) el.scrollTop = el.scrollHeight;
    });
  });

  async function earlier() {
    const el = box;
    const before = el ? el.scrollHeight : 0;
    const added = await chat.earlier();
    if (added && el) {
      await tick();
      el.scrollTop = el.scrollHeight - before;
    }
  }

  export function toBottom() {
    const el = box;
    if (el) el.scrollTop = el.scrollHeight;
  }
</script>

<div
  class="msgs"
  bind:this={box}
  onscroll={() => {
    const el = box;
    if (el) atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 120;
  }}
>
  {#if chat.hasMore}
    <button class="more" onclick={earlier}>↑ earlier messages</button>
  {/if}
  {#each chat.messages as m (m.seq)}
    <Bubble message={m} sessions={chat.sessions} selfName={chat.current?.name ?? ""} />
  {/each}
  {#if chat.pending}
    <div class="b user pending">{chat.pending.text}</div>
  {/if}
  {#if !chat.messages.length && !chat.pending && !chat.loading}
    <div class="sys">No messages yet.</div>
  {/if}
</div>
