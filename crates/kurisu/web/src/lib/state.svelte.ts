import { Api, type Approval, type AuditRow, type Message, type Session } from "./api";

const ls = {
  get(k: string) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k: string, v: string) { try { localStorage.setItem(k, v); } catch { /* storage unavailable */ } },
  del(k: string) { try { localStorage.removeItem(k); } catch { /* ignore */ } },
};
export const storage = ls;

/** Merge two seq-keyed lists: no duplicates, ordered by seq (live events and fetched history can overlap). */
export function mergeBySeq<T extends { seq: number }>(a: T[], b: T[]): T[] {
  const m = new Map<number, T>();
  for (const x of a) m.set(x.seq, x);
  for (const x of b) if (!m.has(x.seq)) m.set(x.seq, x);
  return [...m.values()].sort((x, y) => x.seq - y.seq);
}

export interface Pending { key: number; text: string; at: number; state: "sending" | "sent" | "failed"; error?: string }

class AppState {
  api: Api | null = $state(null);
  connected = $state(false);
  authError = $state("");
  sessions: Session[] = $state([]);
  selected: string | null = $state(null);
  messages: Message[] = $state([]);
  audit: AuditRow[] = $state([]);
  pending: Pending[] = $state([]);
  /** Pending approvals across all sessions (live from the stream). */
  approvals: Approval[] = $state([]);
  /** Approvals of the open channel, any status (for the cards' final state). */
  approvalLog: Record<string, Approval> = $state({});
  loaded = $state(false);
  loadError = $state("");
  private loading = false;
  serverRss: number | null = $state(null);
  groupBy: "tree" | "role" = $state((ls.get("kurisu.groupBy") as "tree" | "role") || "tree");
  closed: string[] = $state(JSON.parse(ls.get("kurisu.closed") || '["__archived"]'));
  navOpen = $state(false);
  detailsOpen = $state(false);
  switcherOpen = $state(false);
  addOpen = $state(false);
  private stopStream: (() => void) | null = null;
  private pkey = 0;

  current = $derived(this.sessions.find((s) => s.id === this.selected) ?? null);

  /** Connection settings: base URL ('' = same origin) and token. */
  connect(base: string, token: string) {
    this.stopStream?.();
    this.api = new Api(base, token);
    this.authError = "";
    return this.api.status().then((st) => {
      ls.set("kurisu.base", base); ls.set("kurisu.token", token);
      this.serverRss = st.server?.rss_kb ?? null;
      this.stopStream = this.api!.stream((e, d) => this.onEvent(e, d), (c) => {
        this.connected = c;
        if (c) this.resync();
      });
      return true;
    }).catch((e) => { this.api = null; this.authError = e.message; return false; });
  }
  forget() { this.stopStream?.(); ls.del("kurisu.token"); this.api = null; this.connected = false; }

  private onEvent(ev: string, data: any) {
    if (ev === "hello") {
      if (data.sessions?.length) this.sessions = data.sessions;
      if (Array.isArray(data.approvals)) this.setApprovals(data.approvals);
    }
    else if (ev === "sessions") { this.sessions = data; this.syncHash(); }
    else if (ev === "approvals") this.setApprovals(data as Approval[]);
    else if (ev === "message") {
      const m = data as Message;
      if (m.session !== this.selected) return;
      // History not loaded yet (or its fetch failed): fetch it, merging this event in.
      if (!this.loaded) { this.messages = mergeBySeq(this.messages, [m]); this.loadChannel(m.session); return; }
      if (!this.messages.some((x) => x.seq === m.seq)) {
        this.messages = mergeBySeq(this.messages, [m]);
        if (m.role === "user") { const i = this.pending.findIndex((p) => p.text.trim() === m.text.trim()); if (i >= 0) this.pending.splice(i, 1); }
      }
    } else if (ev === "audit") {
      const a = data as AuditRow;
      if (a.target !== this.selected) return;
      this.audit = mergeBySeq(this.audit, [a]);
      if (!this.loaded) this.loadChannel(a.target);
    } else if (ev === "resync") this.resync();
  }

  private setApprovals(list: Approval[]) {
    const before = new Set(this.approvals.map((a) => a.id));
    this.approvals = list;
    const now = new Set(list.map((a) => a.id));
    for (const a of list) if (a.session === this.selected) this.approvalLog[a.id] = a;
    // Something left the pending list in the open channel: fetch its final state.
    const settled = [...before].some((id) => !now.has(id) && this.approvalLog[id]);
    if (settled && this.selected) this.loadApprovals(this.selected);
  }

  async loadApprovals(id: string) {
    if (!this.api) return;
    try {
      const list = await this.api.approvals(id, true);
      if (id !== this.selected) return;
      const log: Record<string, Approval> = {};
      for (const a of list) log[a.id] = a;
      this.approvalLog = log;
    } catch { /* cards fall back to the pending list */ }
  }

  async approve(id: string, always = false) {
    try { await this.api!.approve(id, always); } catch (e: any) { alert(e.message); }
    if (this.selected) this.loadApprovals(this.selected);
  }
  async deny(id: string, reason?: string) {
    try { await this.api!.deny(id, reason); } catch (e: any) { alert(e.message); }
    if (this.selected) this.loadApprovals(this.selected);
  }

  /** Keep `#chat=<name>` in the address bar in step with the open channel (renames included). */
  syncHash() {
    const s = this.current;
    const h = new URLSearchParams(location.hash.slice(1));
    const want = s ? s.name : null;
    if ((h.get("chat") || null) === want) return;
    if (want) h.set("chat", want); else h.delete("chat");
    history.replaceState(null, "", location.pathname + location.search + (h.toString() ? "#" + h.toString() : ""));
  }

  async resync() {
    if (!this.api) return;
    try { this.sessions = await this.api.sessions(); } catch { /* stream will retry */ }
    if (this.selected) await this.loadChannel(this.selected);
  }

  /** Fetch a channel's history and merge it with any live events that arrived meanwhile; retries on failure. */
  async loadChannel(id: string, attempt = 0): Promise<void> {
    if (!this.api || (this.loading && attempt === 0)) return;
    this.loading = true;
    try {
      const [m, a] = await Promise.all([this.api.messages(id), this.api.events(id)]);
      if (id !== this.selected) return;
      this.messages = mergeBySeq(m, this.messages);
      this.audit = mergeBySeq(a, this.audit);
      this.loaded = true; this.loadError = "";
      this.loadApprovals(id);
    } catch (e: any) {
      if (id !== this.selected) return;
      this.loadError = e.message;
      if (attempt < 5) {
        this.loading = false;
        await new Promise((r) => setTimeout(r, 700 * (attempt + 1)));
        if (id === this.selected && !this.loaded) return this.loadChannel(id, attempt + 1);
      }
    } finally {
      this.loading = false;
    }
  }

  async open(id: string) {
    if (id !== this.selected) {
      this.selected = id; this.messages = []; this.audit = []; this.pending = []; this.approvalLog = {};
      this.loaded = false; this.loadError = ""; this.loading = false;
    }
    this.navOpen = false;
    const name = this.sessions.find((s) => s.id === id)?.name ?? id;
    const h = new URLSearchParams(location.hash.slice(1));
    if (h.get("chat") !== name) { h.set("chat", name); location.hash = h.toString(); }
    await this.loadChannel(id);
  }

  async send(text: string) {
    const s = this.current; if (!s || !this.api) return;
    const p: Pending = { key: ++this.pkey, text, at: Date.now() / 1000, state: "sending" };
    this.pending.push(p);
    try { await this.api.send(s.id, text); const q = this.pending.find((x) => x.key === p.key); if (q) q.state = "sent"; }
    catch (e: any) { const q = this.pending.find((x) => x.key === p.key); if (q) { q.state = "failed"; q.error = e.message; } }
  }

  // --- unread & sidebar prefs ---
  seen(id: string) { return Number(ls.get("kurisu.seen." + id) || 0); }
  markSeen(s: Session) { ls.set("kurisu.seen." + s.id, String(s.replies || 0)); }
  unread(s: Session) { return s.id === this.selected ? 0 : Math.max(0, (s.replies || 0) - this.seen(s.id)); }
  toggle(key: string) {
    this.closed = this.closed.includes(key) ? this.closed.filter((k) => k !== key) : [...this.closed, key];
    ls.set("kurisu.closed", JSON.stringify(this.closed));
  }
  setGroup(g: "tree" | "role") { this.groupBy = g; ls.set("kurisu.groupBy", g); }
  byName(n: string) { return this.sessions.find((s) => s.name === n && s.lifecycle !== "archived") ?? this.sessions.find((s) => s.name === n); }
}

export const app = new AppState();

export function stateOf(s: Session): [string, string] {
  if (s.status === "awaiting_approval") return ["approval", "waiting for approval"];
  if (s.lifecycle === "paused") return ["paused", "paused"];
  if (s.lifecycle === "archived") return ["stopped", "archived"];
  if (s.status === "working" || s.status === "starting") return ["busy", s.status === "starting" ? "starting" : "working"];
  if (s.status === "error") return ["error", "error"];
  if (s.status === "new") return ["new", "new"];
  if (s.status === "stopped") return ["stopped", "stopped"];
  return ["idle", "idle"];
}
export const initials = (n: string) => (n || "?").split(/[-_ ]+/).filter(Boolean).slice(0, 2).map((w) => w[0]).join("").toUpperCase() || "?";
export const hhmm = (t: number) => new Date(t * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
export function dayLabel(t: number) {
  const d = new Date(t * 1000), now = new Date(), y = new Date(now); y.setDate(now.getDate() - 1);
  if (d.toDateString() === now.toDateString()) return "Today";
  if (d.toDateString() === y.toDateString()) return "Yesterday";
  return d.toLocaleDateString([], { weekday: "long", month: "long", day: "numeric" });
}
