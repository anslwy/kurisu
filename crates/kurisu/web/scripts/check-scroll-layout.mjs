// Static scroll contract, run with svelte-check. Browser/Safari gestures still
// need manual verification; these assertions do not simulate a layout engine.
import assert from "node:assert/strict";
import { parse } from "svelte/compiler";
import { readFileSync } from "node:fs";

const read = (path) => readFileSync(new URL(`../${path}`, import.meta.url), "utf8");
const channel = read("src/components/Channel.svelte");
const app = read("src/app.css");
const shell = read("src/App.svelte");
function rule(css, selector) {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = css.match(new RegExp(`${escaped}\\s*\\{([^}]+)\\}`));
  assert.ok(match, `missing rule: ${selector}`);
  return match[1];
}
function declaration(css, selector, property, value) {
  assert.match(rule(css, selector), new RegExp(`${property}:\\s*${value}\\s*;`),
    `${selector} must keep ${property}: ${value}`);
}
declaration(app, "html, body, #app", "overflow", "hidden");
declaration(app, "html, body, #app", "overscroll-behavior", "none");
declaration(app, "body", "position", "fixed");
declaration(app, "body", "height", "100dvh");
declaration(shell, ".layout", "height", "100dvh");
declaration(channel, ".feed", "overflow-y", "auto");
declaration(channel, ".feed", "min-height", "0");
declaration(channel, ".feed", "overscroll-behavior", "contain");
declaration(channel, ".feed", "-webkit-overflow-scrolling", "touch");
declaration(channel, ".sysline", "overflow-wrap", "anywhere");
assert.match(rule(channel, ".msg"), /minmax\(0,\s*1fr\)/, "message content must shrink around code blocks");
assert.match(rule(channel, "main"), /var\(--safe-bottom\)/, "channel must reserve the bottom safe area");
assert.match(read("src/palette.css"), /--safe-bottom:\s*env\(safe-area-inset-bottom/, "safe-area value must come from env()");
for (const file of ["index.html", "chat.html"]) {
  const viewport = read(file).match(/<meta\s+name="viewport"[^>]+>/)?.[0];
  assert.ok(viewport, `${file}: missing viewport`);
  assert.doesNotMatch(viewport, /user-scalable\s*=\s*no|maximum-scale/i, `${file}: preserve pinch zoom`);
}
console.log("scroll layout contract: passed (static checks; no browser dependency)");

const panel = read("src/components/TerminalPanel.svelte");
for (const sidebar of ["src/components/Sidebar.svelte", "src/chat/Sidebar.svelte"]) {
  assert.doesNotMatch(read(sidebar), /TERMINAL|TerminalToggle|onterminal/);
}
function findNodes(node, match) {
  if (!node || typeof node !== "object") return [];
  return [...(match(node) ? [node] : []), ...Object.values(node).flatMap((value) => findNodes(value, match))];
}
for (const [file, header] of [["src/components/Channel.svelte", "chead"], ["src/chat/Chat.svelte", "topbar"]]) {
  const fragment = parse(read(file), { modern: true }).fragment;
  const headers = findNodes(fragment, (node) => node.type === "RegularElement" &&
    node.attributes.some((a) => a.name === "class" && Array.isArray(a.value) && a.value.some((v) => v.type === "Text" && v.data === header)));
  const isToggle = (node) => node.type === "Component" && node.name === "TerminalToggle";
  const isSidebar = (node) => node.type === "Component" && node.name === "SidebarToggle";
  assert.equal(headers.length, 1, `${file}: one chat header`);
  assert.equal(findNodes(headers[0].fragment, isToggle).length, 1, `${file}: toggle must be inside the chat header`);
  assert.equal(findNodes(fragment, isToggle).length, 1, `${file}: no duplicate toggle`);
  assert.equal(findNodes(headers[0].fragment, isSidebar).length, 1, `${file}: sessions toggle belongs in the chat header`);
  assert.equal(findNodes(fragment, isSidebar).length, 1, `${file}: no duplicate sessions toggle`);
}
const button = read("src/components/IconButton.svelte");
assert.equal((read("src/components/TerminalToggle.svelte").match(/\.terminal-toggle/g) || []).length, 0,
  "the shared icon-button style has one home, not a per-toggle copy");
assert.match(read("src/components/TerminalToggle.svelte"), /<IconButton title="Terminal" pressed=\{open\} expanded=\{open\}/);
assert.match(read("src/components/SidebarToggle.svelte"), /<IconButton title=\{label\} pressed=\{collapsed\} expanded=\{!collapsed\}/);
assert.match(read("src/components/SidebarToggle.svelte"), /@media \(max-width: 760px\)/);
assert.match(button, /class="icon-button"/);
assert.match(button, /aria-label=\{label \?\? title\}/);
assert.match(button, /aria-expanded=\{expanded\}/);
assert.match(button, /\.icon-button\.pressed \{[^}]*var\(--accent\)/);
assert.match(button, /\.icon-button:focus-visible \{[^}]*var\(--accent\)/);
for (const toggle of [read("src/components/TerminalToggle.svelte"), read("src/components/SidebarToggle.svelte")]) {
  assert.match(toggle, /stroke-width="1.6"/);
  assert.match(toggle, /width="20" height="20"/);
}
console.log("one shared icon-button, terminal and sessions toggles in both headers: passed");

// Resizable terminal splitter + collapsible sessions sidebar, in both apps.
const splitter = read("src/components/Splitter.svelte");
assert.match(splitter, /role="separator"/);
assert.match(splitter, /aria-orientation="vertical"/);
assert.match(splitter, /aria-valuenow=\{width\}/);
assert.match(splitter, /tabindex="0"/);
assert.match(splitter, /cursor: col-resize/);
assert.match(splitter, /ondblclick=\{reset\}/);
assert.match(splitter, /onkeydown=\{key\}/);
assert.match(splitter, /e\.key === "ArrowLeft"/);
assert.match(splitter, /e\.key === "ArrowRight"/);
assert.match(splitter, /clampTerminalWidth/);
assert.match(splitter, /setPref\(storageKey/);
const layout = read("src/lib/layout.ts");
assert.match(layout, /export const CHAT_MIN = 360/);
assert.match(layout, /export const TERMINAL_MIN = 320/);
assert.match(layout, /export function clampTerminalWidth/);
assert.match(layout, /localStorage\.(get|set)Item/);

for (const [file, storageKey, flagKey, reserve] of [
  ["src/App.svelte", "ku.terminalWidth", "ku.sidebarHidden", 260],
  ["src/chat/Chat.svelte", "ku_web_terminal_width", "ku_web_sidebar_hidden", 320],
]) {
  const app = read(file);
  assert.match(app, new RegExp(`storedWidth\\("${storageKey}"\\)`), `${file}: restore the terminal width`);
  assert.match(app, new RegExp(`storageKey="${storageKey}"`), `${file}: persist the terminal width`);
  assert.match(app, new RegExp(`reserve=\\{sidebarHidden \\? 0 : ${reserve}\\}`), `${file}: keep the chat readable`);
  assert.match(app, /<Splitter bind:width=\{terminalWidth\}/);
  assert.match(app, /\{#if terminalOpen\}<Splitter/, `${file}: the handle only exists with the pane`);
  assert.match(app, /hideBelow=\{1100\}/, `${file}: no handle once the pane overlays the chat`);
  assert.match(app, /--terminal-w:\{terminalWidth\}px/, `${file}: the pane width comes from state`);
  assert.match(app, /\b(collapsed|hidden)=\{sidebarHidden\}/, `${file}: sidebar follows the toggle`);
  assert.match(app, new RegExp(`storedFlag\\("${flagKey}"\\)`), `${file}: restore the sidebar state`);
  assert.match(app, new RegExp(`setFlag\\("${flagKey}"`), `${file}: persist the sidebar state`);
}
assert.match(read("src/components/Sidebar.svelte"), /nav\.collapsed \{ display: none; \}/);
assert.match(read("src/chat/chat.css"), /\.side\.hidden \{ display: none; \}/);
console.log("splitter, restored width and collapsible sessions contract: passed");
for (const app of [shell, read("src/chat/Chat.svelte")]) {
  assert.match(app, /terminalOpen = !terminalOpen/);
  assert.match(app, /\{#if terminalOpen\}<TerminalPanel/);
  assert.match(app, /onclose=\{\(\) => terminalOpen = false\}/);
}
assert.match(panel, /aria-label="Close terminal"/);
assert.match(panel, /return \(\) => \{ stopped = true; ws\?\.close\(\)/);
declaration(panel, ".terminal-host", "min-height", "0");
assert.match(panel, /max-width: 1100px/);
assert.match(panel, /position: fixed; inset: calc\(var\(--safe-top\) \+ var\(--chat-header-height\)\) 0 0; width: 100%/);
assert.match(panel, /class="terminal-panel themed-surface"/);
declaration(panel, ".themed-surface", "background", "var\\(--bg2\\)");
assert.doesNotMatch(panel, /#[0-9a-f]{3,8}\b/i);
assert.match(panel, /scheme.addEventListener\("change", applyTheme\)/);
assert.match(panel, /scheme.removeEventListener\("change", applyTheme\)/);
console.log("terminal toggle, mount, cleanup and mobile overlay contract: passed");
