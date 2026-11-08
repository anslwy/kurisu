<script lang="ts">
  import { chat } from "./state.svelte";

  let text = $state("");
  let area = $state<HTMLTextAreaElement | null>(null);

  const disabled = $derived(!chat.current || !text.trim() || chat.sending);

  async function submit(ev?: SubmitEvent) {
    ev?.preventDefault();
    const body = text;
    if (!body.trim() || !chat.current || chat.sending) return;
    text = "";
    resize();
    const ok = await chat.send(body);
    if (!ok) text = body; // give the text back when the send failed
    area?.focus();
  }

  async function stop() {
    if (chat.stopping) return;
    await chat.stop();
    area?.focus();
  }

  function resize() {
    if (!area) return;
    area.style.height = "auto";
    area.style.height = `${Math.min(area.scrollHeight, 140)}px`;
  }

  function onkeydown(ev: KeyboardEvent) {
    if (ev.key === "Enter" && !ev.shiftKey && !ev.isComposing) {
      ev.preventDefault();
      void submit();
    }
  }
</script>

<form class="composer" onsubmit={submit}>
  <textarea
    bind:this={area}
    bind:value={text}
    rows="1"
    placeholder={chat.current ? "Message the session…" : "Pick a session"}
    oninput={resize}
    onkeydown={onkeydown}
  ></textarea>
  {#if chat.running}
    <button type="button" class="stop" onclick={stop} disabled={chat.stopping} aria-label="Stop the running turn">
      <span class="stop-icon" aria-hidden="true"></span>{chat.stopping ? "stopping…" : "Stop"}
    </button>
  {:else}
    <button type="submit" disabled={disabled}>{chat.sending ? "sending…" : "Send"}</button>
  {/if}
</form>
