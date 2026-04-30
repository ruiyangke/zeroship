import { useEffect, useState } from "react";
import { listTodos, type Todo } from "../server";

export function App({ navigate }: { navigate: (to: string) => void }) {
  const [todos, setTodos] = useState<Todo[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    // `listTodos` is the build-time RPC stub — the actual function
    // runs in the V8 worker, this resolves with the JSON response.
    listTodos()
      .then((rows) => setTodos(rows))
      .finally(() => setLoading(false));
  }, []);

  const addLocal = () => {
    // Optimistic UI for the demo — no backing persistence.
    setTodos((prev) => [
      ...prev,
      { id: prev.length + 1, text: `Local todo ${prev.length + 1}`, done: false },
    ]);
  };

  return (
    <main style={styles.wrap}>
      <h1>csr-todo</h1>
      <p style={styles.subtitle}>Client-side rendered SPA · 1 RPC endpoint</p>
      <p>{loading ? "loading…" : `${todos.length} todos`}</p>
      <ul style={styles.list}>
        {todos.map((t) => (
          <li key={t.id} style={styles.item}>
            <span style={t.done ? styles.done : undefined}>{t.text}</span>
          </li>
        ))}
      </ul>
      <button onClick={addLocal} style={styles.btn}>Add local</button>
      <p>
        <a href="/about" onClick={(e) => { e.preventDefault(); navigate("/about"); }}>
          About this demo
        </a>
      </p>
    </main>
  );
}

const styles: Record<string, React.CSSProperties> = {
  wrap:     { fontFamily: "system-ui, sans-serif", maxWidth: 480, margin: "40px auto", padding: "0 16px" },
  subtitle: { color: "#666", marginTop: -8 },
  list:     { listStyle: "none", padding: 0 },
  item:     { padding: "6px 0", borderBottom: "1px solid #eee" },
  done:     { textDecoration: "line-through", color: "#999" },
  btn:      { padding: "8px 14px", border: "1px solid #ccc", background: "#fafafa", borderRadius: 6, cursor: "pointer" },
};
