import { useState, useRef, useEffect } from "react";
import { chat } from "./index";

interface ChatMsg {
  role: "user" | "assistant";
  content: string;
}

export function App() {
  const [messages, setMessages] = useState<ChatMsg[]>([]);
  const [input, setInput] = useState("");
  const [loading, setLoading] = useState(false);
  const [firstTokenMs, setFirstTokenMs] = useState<number | null>(null);
  const bottomRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages]);

  const send = async (e: React.FormEvent) => {
    e.preventDefault();
    const text = input.trim();
    if (!text || loading) return;

    setMessages((prev) => [...prev, { role: "user", content: text }]);
    setInput("");
    setLoading(true);
    setFirstTokenMs(null);

    // Placeholder assistant bubble that we fill incrementally.
    setMessages((prev) => [...prev, { role: "assistant", content: "" }]);

    const started = performance.now();
    let firstToken = true;
    let content = "";

    try {
      // `chat` is the client stub produced by @zeroship/vite-plugin:
      // an async iterable that streams `{ token: string }` events.
      for await (const ev of chat(text, messages)) {
        if (firstToken) {
          setFirstTokenMs(Math.round(performance.now() - started));
          firstToken = false;
        }
        if (typeof ev.token !== "string") continue;

        content += ev.token;
        setMessages((prev) => {
          const updated = [...prev];
          updated[updated.length - 1] = { role: "assistant", content };
          return updated;
        });
      }
    } catch (e: any) {
      setMessages((prev) => {
        const updated = [...prev];
        updated[updated.length - 1] = {
          role: "assistant",
          content: `Error: ${e?.message ?? e}`,
        };
        return updated;
      });
    } finally {
      setLoading(false);
      inputRef.current?.focus();
    }
  };

  return (
    <div style={styles.container}>
      <div style={styles.card}>
        <div style={styles.header}>
          <h1 style={styles.title}>OpenAI Stream</h1>
          <span style={styles.badge}>zeroship V8 + openai^6</span>
        </div>
        <div style={styles.subhead}>
          <code>gpt-5.4-mini</code> · async generator · URL-path RPC
          {firstTokenMs != null && (
            <span style={styles.metric}> · first token {firstTokenMs} ms</span>
          )}
        </div>

        <div style={styles.messages}>
          {messages.length === 0 && (
            <div style={styles.empty}>
              Ask anything. Tokens stream in as the model generates them.
            </div>
          )}
          {messages.map((msg, i) => (
            <div
              key={i}
              style={{
                ...styles.message,
                ...(msg.role === "user" ? styles.userMsg : styles.assistantMsg),
              }}
            >
              <div style={styles.msgRole}>
                {msg.role === "user" ? "You" : "AI"}
              </div>
              <div style={styles.msgContent}>
                {msg.content ||
                  (loading && i === messages.length - 1 ? "…" : "")}
              </div>
            </div>
          ))}
          <div ref={bottomRef} />
        </div>

        <form onSubmit={send} style={styles.form}>
          <input
            ref={inputRef}
            value={input}
            onChange={(e) => setInput(e.target.value)}
            placeholder="Type a message…"
            style={styles.input}
            disabled={loading}
            autoFocus
          />
          <button
            type="submit"
            style={styles.sendBtn}
            disabled={!input.trim() || loading}
          >
            {loading ? "…" : "Send"}
          </button>
        </form>
      </div>
    </div>
  );
}

const styles: Record<string, React.CSSProperties> = {
  container: {
    minHeight: "100vh",
    background: "linear-gradient(135deg, #0f172a, #1e1b4b, #312e81)",
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    fontFamily:
      '-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif',
    padding: 20,
  },
  card: {
    background: "#1a1a2e",
    borderRadius: 16,
    padding: "24px",
    width: "100%",
    maxWidth: 640,
    height: "85vh",
    display: "flex",
    flexDirection: "column",
    boxShadow: "0 20px 60px rgba(0,0,0,0.5)",
    border: "1px solid rgba(255,255,255,0.06)",
  },
  header: {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    marginBottom: 4,
  },
  title: {
    margin: 0,
    fontSize: 24,
    fontWeight: 300,
    color: "#e0e0e0",
    letterSpacing: 2,
  },
  badge: {
    fontSize: 10,
    padding: "3px 8px",
    borderRadius: 20,
    background: "rgba(16,185,129,0.15)",
    color: "#34d399",
    border: "1px solid rgba(16,185,129,0.3)",
  },
  subhead: {
    fontSize: 12,
    color: "rgba(255,255,255,0.35)",
    marginBottom: 16,
  },
  metric: {
    color: "#34d399",
  },
  messages: {
    flex: 1,
    overflowY: "auto",
    marginBottom: 16,
    display: "flex",
    flexDirection: "column",
    gap: 12,
  },
  empty: {
    textAlign: "center",
    color: "rgba(255,255,255,0.25)",
    padding: "60px 20px",
    fontSize: 14,
    lineHeight: 1.6,
  },
  message: {
    padding: "10px 14px",
    borderRadius: 12,
    maxWidth: "85%",
    fontSize: 14,
    lineHeight: 1.5,
  },
  userMsg: {
    alignSelf: "flex-end",
    background: "#6366f1",
    color: "#fff",
  },
  assistantMsg: {
    alignSelf: "flex-start",
    background: "rgba(255,255,255,0.06)",
    color: "#d1d5db",
    border: "1px solid rgba(255,255,255,0.08)",
  },
  msgRole: {
    fontSize: 10,
    fontWeight: 600,
    marginBottom: 4,
    opacity: 0.6,
    textTransform: "uppercase",
  },
  msgContent: {
    whiteSpace: "pre-wrap",
    wordBreak: "break-word",
  },
  form: {
    display: "flex",
    gap: 8,
  },
  input: {
    flex: 1,
    padding: "12px 16px",
    borderRadius: 10,
    border: "1px solid rgba(255,255,255,0.1)",
    background: "rgba(255,255,255,0.05)",
    color: "#e0e0e0",
    fontSize: 14,
    outline: "none",
  },
  sendBtn: {
    padding: "12px 20px",
    borderRadius: 10,
    border: "none",
    background: "#10b981",
    color: "#fff",
    fontSize: 14,
    fontWeight: 600,
    cursor: "pointer",
  },
};
