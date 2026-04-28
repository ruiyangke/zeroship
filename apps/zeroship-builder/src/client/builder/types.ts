// ─── Builder types ──────────────────────────────────────────────
//
// Wire types for the agent SSE protocol + per-message state in the
// chat. Kept as plain interfaces so React state updates stay shallow
// and serialize cleanly into localStorage.

export type Role = "user" | "assistant" | "system";

export interface ToolEvent {
  /** Unique id assigned by the chat layer; lets React reconcile streams. */
  id: string;
  name: string;
  /** Input shown when the call started (parsed JSON if possible). */
  input?: unknown;
  /** Result captured when the tool finished. */
  output?: string;
  /** Set true once tool_end fires; until then we render a spinner. */
  done: boolean;
  /** Set true if the tool raised an error. */
  error?: boolean;
}

export interface ChatMessage {
  id: string;
  role: Role;
  /** Plain text content; we accumulate streaming deltas here. */
  content: string;
  /** Tool calls associated with an assistant turn, in order. */
  tools?: ToolEvent[];
  /** Wall-clock when the message was first surfaced. */
  createdAt: number;
}

/** Wire shape from the agent's `/chat` SSE endpoint. */
export type AgentEvent =
  | { type: "text"; content: string }
  | { type: "tool_start"; name: string; input?: unknown }
  | { type: "tool_end"; name: string; output?: string; error?: boolean }
  | { type: "done" }
  | { type: "error"; content: string };

export type BuilderStatus =
  | "idle"
  | "thinking"
  | "calling-tool"
  | "deploying"
  | "error";
