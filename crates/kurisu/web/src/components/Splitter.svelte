<script lang="ts">
  import { onMount } from "svelte";
  import {
    CHAT_MIN, SPLITTER_WIDTH, TERMINAL_DEFAULT, TERMINAL_MIN,
    clampTerminalWidth, setPref, terminalMax, terminalSpace,
  } from "../lib/layout";

  // The divider between the chat and the docked terminal. Drag it (pointer or
  // arrow keys) to size the terminal, double-click to go back to the default;
  // the width is stored under storageKey. `reserve` is the width of whatever
  // sits left of the chat (the sessions sidebar) and must stay readable.
  let {
    width = $bindable(TERMINAL_DEFAULT),
    storageKey,
    reserve = 0,
    hideBelow = 760,
    label = "Resize terminal panel",
  }: {
    width?: number;
    storageKey: string;
    reserve?: number;
    hideBelow?: number;
    label?: string;
  } = $props();

  let el: HTMLDivElement | undefined = $state();
  let dragging = $state(false);
  let vw = $state(typeof window === "undefined" ? 1280 : window.innerWidth);
  // Below this the pane overlays the chat (its own media query), so there is
  // nothing to drag; render no handle and leave the pane's overlay alone.
  const wide = $derived(vw > hideBelow);
  const max = $derived(terminalMax(vw - reserve - SPLITTER_WIDTH - CHAT_MIN));

  function space() {
    const container = el?.parentElement?.getBoundingClientRect().width ?? vw;
    return terminalSpace(container, reserve);
  }
  function apply(next: number, persist: boolean) {
    width = clampTerminalWidth(next, space());
    if (persist) setPref(storageKey, String(width));
  }

  let drag: { x: number; width: number } | null = null;
  function down(e: PointerEvent) {
    if (e.button !== 0 || !el) return;
    e.preventDefault();
    drag = { x: e.clientX, width };
    dragging = true;
    try { el.setPointerCapture(e.pointerId); } catch { /* not every pointer can capture */ }
    el.focus();
  }
  function move(e: PointerEvent) {
    if (!drag) return;
    apply(drag.width + (drag.x - e.clientX), false);
  }
  function up(e: PointerEvent) {
    if (!drag) return;
    drag = null;
    dragging = false;
    try { el?.releasePointerCapture(e.pointerId); } catch { /* already released */ }
    setPref(storageKey, String(width));
  }
  function reset(e: MouseEvent) {
    e.preventDefault();
    apply(TERMINAL_DEFAULT, true);
  }
  function key(e: KeyboardEvent) {
    const step = e.shiftKey ? 64 : 16;
    let next: number;
    if (e.key === "ArrowLeft") next = width + step;
    else if (e.key === "ArrowRight") next = width - step;
    else if (e.key === "Home") next = TERMINAL_DEFAULT;
    else return;
    e.preventDefault();
    apply(next, true);
  }

  onMount(() => {
    const measure = () => {
      vw = window.innerWidth;
      // A width stored on a wider screen must not squeeze the chat here.
      width = clampTerminalWidth(width, space());
    };
    measure();
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  });
</script>

{#if wide}
  <!-- A separator is a keyboard-operable control: arrow keys resize, not "activate". -->
  <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div
    class="splitter"
    class:dragging
    bind:this={el}
    role="separator"
    aria-orientation="vertical"
    aria-label={label}
    aria-valuemin={TERMINAL_MIN}
    aria-valuemax={max}
    aria-valuenow={width}
    title="Drag to resize · double-click to reset"
    tabindex="0"
    onpointerdown={down}
    onpointermove={move}
    onpointerup={up}
    onpointercancel={up}
    ondblclick={reset}
    onkeydown={key}
  ></div>
{/if}

<style>
  .splitter { position: relative; width: 8px; flex: none; align-self: stretch; cursor: col-resize; touch-action: none; }
  .splitter::before {
    content: ""; position: absolute; inset: 10px 3px; border-radius: 3px;
    background: var(--line); opacity: 0; transition: opacity .12s;
  }
  .splitter:hover::before, .splitter:focus-visible::before, .splitter.dragging::before { opacity: 1; background: var(--muted); }
  .splitter:focus-visible { outline: 2px solid var(--accent); outline-offset: -2px; border-radius: 4px; }
</style>
