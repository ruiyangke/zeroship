// ─── Conversation persistence (localStorage MVP) ────────────────
//
// Keyed by appId so each project has its own thread. Capped at the
// last 100 messages per project to keep page-load fast and the
// localStorage budget under control. Upgrade path: move to
// @zeroship/db once the editor has its own backend tables.

import type { ChatMessage } from "./types";

const PREFIX = "zeroship_builder_chat_";
const MAX_MESSAGES = 100;

export function loadConversation(appId: string): ChatMessage[] {
  try {
    const raw = localStorage.getItem(PREFIX + appId);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed as ChatMessage[];
  } catch {
    return [];
  }
}

export function saveConversation(appId: string, messages: ChatMessage[]): void {
  try {
    const trimmed = messages.length > MAX_MESSAGES
      ? messages.slice(messages.length - MAX_MESSAGES)
      : messages;
    localStorage.setItem(PREFIX + appId, JSON.stringify(trimmed));
  } catch {
    // localStorage is full or unavailable — silent best-effort.
  }
}

export function clearConversation(appId: string): void {
  try { localStorage.removeItem(PREFIX + appId); } catch {}
}
