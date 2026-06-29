import { useEffect, useState } from "react";
import type { Message } from "./api";
import { addMessage, getMessages } from "./api";

export function App() {
  const [messages, setMessages] = useState<Message[]>([]);
  const [text, setText] = useState("");
  const [loading, setLoading] = useState(true);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = async () => {
    const next = await getMessages();
    setMessages(next);
  };

  useEffect(() => {
    void refresh()
      .catch((err) => setError(err instanceof Error ? err.message : String(err)))
      .finally(() => setLoading(false));
  }, []);

  const submit = async (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const trimmed = text.trim();
    if (!trimmed) return;

    setSubmitting(true);
    setError(null);
    try {
      await addMessage({ text: trimmed });
      setText("");
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <main style={styles.wrap}>
      <h1>zeroship starter</h1>
      <form onSubmit={submit} style={styles.form}>
        <input
          value={text}
          onChange={(event) => setText(event.currentTarget.value)}
          placeholder="Write a message"
          maxLength={280}
          style={styles.input}
        />
        <button disabled={submitting || text.trim().length === 0} style={styles.button}>
          {submitting ? "Adding..." : "Add"}
        </button>
      </form>
      {error ? <p style={styles.error}>{error}</p> : null}
      {loading ? <p>Loading...</p> : null}
      <ul style={styles.list}>
        {messages.map((message) => (
          <li key={message.id} style={styles.item}>
            <span>{message.text}</span>
            <time dateTime={new Date(message.createdAt).toISOString()} style={styles.time}>
              {new Date(message.createdAt).toLocaleTimeString()}
            </time>
          </li>
        ))}
      </ul>
    </main>
  );
}

const styles: Record<string, React.CSSProperties> = {
  wrap: { maxWidth: 560, margin: "48px auto", padding: "0 16px", fontFamily: "system-ui, sans-serif" },
  form: { display: "flex", gap: 8 },
  input: { flex: 1, padding: "10px 12px", border: "1px solid #ccc", borderRadius: 6, font: "inherit" },
  button: { padding: "10px 16px", border: "1px solid #222", borderRadius: 6, background: "#222", color: "#fff" },
  error: { color: "#b00020" },
  list: { listStyle: "none", padding: 0, marginTop: 24 },
  item: { display: "flex", justifyContent: "space-between", gap: 16, padding: "10px 0", borderBottom: "1px solid #eee" },
  time: { color: "#666", fontSize: 13, whiteSpace: "nowrap" },
};
