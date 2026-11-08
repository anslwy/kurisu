// Exercise actual compiled Svelte state without a browser or web listener.
import assert from 'node:assert/strict';
import { Script } from 'node:vm';
import { mkdtemp, readFile, writeFile, rm } from 'node:fs/promises';
import { compile, compileModule } from 'svelte/compiler';
import ts from 'typescript';
const root = new URL('../', import.meta.url);
const tmp = await mkdtemp(new URL('.chat-test-', root).pathname);
try {
  for (const name of ['types', 'api', 'state.svelte', 'build-warning']) {
    const source = await readFile(new URL(`src/chat/${name}.ts`, root), 'utf8');
    let code = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext } }).outputText;
    if (name === 'state.svelte') code = compileModule(code, { filename: 'state.svelte.js', generate: 'client' }).js.code;
    code = code.replaceAll('"./api"', '"./api.mjs"').replaceAll('"./types"', '"./types.mjs"');
    await writeFile(`${tmp}/${name}.mjs`, code);
  }
  class Events {
    static CLOSED = 2;
    static last;
    listeners = {};
    constructor() { Events.last = this; }
    addEventListener(name, f) { this.listeners[name] = f; }
    emit(name, data) { this.listeners[name]?.({ data: JSON.stringify(data) }); }
    close() {}
  }
  globalThis.EventSource = Events;
  const { ChatState, chat } = await import(`${tmp}/state.svelte.mjs`);
  const composer = await readFile(new URL('src/chat/Composer.svelte', root), 'utf8');
  const compiled = compile(composer, { filename: 'Composer.svelte', generate: 'server' }).js.code.replaceAll('"./state.svelte"', '"./state.svelte.mjs"');
  await writeFile(`${tmp}/Composer.mjs`, compiled);
  const { default: Composer } = await import(`${tmp}/Composer.mjs`);
  const { render } = await import('svelte/server');
  // The banner consumes the server's shared doctor check, rather than comparing
  // Vite hashes directly with unrelated git identifiers.
  const { checkBuild } = await import(`${tmp}/build-warning.mjs`);
  const savedFetch = globalThis.fetch;
  globalThis.fetch = async (url, options) => {
    assert.match(url, /bundle=old&git_sha=old-sha/);
    assert.equal(options.cache, 'no-store');
    return {ok:true,json:async()=>({status:'FAIL'})};
  };
  const warning = await checkBuild('old', 'old-sha');
  assert.match(warning, /different build.*Reload/);
  const banner = compile(await readFile(new URL('src/chat/BuildBanner.svelte', root), 'utf8'), {filename:'BuildBanner.svelte',generate:'server'}).js.code;
  await writeFile(`${tmp}/BuildBanner.mjs`, banner);
  const { default: BuildBanner } = await import(`${tmp}/BuildBanner.mjs`);
  assert.match(render(BuildBanner, {props:{warning}}).body, /role="status"/);
  globalThis.fetch = async()=>({ok:true,json:async()=>({status:'OK'})});
  assert.equal(await checkBuild('current','current-sha'), '');
  assert.doesNotMatch(render(BuildBanner, {props:{warning:''}}).body, /role="status"/);
  globalThis.fetch = savedFetch;

  chat.sessions = [{ id: 's', status: 'working', lifecycle: 'active' }];
  chat.currentId = 's';
  const html = render(Composer).body;
  assert.match(html, />Queue</);
  assert.match(html, />Steer now</);
  assert.match(html, /Stop the running turn/);

  // Compile and render the real sidebar/panel with Svelte's built-in server
  // renderer. No DOM/browser dependency; onMount does not start a WebSocket.
  for (const name of ['Sidebar', 'TerminalPanel', 'TerminalToggle', 'IconButton', 'SidebarToggle', 'Splitter']) {
    const path = name === 'Sidebar' ? 'src/chat/Sidebar.svelte' : `src/components/${name}.svelte`;
    let source = await readFile(new URL(path, root), 'utf8');
    if (name === 'TerminalPanel') {
      source = source.replace(/  import (?:xtermUrl|fitUrl) from [^;]+;/g, '')
        .replace(/  import "[^";]+\.css";/g, '');
      source = source.replace('  let { endpoint', '  const xtermUrl = "xterm.js", fitUrl = "fit.js";\n  let { endpoint');
    }
    let compiled = compile(source, { filename: name + '.svelte', generate: 'server' }).js.code
      .replaceAll('"./state.svelte"', '"./state.svelte.mjs"')
      .replaceAll('"./types"', '"./types.mjs"')
      .replaceAll('"../lib/terminal-theme"', '"./terminal-theme.mjs"')
      .replaceAll('"./IconButton.svelte"', '"./IconButton.mjs"')
      .replaceAll('"./TerminalToggle.svelte"', '"./TerminalToggle.mjs"')
      .replaceAll('"../lib/layout"', '"./layout.mjs"');
    // Sidebar formatting uses no browser APIs.
    const format = ts.transpileModule(await readFile(new URL('src/chat/format.ts', root), 'utf8'), {
      compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext }
    }).outputText.replaceAll('"./types"', '"./types.mjs"');
    await writeFile(`${tmp}/format.mjs`, format);
    compiled = compiled.replaceAll('"./format"', '"./format.mjs"');
    await writeFile(`${tmp}/${name}.mjs`, compiled);
  }
  const { default: Sidebar } = await import(`${tmp}/Sidebar.mjs`);
  chat.sessions = [];
  assert.doesNotMatch(render(Sidebar).body, /TERMINAL|aria-expanded/);
  const layoutCode = ts.transpileModule(await readFile(new URL('src/lib/layout.ts', root), 'utf8'), {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext }
  }).outputText;
  await writeFile(`${tmp}/layout.mjs`, layoutCode);
  const { default: TerminalToggle } = await import(`${tmp}/TerminalToggle.mjs`);
  const closed = render(TerminalToggle, { props: { open: false, ontoggle() {} } }).body;
  const open = render(TerminalToggle, { props: { open: true, ontoggle() {} } }).body;
  assert.match(closed, /aria-label="Terminal" aria-expanded="false"/);
  assert.match(open, /aria-label="Terminal" aria-expanded="true"/);
  assert.match(open, /class="[^"]*\bpressed\b/);
  assert.match(open, /stroke-width="1.6"/);
  assert.match(open, /width="20" height="20"/);
  // Same icon-button component, independent state: the sessions toggle says
  // what it will do and hides nothing on its own.
  const { default: SidebarToggle } = await import(`${tmp}/SidebarToggle.mjs`);
  const shown = render(SidebarToggle, { props: { collapsed: false, ontoggle() {} } }).body;
  const hidden = render(SidebarToggle, { props: { collapsed: true, ontoggle() {} } }).body;
  assert.match(shown, /aria-label="Hide sessions" aria-expanded="true"/);
  assert.match(hidden, /aria-label="Show sessions" aria-expanded="false"/);
  assert.match(hidden, /class="[^"]*\bpressed\b"/);
  assert.match(shown, /stroke-width="1.6"/);
  // The divider is a keyboard-reachable separator exposing its live width.
  const { default: Splitter } = await import(`${tmp}/Splitter.mjs`);
  const splitter = render(Splitter, { props: { width: 500, storageKey: 'ku.terminalWidth' } }).body;
  assert.match(splitter, /role="separator"/);
  assert.match(splitter, /aria-orientation="vertical"/);
  assert.match(splitter, /aria-valuemin="320"/);
  assert.match(splitter, /aria-valuenow="500"/);
  assert.match(splitter, /tabindex="0"/);
  const themeCode = await readFile(new URL('src/lib/terminal-theme.js', root), 'utf8');
  await writeFile(`${tmp}/terminal-theme.mjs`, themeCode);
  const remote = await readFile(new URL('../src/remote.rs', root), 'utf8');
  const terminalPage = remote.slice(remote.indexOf('fn terminal_page('));
  const inline = terminalPage.match(/<script src="\/fit.js"><\/script><script>([\s\S]*?)<\/script>/)?.[1];
  assert.ok(inline, 'standalone terminal script exists');
  new Script(inline.replace('__TERMINAL_THEME__', themeCode.replace('export function terminalTheme', 'function terminalTheme')));

  const { terminalTheme } = await import(`${tmp}/terminal-theme.mjs`);
  const palette = await readFile(new URL('src/palette.css', root), 'utf8');
  const [light, dark] = palette.split('@media (prefers-color-scheme: dark)');
  for (const css of [light, dark]) {
    const vars = Object.fromEntries([...css.matchAll(/(--[\w-]+):\s*(#[0-9a-f]{6})/g)].map((m) => [m[1], m[2]]));
    const theme = terminalTheme((key) => { assert.ok(vars[key], key); return vars[key]; });
    assert.equal(theme.background, vars['--bg2']);
    assert.equal(theme.foreground, vars['--fg']);
    assert.equal(theme.cursor, vars['--accent']);
    assert.equal(theme.selectionBackground, vars['--accent'] + '40');
    assert.equal(theme.red, vars['--accent']);
    assert.equal(theme.brightRed, vars['--err']);
    assert.equal(theme.green, vars['--ok']);
    assert.equal(theme.yellow, vars['--warn']);
    assert.equal(theme.blue, vars['--busy']);
    assert.equal(theme.brightBlack, vars['--muted']);
    assert.equal(Object.keys(theme).length, 22, 'all 16 ANSI colours plus six UI colours');
  }
  const { default: TerminalPanel } = await import(`${tmp}/TerminalPanel.mjs`);
  const panel = render(TerminalPanel, { props: { endpoint: '/_web/terminal', onclose() {} } }).body;
  assert.match(panel, /aria-label="Terminal"/);
  assert.match(panel, /terminal-panel themed-surface/);
  assert.match(panel, /aria-label="Close terminal"/);
  assert.match(panel, /aria-label="Terminal directory"/);

  const state = new ChatState();
  state.sessions = [{ id: 's', name: 'session', status: 'working', lifecycle: 'active' }];
  state.currentId = 's';
  state.connect();
  let request;
  const row = { seq: 7, session: 's', role: 'user', text: 'next', at: 1, data: { delivery: 'queued' } };
  globalThis.fetch = async (url, init) => {
    request = { url, ...JSON.parse(init.body) };
    // Stream beats HTTP: no duplicate pending bubble or stale queue status.
    Events.last.emit('delivery', { session: 's', seq: 7, state: 'delivered' });
    Events.last.emit('message', row);
    return Response.json({ seq: 7, queued: true, steered: false });
  };
  assert.equal(await state.send('next'), true);
  assert.equal(request.mode, 'queue');
  assert.equal(state.messages.length, 1);
  assert.equal(state.pending, null);
  assert.equal(state.delivery[7], 'delivered');
  assert.equal(state.running, true);
  globalThis.fetch = async (url, init) => {
    request = JSON.parse(init.body);
    return Response.json({ seq: 8, queued: false, steered: true });
  };
  assert.equal(await state.send('change course', 'steer'), true);
  assert.equal(request.mode, 'steer');
  assert.equal(state.delivery[8], 'steered');
  Events.last.emit('message', { ...row, seq: 8 });
  assert.equal(state.pending, null);
  Events.last.emit('turn', { session: 's', status: 'error' });
  assert.equal(state.delivery[7], 'delivered');
  globalThis.fetch = async () => Response.json({ messages: [{ ...row, data: { delivery: 'delivered' } }, { ...row, seq: 9 }], has_more: false });
  await state.select('s');
  assert.equal(state.delivery[9], 'queued');
  globalThis.fetch = async () => Response.json({ messages: [{ ...row, seq: 9, data: { delivery: 'delivered' } }], has_more: false });
  Events.last.emit('hello', { sessions: state.sessions, default: 's' });
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(state.delivery[9], 'delivered', 'reconnect refreshes a delivery event missed while disconnected');
  globalThis.fetch = async () => {
    Events.last.emit('turn', { session: 's', status: 'stopped' });
    Events.last.emit('turn', { session: 's', status: 'working' });
    return Response.json({ stopped: true, interrupted: true });
  };
  assert.equal(await state.stop(), true);
  assert.equal(state.running, true, 'Stop must not hide the next queued turn');
  Events.last.emit('turn', { session: 's', status: 'stopped' });
  Events.last.emit('tool', { session: 's', tool_call_id: 'old', status: 'cancelled' });
  assert.equal(state.running, false, 'cancelled tool cleanup must not restart the UI');
  state.close();
  console.log('PASS chat state: busy queue, steering, delivery/history races, Stop with queued turn');
} finally { await rm(tmp, { recursive: true, force: true }); }
