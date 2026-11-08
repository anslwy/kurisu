<script lang="ts">
  import ToolRow from "./ToolRow.svelte";
  import { clock } from "./format";
  import { bubbleKind, type Delivery, type Message, type Session } from "./types";

  let {
    message,
    sessions,
    selfName = "",
    streaming = false,
    delivery,
  }: { message: Message; sessions: Session[]; selfName?: string; streaming?: boolean; delivery?: Delivery } = $props();

  const kind = $derived(bubbleKind(message.role));
  const peer = $derived(kind === "peer" ? sessions.find((s) => s.id === message.role)?.name ?? "peer" : "");
  const label = $derived(
    kind === "user" ? "You"
      : kind === "peer" ? peer
      : kind === "agent" ? selfName
      : kind === "approval" ? "approval"
      : kind === "error" ? "error"
      : "",
  );
  const tool = $derived(message.data ?? null);
</script>

{#if kind === "tool"}
  <ToolRow tool={tool ?? {}} text={message.text} />
{:else if kind === "compaction"}
  <div class="b compaction">{message.text}</div>
{:else}
  <div class="b {kind}" class:streaming>
    {#if label}<div class="meta">{label}</div>{/if}
    <div class="text">{message.text}{#if streaming}<span class="caret" aria-hidden="true"></span>{/if}</div>
    <div class="stamp">{clock(message.at)}{#if delivery} · <span aria-live="polite">{delivery}</span>{/if}</div>
  </div>
{/if}
