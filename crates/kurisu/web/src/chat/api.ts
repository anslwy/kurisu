// Typed client for the /web endpoints (crates/kurisu/src/chat.rs).
//
// No token: these routes are gated by the `ku_web` cookie that the pairing
// flow sets, so every call is same-origin and carries it automatically.

import type { ApiErrorBody, HistoryPage, InterruptResult, SendResult, SessionsResponse } from "./types";

export class WebApiError extends Error {
  constructor(message: string, public status: number) {
    super(message);
  }
}

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  let res: Response;
  try {
    res = await fetch(path, { credentials: "same-origin", cache: "no-store", ...init });
  } catch (e) {
    throw new WebApiError(e instanceof Error ? e.message : "network error", 0);
  }
  let body: unknown = null;
  try {
    body = await res.json();
  } catch {
    body = null;
  }
  if (!res.ok) {
    const msg = (body as ApiErrorBody | null)?.error ?? `${res.status} ${res.statusText}`;
    throw new WebApiError(msg, res.status);
  }
  return body as T;
}

export const api = {
  sessions: (archived = true) => req<SessionsResponse>(`/_web/api/sessions?archived=${archived ? "true" : "false"}`),

  messages: (id: string, q: { after?: number; before?: number; limit?: number } = {}) => {
    const p = new URLSearchParams();
    if (q.after !== undefined) p.set("after", String(q.after));
    if (q.before !== undefined) p.set("before", String(q.before));
    if (q.limit !== undefined) p.set("limit", String(q.limit));
    const qs = p.toString();
    return req<HistoryPage>(`/_web/api/sessions/${encodeURIComponent(id)}/messages${qs ? `?${qs}` : ""}`);
  },

  send: (id: string, text: string) =>
    req<SendResult>(`/_web/api/sessions/${encodeURIComponent(id)}/send`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ text }),
    }),

  interrupt: (id: string) =>
    req<InterruptResult>(`/_web/api/sessions/${encodeURIComponent(id)}/interrupt`, { method: "POST" }),
};
