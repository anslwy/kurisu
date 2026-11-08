// Shared geometry and preferences for the two web layouts (the standalone app
// and the /web chat): the terminal pane keeps room for its controls and the
// chat keeps a readable width; the splitter and the sessions toggle remember
// where the user left them.

export const TERMINAL_DEFAULT = 420;
export const TERMINAL_MIN = 320;
export const TERMINAL_MAX = 960;
/** Below this the chat gets too narrow to read (avatar gutter + text + padding). */
export const CHAT_MIN = 360;
export const SPLITTER_WIDTH = 8;

/**
 * The widest the terminal may get without squeezing the chat below CHAT_MIN.
 * `available` is the container width left over after the sessions sidebar and
 * the splitter; it is clamped to a sane range so a tiny window still yields a
 * usable (if overlapped) pane instead of a negative width.
 */
export function terminalMax(available: number): number {
  if (!Number.isFinite(available)) return TERMINAL_MAX;
  return Math.max(TERMINAL_MIN, Math.min(TERMINAL_MAX, Math.floor(available)));
}

/** Clamp a stored or dragged width into [TERMINAL_MIN, terminalMax(available)]. */
export function clampTerminalWidth(width: number, available: number): number {
  if (!Number.isFinite(width)) return TERMINAL_DEFAULT;
  return Math.round(Math.min(terminalMax(available), Math.max(TERMINAL_MIN, width)));
}

/** Space a pane may use: container minus the panes left of it and the chat minimum. */
export function terminalSpace(container: number, reserve: number): number {
  return terminalMax(container - reserve - SPLITTER_WIDTH - CHAT_MIN);
}

export function pref(key: string): string | null {
  try { return localStorage.getItem(key); } catch { return null; }
}

export function setPref(key: string, value: string) {
  try { localStorage.setItem(key, value); } catch { /* storage unavailable */ }
}

/** A stored terminal width, or the default when absent/corrupt. */
export function storedWidth(key: string): number {
  const raw = pref(key);
  const n = raw === null ? NaN : Number(raw);
  return Number.isFinite(n) && n > 0 ? n : TERMINAL_DEFAULT;
}

export function storedFlag(key: string): boolean {
  return pref(key) === "1";
}

export function setFlag(key: string, on: boolean) {
  setPref(key, on ? "1" : "0");
}
