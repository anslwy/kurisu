// Exercise the shared splitter geometry and preference helpers without a browser.
import assert from "node:assert/strict";
import { mkdtemp, readFile, writeFile, rm } from "node:fs/promises";
import ts from "typescript";

const root = new URL("../", import.meta.url);
const store = new Map();
globalThis.localStorage = {
  getItem: (k) => (store.has(k) ? store.get(k) : null),
  setItem: (k, v) => store.set(k, String(v)),
  removeItem: (k) => store.delete(k),
};

const tmp = await mkdtemp(new URL(".layout-test-", root).pathname);
try {
  const source = await readFile(new URL("src/lib/layout.ts", root), "utf8");
  const js = ts.transpileModule(source, {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
  }).outputText;
  await writeFile(`${tmp}/layout.mjs`, js);
  const L = await import(`${tmp}/layout.mjs`);

  // Defaults fit between the minimum and the chat's readable width.
  assert.ok(L.TERMINAL_MIN < L.TERMINAL_DEFAULT && L.TERMINAL_DEFAULT < L.TERMINAL_MAX);

  // terminalMax never drops below the terminal minimum, even in a tiny window.
  assert.equal(L.terminalMax(-500), L.TERMINAL_MIN);
  assert.equal(L.terminalMax(100000), L.TERMINAL_MAX);
  assert.equal(L.terminalMax(500.9), 500);

  // Widths clamp to a usable range and survive junk.
  assert.equal(L.clampTerminalWidth(10, 800), L.TERMINAL_MIN);
  assert.equal(L.clampTerminalWidth(2000, 800), 800);
  assert.equal(L.clampTerminalWidth(430.6, 800), 431);
  assert.equal(L.clampTerminalWidth(Number.NaN, 800), L.TERMINAL_DEFAULT);
  assert.equal(L.clampTerminalWidth(500, 100), L.TERMINAL_MIN, "small windows still yield a usable pane");

  // terminalSpace leaves the chat its readable width and takes the sidebar
  // and the handle out of the container first.
  assert.equal(L.terminalSpace(1280, 320), 1280 - 320 - L.SPLITTER_WIDTH - L.CHAT_MIN);

  // Preferences round-trip, and a corrupt/absent width falls back to default.
  assert.equal(L.storedWidth("w"), L.TERMINAL_DEFAULT);
  L.setPref("w", "512");
  assert.equal(L.storedWidth("w"), 512);
  L.setPref("w", "not-a-number");
  assert.equal(L.storedWidth("w"), L.TERMINAL_DEFAULT);
  L.setPref("w", "-3");
  assert.equal(L.storedWidth("w"), L.TERMINAL_DEFAULT, "zero/negative widths are ignored");
  assert.equal(L.storedFlag("f"), false);
  L.setFlag("f", true);
  assert.equal(L.storedFlag("f"), true);
  L.setFlag("f", false);
  assert.equal(L.storedFlag("f"), false);

  // localStorage can be unavailable (private mode); nothing may throw.
  globalThis.localStorage = { getItem() { throw new Error("denied"); }, setItem() { throw new Error("denied"); }, removeItem() { throw new Error("denied"); } };
  assert.equal(L.storedWidth("w"), L.TERMINAL_DEFAULT);
  L.setPref("w", "1");
  assert.equal(L.storedFlag("f"), false);
  console.log("layout geometry and preferences: passed");
} finally {
  await rm(tmp, { recursive: true, force: true });
}
