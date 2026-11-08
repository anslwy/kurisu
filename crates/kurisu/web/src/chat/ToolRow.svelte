<script lang="ts">
  import { compact, oneLine } from "./format";
  import type { LiveTool, ToolData } from "./types";

  type ToolView = Partial<Pick<LiveTool, "tool" | "title" | "status" | "input" | "output" | "tool_call_id">> &
    Partial<ToolData>;

  let { tool, text = "" }: { tool: ToolView; text?: string } = $props();

  const name = $derived(tool.tool || text || "tool");
  const arg = $derived(oneLine(tool.input ?? tool.title ?? "", 88));
  const input = $derived(compact(tool.input, 8000));
  const output = $derived(compact(tool.output, 8000));
</script>

<details class="toolrow">
  <summary>
    <span class="tname">{name}</span>
    {#if arg}<span class="targ">{arg}</span>{/if}
    <span class="tst">{tool.status ?? ""}</span>
  </summary>
  <div class="tbody">
    {#if input}<pre class="tin">{input}</pre>{/if}
    {#if output}<pre class="tout">{output}</pre>{/if}
  </div>
</details>
