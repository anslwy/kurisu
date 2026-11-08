// The /web chat state: which sessions are listed, which one is open, the rows
// on screen and the live connection. Views stay dumb; everything that decides
// *what* to show lives here.

import { api, WebApiError } from "./api";
import type { Message, Session, StreamEvent } from "./types";

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

  private seen = new Set<number>();
  private stream: EventSource | null = null;

  get current(): Session | null {
    return this.sessions.find((s) => s.id === this.currentId) ?? null;
  }

  get rows(): Session[] {
    return visibleSessions(this.sessions, this.showArchived);
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
