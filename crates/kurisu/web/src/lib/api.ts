// Kurisu API v1 client (docs/api.md). Works same-origin (served by `ku web`)
// or against a remote base URL (e.g. a Kurisu over Tailscale, a Tauri app).

export interface Session {
  id: string; name: string; role: "lead" | "worker"; harness: string; cwd: string;
  model: string; effort: string | null; status: string; lifecycle: "active" | "paused" | "archived";
  description: string; note: string; created_by_session: string | null; created_by_name: string | null;
  children: { id: string; name: string }[]; replies?: number;
  last?: { seq: number; role: string; text: string; at: number };
}
export interface Message { seq: number; session: string; role: string; text: string; at: number }
export interface AuditRow { seq: number; actor: string; actor_name: string; target: string; target_name: string | null; field: string; old: string | null; new: string | null; at: number }
export interface UpdateResult { name: string; changes: { field: string; new: string }[]; unchanged: string[]; warning?: string | null }

export class ApiError extends Error { constructor(msg: string, public status: number) { super(msg); } }

export class Api {
  constructor(public base: string, public token: string) {}

  private url(p: string) { return this.base.replace(/\/$/, "") + "/api/v1" + p; }

  async req<T>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await fetch(this.url(path), {
      method,
      headers: { Authorization: `Bearer ${this.token}`, ...(body ? { "content-type": "application/json" } : {}) },
      body: body ? JSON.stringify(body) : undefined,
    });
    let data: { ok?: T; error?: string } = {};
    try { data = await res.json(); } catch { data = { error: `${res.status} ${res.statusText}` }; }
    if (!res.ok || data.error) throw new ApiError(data.error || `${res.status} ${res.statusText}`, res.status);
    return data.ok as T;
  }

  status() { return this.req<{ api: string; version: string; clis: string[]; server: { rss_kb?: number } | null; home: string }>("GET", "/status"); }
  sessions() { return this.req<Session[]>("GET", "/sessions?all=true"); }
  messages(id: string, after = 0) { return this.req<Message[]>("GET", `/sessions/${encodeURIComponent(id)}/messages?after=${after}`); }
  events(id: string) { return this.req<AuditRow[]>("GET", `/sessions/${encodeURIComponent(id)}/events`); }
  send(id: string, text: string) { return this.req<{ reply: string }>("POST", `/sessions/${encodeURIComponent(id)}/messages`, { text }); }
  update(id: string, fields: Record<string, string>) { return this.req<UpdateResult>("POST", `/sessions/${encodeURIComponent(id)}/update`, fields); }
  stop(id: string) { return this.req<{ name: string; was_running: boolean }>("POST", `/sessions/${encodeURIComponent(id)}/stop`); }
  create(name: string, description: string, model?: string) { return this.req<Session & { warning?: string }>("POST", "/sessions", { name, description, model }); }
  lead(text: string) { return this.req<{ id: string; reply: string }>("POST", "/lead", { text }); }

  /** Server-sent events over fetch (keeps the token in a header). Reconnects with backoff. */
  stream(on: (event: string, data: any) => void, onState: (connected: boolean) => void): () => void {
    let stopped = false, ctrl: AbortController | null = null, delay = 500;
    const run = async () => {
      while (!stopped) {
        ctrl = new AbortController();
        try {
          const res = await fetch(this.url("/stream"), { headers: { Authorization: `Bearer ${this.token}` }, signal: ctrl.signal });
          if (!res.ok || !res.body) throw new ApiError(`stream ${res.status}`, res.status);
          onState(true); delay = 500;
          const reader = res.body.getReader(); const dec = new TextDecoder(); let buf = "";
          for (;;) {
            const { done, value } = await reader.read();
            if (done) break;
            buf += dec.decode(value, { stream: true });
            let i;
            while ((i = buf.indexOf("\n\n")) >= 0) {
              const chunk = buf.slice(0, i); buf = buf.slice(i + 2);
              let ev = "message", data = "";
              for (const line of chunk.split("\n")) {
                if (line.startsWith("event:")) ev = line.slice(6).trim();
                else if (line.startsWith("data:")) data += line.slice(5).trim();
              }
              if (data) { try { on(ev, JSON.parse(data)); } catch { /* ignore malformed */ } }
            }
          }
        } catch (e) {
          if (stopped) return;
          if (e instanceof ApiError && e.status === 401) { onState(false); return; }
        }
        onState(false);
        await new Promise((r) => setTimeout(r, delay)); delay = Math.min(delay * 2, 8000);
      }
    };
    run();
    return () => { stopped = true; ctrl?.abort(); };
  }
}
