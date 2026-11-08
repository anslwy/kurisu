// The /web chat state: which sessions are listed, which one is open, the rows
// on screen and the live connection. Views stay dumb; everything that decides
// *what* to show lives here.

import { api, WebApiError } from "./api";
import { liveState, type LiveBlock, type LiveTool, type LiveTurn, type Message, type Session, type StreamEvent } from "./types";

export type Connection = "connecting" | "live" | "reconnecting" | "closed";

const PAGE = 60;
const SESSION_KEY = "ku_web_session";

/** Session rows the sidebar shows, archived ones only when asked. */
export function visibleSessions(sessions: Session[], archived: boolean): Session[] {
  return archived ? sessions : sessions.filter((s) => s.lifecycle !== "archived");
}

/**
 * Which session to open: the newest live one, else any live one, else the
 * newest archived one — mirrors `chat::pick_default` on the server, so a page
 * opened with no stored choice lands where the API says it should.
 */
export function pickSession(sessions: Session[], wanted: string | null, fallback: string | null): Session | null {
  const by = (key: string | null) => (key ? sessions.find((s) => s.id === key || s.name === key) ?? null : null);
  return by(wanted) ?? by(fallback) ?? sessions.find((s) => s.lifecycle !== "archived") ?? sessions[0] ?? null;
}

export class ChatState {
  sessions = $state<Session[]>([]);
  currentId = $state<string | null>(null);
  messages = $state<Message[]>([]);
  hasMore = $state(false);
  loading = $state(false);
  sending = $state(false);
  error = $state<string | null>(null);
  connection = $state<Connection>("connecting");
  showArchived = $state(false);

  /** A send that is on its way: rendered as a dim bubble until it is recorded. */
  pending = $state<{ text: string; seq: number | null } | null>(null);

  /** In-flight turn output, kept per session so switching back does not lose it. */
  live = $state<Record<string, LiveTurn>>({});
  /** The last live turn state per session (sessions polling can lag behind it). */
  turnState = $state<Record<string, "working" | "idle" | "stopped" | "error">>({});
  stopping = $state(false);

  private seen = new Set<number>();
  private stream: EventSource | null = null;

  get current(): Session | null {
    return this.sessions.find((s) => s.id === this.currentId) ?? null;
  }

  get rows(): Session[] {
    return visibleSessions(this.sessions, this.showArchived);
  }

  get liveTurn(): LiveTurn | null {
    return this.currentId ? (this.live[this.currentId] ?? null) : null;
  }

  /** True while the selected session has a turn in flight. */
  get running(): boolean {
    const s = this.current;
    if (!s) return false;
    const state = this.turnState[s.id];
    return state === "working" || (state === undefined && liveState(s) === "working");
  }

  byId(id: string): Session | undefined {
    return this.sessions.find((s) => s.id === id);
  }

  /** Boot: list sessions, pick one, then follow the stream. */
  async boot(): Promise<void> {
    await this.loadSessions();
    const wanted = stored();
    const pick = pickSession(this.sessions, wanted, null);
    if (pick) await this.select(pick.id);
    this.connect();
  }

  async loadSessions(): Promise<void> {
    try {
      const r = await api.sessions(true);
      this.sessions = r.sessions;
      if (!this.currentId) {
        const pick = pickSession(this.sessions, stored(), r.default);
        if (pick) this.currentId = pick.id;
      }
      this.error = null;
    } catch (e) {
      this.error = message(e, "Could not load sessions");
    }
  }

  async select(id: string): Promise<void> {
    this.currentId = id;
    store(id);
    // Opening an archived session (the fallback when nothing is live) should
    // show its row, not hide it behind the toggle that is off by default.
    if (this.byId(id)?.lifecycle === "archived") this.showArchived = true;
    this.messages = [];
    this.hasMore = false;
    this.seen.clear();
    this.pending = null;
    this.loading = true;
    try {
      const page = await api.messages(id, { limit: PAGE });
      for (const m of page.messages) this.add(m);
      this.hasMore = page.has_more;
      this.error = null;
    } catch (e) {
      this.error = message(e, "Could not load the history");
    } finally {
      this.loading = false;
    }
  }

  /** Older rows, inserted above and keeping the scroll position. */
  async earlier(): Promise<number> {
    if (!this.currentId || this.messages.length === 0) return 0;
    const first = this.messages[0].seq;
    try {
      const page = await api.messages(this.currentId, { before: first, limit: PAGE });
      let added = 0;
      for (const m of page.messages) {
        if (this.seen.has(m.seq)) continue;
        this.seen.add(m.seq);
        this.messages = [m, ...this.messages];
        added++;
      }
      this.hasMore = page.has_more;
      return added;
    } catch (e) {
      this.error = message(e, "Could not load earlier messages");
      return 0;
    }
  }

  async send(text: string): Promise<boolean> {
    const id = this.currentId;
    const body = text.trim();
    if (!id || !body || this.sending) return false;
    this.sending = true;
    this.pending = { text: body, seq: null };
    try {
      const r = await api.send(id, body);
      // The server records the message before it answers, so the stream will
      // deliver it: keep the bubble until that row arrives (send() clears it).
      this.pending = { text: body, seq: r.seq };
      this.turnState = { ...this.turnState, [id]: "working" };
      this.error = null;
      return true;
    } catch (e) {
      this.pending = null;
      this.error = message(e, "Not sent");
      return false;
    } finally {
      this.sending = false;
    }
  }

  /** Ask the daemon to cancel the selected session's running turn. */
  async stop(): Promise<boolean> {
    const id = this.currentId;
    if (!id || this.stopping) return false;
    this.stopping = true;
    try {
      const r = await api.interrupt(id);
      const next = r.stopped ? "idle" : "working";
      this.turnState = { ...this.turnState, [id]: next };
      if (r.stopped) this.clearLive(id);
      else this.error = "The turn is still running; try stop again.";
      return r.stopped;
    } catch (e) {
      this.error = message(e, "Could not stop the turn");
      return false;
    } finally {
      this.stopping = false;
    }
  }

  connect(): void {
    this.stream?.close();
    const es = new EventSource("/_web/stream");
    this.stream = es;
    this.connection = "connecting";
    es.onopen = () => (this.connection = "live");
    es.onerror = () => {
      if (es.readyState === EventSource.CLOSED) this.connection = "closed";
      else this.connection = "reconnecting";
    };
    es.addEventListener("hello", (ev) => {
      const d = JSON.parse((ev as MessageEvent<string>).data) as Extract<StreamEvent, { type: "hello" }>;
      this.sessions = d.sessions;
      if (!this.currentId) {
        const pick = pickSession(this.sessions, stored(), d.default);
        if (pick) void this.select(pick.id);
      }
      this.connection = "live";
    });
    es.addEventListener("sessions", (ev) => {
      const d = JSON.parse((ev as MessageEvent<string>).data) as Session[];
      this.sessions = d;
    });
    es.addEventListener("message", (ev) => {
      const m = JSON.parse((ev as MessageEvent<string>).data) as Message;
      if (m.session === this.currentId) this.add(m);
    });
    es.addEventListener("delta", (ev) => {
      const d = JSON.parse((ev as MessageEvent<string>).data) as Extract<StreamEvent, { type: "delta" }>;
      this.onText(d.session, d.text);
    });
    es.addEventListener("tool", (ev) => {
      const d = JSON.parse((ev as MessageEvent<string>).data) as Extract<StreamEvent, { type: "tool" }>;
      this.onTool(d);
    });
    es.addEventListener("turn", (ev) => {
      const d = JSON.parse((ev as MessageEvent<string>).data) as Extract<StreamEvent, { type: "turn" }>;
      this.onTurn(d.session, d.status);
    });
    es.addEventListener("resync", () => (this.connection = "reconnecting"));
    es.addEventListener("closed", () => {
      this.connection = "closed";
      es.close();
    });
  }

  close(): void {
    this.stream?.close();
    this.stream = null;
  }

  /** Append a row once (the stream and the paged load overlap on purpose). */
  private add(m: Message): void {
    if (this.seen.has(m.seq)) return;
    this.seen.add(m.seq);
    this.messages = [...this.messages, m];
    if (this.pending && this.pending.seq === m.seq) this.pending = null;
    if (m.role === "agent") this.clearLiveText(m.session);
    if (m.role === "tool" && m.data?.tool_call_id) this.dropLiveTool(m.session, m.data.tool_call_id);
  }

  private onText(session: string, text: string): void {
    if (!text) return;
    this.turnState = { ...this.turnState, [session]: "working" };
    const prev = this.live[session];
    const blocks: LiveBlock[] = prev ? [...prev.blocks] : [];
    const last = blocks[blocks.length - 1];
    if (last?.kind === "text") blocks[blocks.length - 1] = { kind: "text", text: last.text + text };
    else blocks.push({ kind: "text", text });
    this.live = { ...this.live, [session]: { session, blocks, state: "working" } };
  }

  private onTool(d: Extract<StreamEvent, { type: "tool" }>): void {
    this.turnState = { ...this.turnState, [d.session]: "working" };
    // A persisted tool row has a history seq: show it as a normal (collapsed)
    // tool bubble and update it in place when the completion event arrives.
    // That keeps it after the turn ends and avoids a live/history duplicate.
    if (d.session === this.currentId && d.seq !== null && d.seq !== undefined) {
      const prior = this.messages.find((m) => m.seq === d.seq)?.data;
      const data = {
        tool_call_id: d.tool_call_id,
        tool: d.tool ?? prior?.tool ?? "tool",
        status: d.status ?? prior?.status ?? "running",
        input: d.input !== undefined ? d.input : prior?.input,
        output: d.output !== undefined ? d.output : prior?.output,
      };
      const found = this.messages.some((m) => m.seq === d.seq);
      if (found) {
        this.messages = this.messages.map((m) =>
          m.seq === d.seq ? { ...m, text: d.title ?? m.text, data } : m,
        );
      } else {
        this.add({
          seq: d.seq,
          session: d.session,
          role: "tool",
          text: d.title ?? d.tool ?? "tool",
          at: Math.floor(Date.now() / 1000),
          data,
        });
      }
      return;
    }
    const prev = this.live[d.session];
    const blocks: LiveBlock[] = prev ? [...prev.blocks] : [];
    const at = blocks.findIndex((b) => b.kind === "tool" && b.tool_call_id === d.tool_call_id);
    const old = at >= 0 ? (blocks[at] as LiveTool) : null;
    const tool: LiveTool = {
      kind: "tool",
      tool_call_id: d.tool_call_id,
      tool: d.tool ?? old?.tool ?? "tool",
      title: d.title ?? old?.title,
      status: d.status ?? old?.status ?? "running",
      input: d.input !== undefined ? d.input : old?.input,
      output: d.output !== undefined ? d.output : old?.output,
      seq: d.seq !== undefined ? d.seq : old?.seq,
    };
    if (at >= 0) blocks[at] = tool;
    else blocks.push(tool);
    this.live = { ...this.live, [d.session]: { session: d.session, blocks, state: "working" } };
  }

  private onTurn(session: string, status: "working" | "idle" | "stopped" | "error"): void {
    this.turnState = { ...this.turnState, [session]: status };
    const prev = this.live[session];
    if (status === "stopped" || status === "error") {
      this.closeRunningTools(session, status === "stopped" ? "cancelled" : "failed");
      this.clearLive(session);
      return;
    }
    if (prev) this.live = { ...this.live, [session]: { ...prev, state: status } };
    if (status === "idle") {
      // The final message is persisted just after the turn ends; clear any
      // remainder if that row does not arrive (e.g. a tool-only turn).
      setTimeout(() => {
        if (this.turnState[session] !== "working") this.clearLive(session);
      }, 2500);
    }
  }

  /** A cancelled turn never sends ToolCallUpdate; close anything still running. */
  private closeRunningTools(session: string, status: string): void {
    let changed = false;
    const messages = this.messages.map((m) => {
      if (m.session !== session || m.role !== "tool" || m.data?.status !== "running") return m;
      changed = true;
      return { ...m, data: { ...m.data, status } };
    });
    if (changed) this.messages = messages;
    const live = this.live[session];
    if (live?.blocks.some((b) => b.kind === "tool" && b.status === "running")) {
      this.live = {
        ...this.live,
        [session]: { ...live, blocks: live.blocks.map((b) => (b.kind === "tool" && b.status === "running" ? { ...b, status } : b)) },
      };
    }
  }

  private clearLiveText(session: string): void {
    const prev = this.live[session];
    if (!prev) return;
    const blocks = prev.blocks.filter((b) => b.kind !== "text");
    const next = { ...this.live };
    if (blocks.length) next[session] = { ...prev, blocks };
    else delete next[session];
    this.live = next;
  }

  private dropLiveTool(session: string, toolCallId: string): void {
    const prev = this.live[session];
    if (!prev) return;
    const blocks = prev.blocks.filter((b) => !(b.kind === "tool" && b.tool_call_id === toolCallId));
    const next = { ...this.live };
    if (blocks.length) next[session] = { ...prev, blocks };
    else delete next[session];
    this.live = next;
  }

  private clearLive(session: string): void {
    if (!(session in this.live)) return;
    const next = { ...this.live };
    delete next[session];
    this.live = next;
  }
}

function stored(): string | null {
  try {
    return localStorage.getItem(SESSION_KEY);
  } catch {
    return null;
  }
}

function store(id: string): void {
  try {
    localStorage.setItem(SESSION_KEY, id);
  } catch {
    /* private mode */
  }
}

function message(e: unknown, prefix: string): string {
  if (e instanceof WebApiError) return `${prefix}: ${e.message}`;
  return `${prefix}: ${e instanceof Error ? e.message : String(e)}`;
}

/** The one store the page uses. */
export const chat = new ChatState();
