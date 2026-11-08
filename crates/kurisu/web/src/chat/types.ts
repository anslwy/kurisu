// Payloads of the /web endpoints (crates/kurisu/src/chat.rs). Kept in one
// place so the components never reach for `any`: the same shapes are asserted
// by the Rust handlers and the SSE stream.

/** Where a session is right now, as the sidebar shows it. */
export type LiveState = "working" | "idle" | "paused" | "archived";

/** What the running tool reports while a turn is working. */
export interface Activity {
  tool?: string;
  title?: string;
  state?: "running" | "done" | string;
  at?: number;
}

export interface LastMessage {
  seq: number;
  role: string;
  text: string;
  at: number;
}

/** One row of `GET /_web/api/sessions` (the server's `peers` output). */
export interface Session {
  id: string;
  name: string;
  role: "lead" | "worker";
  harness: string;
  cwd: string;
  model: string;
  effort: string | null;
  status: string;
  lifecycle: "active" | "paused" | "archived";
  description: string;
  note: string;
  created_by_session: string | null;
  created_by_name: string | null;
  last_activity: number | null;
  created_at: number;
  children?: { id: string; name: string }[];
  replies?: number;
  last?: LastMessage | null;
  activity?: Activity | null;
}

/** A history row. `role` is user | agent | tool | error | approval | compaction,
 * or the id of the session that sent it (a peer). */
export interface Message {
  seq: number;
  session: string;
  role: string;
  text: string;
  at: number;
  data?: ToolData | null;
}

export interface ToolData {
  tool?: string;
  tool_call_id?: string;
  status?: string;
  input?: unknown;
  output?: unknown;
}

export interface HistoryPage {
  session: { id: string; name: string; harness: string; model: string };
  total: number;
  messages: Message[];
  has_more: boolean;
  direction: "after" | "before";
  first_seq: number | null;
  last_seq: number | null;
}

export interface SessionsResponse {
  sessions: Session[];
  default: string | null;
  archived: boolean;
}

export interface SendResult {
  id: string;
  name: string;
  seq: number;
  queued: boolean;
  steered: boolean;
}

export interface InterruptResult {
  id: string;
  name: string;
  interrupted: boolean;
  stopped: boolean;
  reason?: string;
}

/** A tool call still in flight (or just finished) in the selected session. */
export interface LiveTool {
  kind: "tool";
  tool_call_id: string;
  tool: string;
  title?: string;
  status: string;
  input?: unknown;
  output?: unknown;
  seq?: number | null;
}

export interface LiveText {
  kind: "text";
  text: string;
}

export type LiveBlock = LiveText | LiveTool;

export interface LiveTurn {
  session: string;
  blocks: LiveBlock[];
  state: "working" | "idle" | "stopped" | "error";
}

export interface ApiErrorBody {
  error: string;
}

/** The events `GET /_web/stream` sends. */
export type StreamEvent =
  | { type: "hello"; sessions: Session[]; default: string | null }
  | { type: "sessions"; sessions: Session[] }
  | { type: "message"; message: Message }
  | { type: "delta"; session: string; kind: "text"; text: string }
  | {
      type: "tool";
      session: string;
      tool_call_id: string;
      tool?: string;
      title?: string;
      status?: string;
      input?: unknown;
      output?: unknown;
      seq?: number | null;
    }
  | { type: "turn"; session: string; status: "working" | "idle" | "stopped" | "error" }
  | { type: "resync"; server?: string }
  | { type: "closed" };

/** How a row is drawn. Peer messages carry the sender's session id as `role`. */
export type BubbleKind = "user" | "agent" | "peer" | "tool" | "error" | "approval" | "compaction";

const KNOWN: Record<string, BubbleKind> = {
  user: "user",
  agent: "agent",
  tool: "tool",
  error: "error",
  approval: "approval",
  compaction: "compaction",
};

export function bubbleKind(role: string): BubbleKind {
  return KNOWN[role] ?? "peer";
}

/** working / idle / paused / archived, from the fields the server sends. */
export function liveState(s: Session): LiveState {
  if (s.lifecycle === "archived") return "archived";
  if (s.activity && s.activity.state === "running") return "working";
  if (s.status === "working") return "working";
  if (s.lifecycle === "paused") return "paused";
  return "idle";
}

export function statusLabel(s: Session): string {
  const st = liveState(s);
  return st === "working" && s.activity?.tool ? `working · ${s.activity.tool}` : st;
}
