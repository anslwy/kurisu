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
