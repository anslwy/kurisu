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
  assert.equal(headers.length, 1, `${file}: one chat header`);
  assert.equal(findNodes(headers[0].fragment, isToggle).length, 1, `${file}: toggle must be inside the chat header`);
  assert.equal(findNodes(fragment, isToggle).length, 1, `${file}: no duplicate toggle`);
}
const toggle = read("src/components/TerminalToggle.svelte");
assert.match(toggle, /title="Terminal" aria-label="Terminal" aria-expanded=\{open\}/);
assert.match(toggle, /stroke-width="1.6"/);
assert.match(toggle, /width="20" height="20"/);
assert.match(toggle, /\.pressed \{[^}]*var\(--accent\)/);
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
