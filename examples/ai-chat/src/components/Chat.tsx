// Chat UI built on `@ai-sdk/react`'s `useChat` hook.
//
// `useChat` owns the message-list state, the input box state, and the
// streaming HTTP fetch. We only have to:
//   - Configure the transport (URL + body shape).
//   - Render `messages` as they arrive — text-delta frames extend the
//     last assistant message in real time.
import { useState } from "react";
import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";

export function Chat() {
  // Local input state — useChat in v5 no longer manages this for you.
  const [input, setInput] = useState("");

  const { messages, sendMessage, status, error, stop } = useChat({
    transport: new DefaultChatTransport({
      // Hit our streaming RPC procedure directly.
      api: "/_zs/v1/chat",

      // The kernel's RPC fast path expects `{ json: <input> }` — wrap
      // the SDK's default `{ messages, ... }` shape in our envelope.
      // `prepareSendMessagesRequest` is the v5 hook for exactly this.
      prepareSendMessagesRequest: ({ messages, id }) => ({
        body: { json: { messages, id } },
      }),
    }),

    // Surface streaming/network errors in the console for debugging.
    onError: (err) => console.error("[chat]", err),
  });

  const isStreaming = status === "submitted" || status === "streaming";

  return (
    <main style={styles.page}>
      <header style={styles.header}>
        <h1 style={styles.title}>AI Chat</h1>
        <p style={styles.subtitle}>
          <code>useChat()</code> from <code>@ai-sdk/react</code> talking to a
          zeroship streaming RPC procedure.
        </p>
      </header>

      <ol style={styles.thread}>
        {messages.length === 0 ? (
          <li style={styles.empty}>Say hi to start a conversation.</li>
        ) : null}
        {messages.map((m) => (
          <li
            key={m.id}
            style={{
              ...styles.message,
              ...(m.role === "user" ? styles.user : styles.assistant),
            }}
          >
            <div style={styles.role}>{m.role}</div>
            <div style={styles.content}>
              {m.parts.map((part, i) => {
                // The v5 message shape carries content under `parts`.
                // Text deltas land here as `{ type: "text", text }`.
                if (part.type === "text") {
                  return <span key={i}>{part.text}</span>;
                }
                // We don't emit reasoning / tool / file parts in this
                // demo; if a real provider does, the chat would still
                // render readable output by ignoring them gracefully.
                return null;
              })}
            </div>
          </li>
        ))}
      </ol>

      {error ? <div style={styles.error}>error: {error.message}</div> : null}

      <form
        style={styles.form}
        onSubmit={(e) => {
          e.preventDefault();
          const text = input.trim();
          if (!text || isStreaming) return;
          sendMessage({ text });
          setInput("");
        }}
      >
        <input
          style={styles.input}
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder={isStreaming ? "Generating reply…" : "Ask anything…"}
          disabled={isStreaming}
          autoFocus
        />
        {isStreaming ? (
          <button type="button" style={styles.stop} onClick={stop}>
            Stop
          </button>
        ) : (
          <button type="submit" style={styles.send} disabled={!input.trim()}>
            Send
          </button>
        )}
      </form>
    </main>
  );
}

// Inline styles keep the demo dependency-free. Swap for Tailwind / CSS
// modules in real apps.
const styles = {
  page: {
    maxWidth: 720,
    margin: "0 auto",
    padding: "32px 16px",
    fontFamily: "ui-sans-serif, system-ui, sans-serif",
    color: "#0f172a",
  },
  header: { marginBottom: 24 },
  title: { fontSize: 24, fontWeight: 600, margin: 0 },
  subtitle: { color: "#64748b", marginTop: 4, marginBottom: 0, fontSize: 14 },
  thread: {
    listStyle: "none",
    padding: 0,
    margin: 0,
    display: "flex",
    flexDirection: "column",
    gap: 12,
    minHeight: 240,
    marginBottom: 16,
  },
  empty: { color: "#94a3b8", textAlign: "center", padding: 48 },
  message: {
    padding: "10px 14px",
    borderRadius: 10,
    border: "1px solid #e2e8f0",
    background: "#f8fafc",
  },
  user: { borderColor: "#bfdbfe", background: "#eff6ff" },
  assistant: { borderColor: "#bbf7d0", background: "#f0fdf4" },
  role: {
    fontSize: 11,
    textTransform: "uppercase" as const,
    letterSpacing: 0.5,
    color: "#475569",
    marginBottom: 4,
  },
  content: { whiteSpace: "pre-wrap" as const, lineHeight: 1.5 },
  error: {
    padding: "8px 12px",
    borderRadius: 6,
    background: "#fef2f2",
    color: "#b91c1c",
    border: "1px solid #fecaca",
    fontSize: 13,
    marginBottom: 12,
  },
  form: { display: "flex", gap: 8 },
  input: {
    flex: 1,
    padding: "10px 12px",
    border: "1px solid #cbd5e1",
    borderRadius: 8,
    fontSize: 14,
    outline: "none",
  },
  send: {
    padding: "10px 16px",
    background: "#2563eb",
    color: "white",
    border: 0,
    borderRadius: 8,
    cursor: "pointer",
    fontSize: 14,
    fontWeight: 500,
  },
  stop: {
    padding: "10px 16px",
    background: "#ef4444",
    color: "white",
    border: 0,
    borderRadius: 8,
    cursor: "pointer",
    fontSize: 14,
    fontWeight: 500,
  },
} as const;
