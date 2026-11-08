<script lang="ts">
  import { chat } from "./state.svelte";

  let text = $state("");
  let area = $state<HTMLTextAreaElement | null>(null);

  const disabled = $derived(!chat.current || !text.trim() || chat.sending);

  async function submit(ev?: SubmitEvent, mode: "queue" | "steer" = "queue") {
    ev?.preventDefault();
    const body = text;
    if (!body.trim() || !chat.current || chat.sending) return;
    text = "";
    resize();
    const ok = await chat.send(body, mode);
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
    placeholder={chat.current ? chat.running ? "Queue a follow-up…" : "Message the session…" : "Pick a session"}
    oninput={resize}
    onkeydown={onkeydown}
  ></textarea>
  <div class="compose-actions">
  <button type="submit" disabled={disabled}>{chat.sending ? "sending…" : chat.running ? "Queue" : "Send"}</button>
  {#if chat.running}
    <button type="button" disabled={disabled} onclick={() => submit(undefined, "steer")} title="Send into the current turn when supported; otherwise queue it">Steer now</button>
    <button type="button" class="stop" onclick={stop} disabled={chat.stopping} aria-label="Stop the running turn">
      <span class="stop-icon" aria-hidden="true"></span>{chat.stopping ? "stopping…" : "Stop"}
    </button>
  {/if}
  </div>
</form>
{#if chat.running}<div class="queue-help">Queue for the next turn, or steer the current turn when supported.</div>{/if}
