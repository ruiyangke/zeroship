import { useState, useEffect, useRef } from "react";
import { listTodos, addTodo, toggleTodo, deleteTodo } from "./index";

interface Todo {
  id: number;
  text: string;
  done: boolean;
}

export function App() {
  const [todos, setTodos] = useState<Todo[]>([]);
  const [input, setInput] = useState("");
  const [loading, setLoading] = useState(true);
  const inputRef = useRef<HTMLInputElement>(null);

  const refresh = async () => {
    setTodos(await listTodos());
    setLoading(false);
  };

  useEffect(() => { refresh(); }, []);

  const handleAdd = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!input.trim()) return;
    await addTodo(input.trim());
    setInput("");
    inputRef.current?.focus();
    await refresh();
  };

  const handleToggle = async (id: number) => {
    await toggleTodo(id);
    await refresh();
  };

  const handleDelete = async (id: number) => {
    await deleteTodo(id);
    await refresh();
  };

  const remaining = todos.filter((t) => !t.done).length;

  return (
    <div style={styles.container}>
      <div style={styles.card}>
        <div style={styles.header}>
          <h1 style={styles.title}>todos</h1>
          <span style={styles.badge}>zeroship + vite</span>
        </div>

        <form onSubmit={handleAdd} style={styles.form}>
          <input
            ref={inputRef}
            value={input}
            onChange={(e) => setInput(e.target.value)}
            placeholder="What needs to be done?"
            style={styles.input}
            autoFocus
          />
          <button type="submit" style={styles.addBtn} disabled={!input.trim()}>
            Add
          </button>
        </form>

        {loading ? (
          <p style={styles.empty}>Loading...</p>
        ) : todos.length === 0 ? (
          <p style={styles.empty}>No todos yet. Add one above!</p>
        ) : (
          <ul style={styles.list}>
            {todos.map((todo) => (
              <li key={todo.id} style={styles.item}>
                <button
                  onClick={() => handleToggle(todo.id)}
                  style={{
                    ...styles.checkbox,
                    ...(todo.done ? styles.checkboxDone : {}),
                  }}
                >
                  {todo.done ? "✓" : ""}
                </button>
                <span
                  style={{
                    ...styles.text,
                    ...(todo.done ? styles.textDone : {}),
                  }}
                >
                  {todo.text}
                </span>
                <button
                  onClick={() => handleDelete(todo.id)}
                  style={styles.deleteBtn}
                >
                  ×
                </button>
              </li>
            ))}
          </ul>
        )}

        {todos.length > 0 && (
          <div style={styles.footer}>
            <span>{remaining} item{remaining !== 1 ? "s" : ""} left</span>
            <span style={styles.footerRight}>{todos.length - remaining} done</span>
          </div>
        )}

        <div style={styles.powered}>
          Powered by <strong>zeroship</strong> V8 runtime — "use server" RPC
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
    margin: 0,
    padding: 20,
  },
  card: {
    background: "#1a1a2e",
    borderRadius: 16,
    padding: "32px 28px",
    width: "100%",
    maxWidth: 480,
    boxShadow: "0 20px 60px rgba(0,0,0,0.5)",
    border: "1px solid rgba(255,255,255,0.06)",
  },
  header: {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    marginBottom: 24,
  },
  title: {
    margin: 0,
    fontSize: 32,
    fontWeight: 200,
    color: "#e0e0e0",
    letterSpacing: 8,
  },
  badge: {
    fontSize: 11,
    padding: "4px 10px",
    borderRadius: 20,
    background: "rgba(99, 102, 241, 0.15)",
    color: "#818cf8",
    border: "1px solid rgba(99, 102, 241, 0.3)",
  },
  form: { display: "flex", gap: 8, marginBottom: 20 },
  input: {
    flex: 1,
    padding: "12px 16px",
    borderRadius: 10,
    border: "1px solid rgba(255,255,255,0.1)",
    background: "rgba(255,255,255,0.05)",
    color: "#e0e0e0",
    fontSize: 15,
    outline: "none",
  },
  addBtn: {
    padding: "12px 20px",
    borderRadius: 10,
    border: "none",
    background: "#6366f1",
    color: "#fff",
    fontSize: 14,
    fontWeight: 600,
    cursor: "pointer",
  },
  list: { listStyle: "none", margin: 0, padding: 0 },
  item: {
    display: "flex",
    alignItems: "center",
    gap: 12,
    padding: "12px 0",
    borderBottom: "1px solid rgba(255,255,255,0.05)",
  },
  checkbox: {
    width: 24,
    height: 24,
    borderRadius: 6,
    border: "2px solid rgba(255,255,255,0.2)",
    background: "transparent",
    color: "transparent",
    fontSize: 14,
    cursor: "pointer",
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    flexShrink: 0,
  },
  checkboxDone: {
    background: "#6366f1",
    borderColor: "#6366f1",
    color: "#fff",
  },
  text: { flex: 1, fontSize: 15, color: "#d1d5db" },
  textDone: { textDecoration: "line-through", color: "rgba(255,255,255,0.3)" },
  deleteBtn: {
    width: 28,
    height: 28,
    borderRadius: 6,
    border: "none",
    background: "transparent",
    color: "rgba(255,255,255,0.3)",
    fontSize: 18,
    cursor: "pointer",
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
  },
  empty: {
    textAlign: "center" as const,
    color: "rgba(255,255,255,0.3)",
    padding: "32px 0",
    fontSize: 14,
  },
  footer: {
    display: "flex",
    justifyContent: "space-between",
    paddingTop: 16,
    fontSize: 13,
    color: "rgba(255,255,255,0.3)",
  },
  footerRight: { color: "rgba(99, 102, 241, 0.6)" },
  powered: {
    marginTop: 24,
    textAlign: "center" as const,
    fontSize: 11,
    color: "rgba(255,255,255,0.2)",
  },
};
