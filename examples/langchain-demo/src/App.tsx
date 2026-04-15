import { useState, useRef, useEffect } from "react";
import { chat } from "./index";

interface ChatMsg { role: string; content: string }

export function App() {
  const [messages, setMessages] = useState<ChatMsg[]>([]);
  const [input, setInput] = useState("");
  const [loading, setLoading] = useState(false);
  const bottomRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages]);

  const send = async (e: React.FormEvent) => {
    e.preventDefault();
    const text = input.trim();
    if (!text || loading) return;

    const userMsg: ChatMsg = { role: "user", content: text };
    const history = [...messages, userMsg];
    setMessages(history);
    setInput("");
    setLoading(true);

    try {
      const result = await chat(text, messages);
      setMessages((prev) => [...prev, result]);
    } catch (e: any) {
      setMessages((prev) => [...prev, { role: "assistant", content: `Error: ${e.message}` }]);
    }

    setLoading(false);
    inputRef.current?.focus();
  };

  return (
    <div style={styles.container}>
      <div style={styles.card}>
        <div style={styles.header}>
          <h1 style={styles.title}>AI Chatbot</h1>
          <span style={styles.badge}>LangChain + zeroship V8</span>
        </div>

        <div style={styles.tools}>
          Tools: <code>calculator</code> <code>weather</code> <code>datetime</code>
        </div>

        <div style={styles.messages}>
          {messages.length === 0 && (
            <div style={styles.empty}>
              Ask me anything! I can do math, check weather, and tell time.
              <br /><br />
              Try: "What's the weather in Tokyo and what's 42 * 17?"
            </div>
          )}
          {messages.map((msg, i) => (
            <div key={i} style={{ ...styles.message, ...(msg.role === "user" ? styles.userMsg : styles.assistantMsg) }}>
              <div style={styles.msgRole}>{msg.role === "user" ? "You" : "AI"}</div>
              <div style={styles.msgContent}>
                {msg.content || (loading && i === messages.length - 1 ? "Thinking..." : "")}
              </div>
            </div>
          ))}
          {loading && (
            <div style={{ ...styles.message, ...styles.assistantMsg }}>
              <div style={styles.msgRole}>AI</div>
              <div style={{ ...styles.msgContent, opacity: 0.5 }}>Thinking...</div>
            </div>
          )}
          <div ref={bottomRef} />
        </div>

        <form onSubmit={send} style={styles.form}>
          <input
            ref={inputRef}
            value={input}
            onChange={(e) => setInput(e.target.value)}
            placeholder="Type a message..."
            style={styles.input}
            disabled={loading}
            autoFocus
          />
          <button type="submit" style={styles.sendBtn} disabled={!input.trim() || loading}>
            {loading ? "..." : "Send"}
          </button>
        </form>

        <div style={styles.powered}>
          Powered by <strong>zeroship</strong> V8 runtime + LangChain + OpenAI gpt-4.1-mini
        </div>
      </div>
    </div>
  );
}

const styles: Record<string, React.CSSProperties> = {
  container: {
    minHeight: "100vh",
    background: "linear-gradient(135deg, #0f0c29, #302b63, #24243e)",
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    fontFamily: '-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif',
    padding: 20,
  },
  card: {
    background: "#1a1a2e",
    borderRadius: 16,
    padding: "24px",
    width: "100%",
    maxWidth: 600,
    height: "85vh",
    display: "flex",
    flexDirection: "column",
    boxShadow: "0 20px 60px rgba(0,0,0,0.5)",
    border: "1px solid rgba(255,255,255,0.06)",
  },
  header: { display: "flex", alignItems: "center", justifyContent: "space-between", marginBottom: 8 },
  title: { margin: 0, fontSize: 24, fontWeight: 300, color: "#e0e0e0", letterSpacing: 2 },
  badge: { fontSize: 10, padding: "3px 8px", borderRadius: 20, background: "rgba(16,185,129,0.15)", color: "#34d399", border: "1px solid rgba(16,185,129,0.3)" },
  tools: { fontSize: 12, color: "rgba(255,255,255,0.3)", marginBottom: 16 },
  messages: { flex: 1, overflowY: "auto" as const, marginBottom: 16, display: "flex", flexDirection: "column" as const, gap: 12 },
  empty: { textAlign: "center" as const, color: "rgba(255,255,255,0.25)", padding: "60px 20px", fontSize: 14, lineHeight: 1.6 },
  message: { padding: "10px 14px", borderRadius: 12, maxWidth: "85%", fontSize: 14, lineHeight: 1.5 },
  userMsg: { alignSelf: "flex-end", background: "#6366f1", color: "#fff" },
  assistantMsg: { alignSelf: "flex-start", background: "rgba(255,255,255,0.06)", color: "#d1d5db", border: "1px solid rgba(255,255,255,0.08)" },
  msgRole: { fontSize: 10, fontWeight: 600, marginBottom: 4, opacity: 0.6, textTransform: "uppercase" as const },
  msgContent: { whiteSpace: "pre-wrap" as const, wordBreak: "break-word" as const },
  form: { display: "flex", gap: 8 },
  input: { flex: 1, padding: "12px 16px", borderRadius: 10, border: "1px solid rgba(255,255,255,0.1)", background: "rgba(255,255,255,0.05)", color: "#e0e0e0", fontSize: 14, outline: "none" },
  sendBtn: { padding: "12px 20px", borderRadius: 10, border: "none", background: "#10b981", color: "#fff", fontSize: 14, fontWeight: 600, cursor: "pointer" },
  powered: { marginTop: 12, textAlign: "center" as const, fontSize: 11, color: "rgba(255,255,255,0.2)" },
};
