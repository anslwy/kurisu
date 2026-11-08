// Static scroll contract, run with svelte-check. Browser/Safari gestures still
// need manual verification; these assertions do not simulate a layout engine.
import assert from "node:assert/strict";
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
assert.match(app, /--safe-bottom:\s*env\(safe-area-inset-bottom/, "safe-area value must come from env()");
for (const file of ["index.html", "chat.html"]) {
  const viewport = read(file).match(/<meta\s+name="viewport"[^>]+>/)?.[0];
  assert.ok(viewport, `${file}: missing viewport`);
  assert.doesNotMatch(viewport, /user-scalable\s*=\s*no|maximum-scale/i, `${file}: preserve pinch zoom`);
}
console.log("scroll layout contract: passed (static checks; no browser dependency)");

const panel = read("src/components/TerminalPanel.svelte");
for (const sidebar of ["src/components/Sidebar.svelte", "src/chat/Sidebar.svelte"]) {
  const source = read(sidebar);
  assert.match(source, /aria-expanded=\{terminalOpen\}/);
  assert.match(source, /onclick=\{onterminal\}>TERMINAL/);
}
for (const app of [shell, read("src/chat/Chat.svelte")]) {
  assert.match(app, /terminalOpen = !terminalOpen/);
  assert.match(app, /\{#if terminalOpen\}<TerminalPanel/);
  assert.match(app, /onclose=\{\(\) => terminalOpen = false\}/);
}
assert.match(panel, /aria-label="Close terminal"/);
assert.match(panel, /return \(\) => \{ stopped = true; ws\?\.close\(\)/);
declaration(panel, ".terminal-host", "min-height", "0");
assert.match(panel, /max-width: 1100px/);
assert.match(panel, /position: fixed; inset: 0; width: 100%/);
console.log("terminal toggle, mount, cleanup and mobile overlay contract: passed");
