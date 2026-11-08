// Small display helpers shared by the chat components.

import { statusLabel, type Session } from "./types";

/** "14:32" today, "7 Nov" before that. */
export function clock(ts: number | null | undefined): string {
  if (!ts) return "";
  const d = new Date(ts * 1000);
  const now = new Date();
  if (d.toDateString() === now.toDateString()) {
    return d.toTimeString().slice(0, 5);
  }
  return d.toLocaleDateString(undefined, { day: "numeric", month: "short" });
}

export function initials(name: string): string {
  const s = name.replace(/[^A-Za-z0-9]/g, "").slice(0, 2);
  return s ? s.toUpperCase() : "?";
}

/** The one-line preview under a session's name. */
export function preview(s: Session): string {
  const last = s.last;
  if (!last) return statusLabel(s);
  const who = last.role === "user" ? "You: " : "";
  return `${who}${last.text}`.replace(/\s+/g, " ").slice(0, 90);
}

/** A compact one-liner for a value that may be text or JSON. */
export function compact(v: unknown, max = 400): string {
  if (v === null || v === undefined) return "";
  const text = typeof v === "string" ? v : JSON.stringify(v, null, 1) ?? "";
  return text.length > max ? `${text.slice(0, max)}…` : text;
}
