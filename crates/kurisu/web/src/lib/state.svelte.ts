import { Api, type AuditRow, type Message, type Session } from "./api";

const ls = {
  get(k: string) { try { return localStorage.getItem(k); } catch { return null; } },
  set(k: string, v: string) { try { localStorage.setItem(k, v); } catch { /* storage unavailable */ } },
  del(k: string) { try { localStorage.removeItem(k); } catch { /* ignore */ } },
};
export const storage = ls;

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
    if (ev === "hello") { if (data.sessions?.length) this.sessions = data.sessions; }
    else if (ev === "sessions") this.sessions = data;
    else if (ev === "message") {
      const m = data as Message;
      if (m.session === this.selected && !this.messages.some((x) => x.seq === m.seq)) {
        this.messages.push(m);
        if (m.role === "user") { const i = this.pending.findIndex((p) => p.text.trim() === m.text.trim()); if (i >= 0) this.pending.splice(i, 1); }
      }
    } else if (ev === "audit") {
      const a = data as AuditRow;
      if (a.target === this.selected && !this.audit.some((x) => x.seq === a.seq)) this.audit.push(a);
    } else if (ev === "resync") this.resync();
  }

  async resync() {
    if (!this.api) return;
    try { this.sessions = await this.api.sessions(); } catch { /* stream will retry */ }
    if (this.selected) await this.loadChannel(this.selected);
  }

  async loadChannel(id: string) {
    if (!this.api) return;
    const [m, a] = await Promise.all([this.api.messages(id), this.api.events(id)]);
    if (id !== this.selected) return;
    this.messages = m; this.audit = a;
  }

  async open(id: string) {
    this.selected = id; this.messages = []; this.audit = []; this.pending = []; this.navOpen = false;
    location.hash = new URLSearchParams({ chat: this.sessions.find((s) => s.id === id)?.name ?? id }).toString();
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
