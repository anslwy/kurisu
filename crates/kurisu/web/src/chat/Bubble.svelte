<script lang="ts">
  import { compact } from "./format";
  import { bubbleKind, type Message, type Session } from "./types";

  let { message, sessions, selfName = "" }: { message: Message; sessions: Session[]; selfName?: string } = $props();

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
  const body = $derived(tool ? compact(tool.input ?? "") : "");
  const out = $derived(tool ? compact(tool.output ?? "") : "");
</script>

{#if kind === "tool"}
  <div class="b tool">
    <div class="head"><span>{tool?.tool ?? "tool"}</span><span class="st">{tool?.status ?? ""}</span></div>
    {#if message.text && !tool?.tool}<span>{message.text}</span>{/if}
    {#if body}<pre>{body}</pre>{/if}
    {#if out}<pre class="out">{out}</pre>{/if}
  </div>
{:else if kind === "compaction"}
  <div class="b compaction">{message.text}</div>
{:else}
  <div class="b {kind}">
    <div class="meta">{label} · {new Date(message.at * 1000).toTimeString().slice(0, 5)}</div>
    <div class="text">{message.text}</div>
  </div>
{/if}
