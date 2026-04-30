import { useState } from "react";
import type { Todo } from "../server";
import { listTodos } from "../api";

export function App({ navigate }: { navigate: (to: string) => void }) {
  // Phase 5 — hooks-on-function pattern (§10):
  //
  //   import { listTodos } from "../api";
  //   listTodos.useQuery({ limit: 50 })
  //
  // The same `listTodos` is callable directly (`await listTodos(...)`)
  // for non-React contexts, but inside a component the `.useQuery`
  // hook gives us caching, automatic retries on retryable errors, and
  // SSR-friendly hydration once the SSR transform lands. No more
  // manual `useEffect` + `useState` glue.
  const { data: todos = [], isLoading } = listTodos.useQuery({ limit: 50 });
  const [localTodos, setLocalTodos] = useState<Todo[]>([]);

  const addLocal = () => {
    setLocalTodos((prev) => [
      ...prev,
      {
        id: todos.length + prev.length + 1,
        text: `Local todo ${todos.length + prev.length + 1}`,
        done: false,
      },
    ]);
  };
  const allTodos: Todo[] = [...todos, ...localTodos];
  const loading = isLoading;

  return (
    <main style={styles.wrap}>
      <h1>csr-todo</h1>
      <p style={styles.subtitle}>Client-side rendered SPA · 1 RPC endpoint</p>
      <p>{loading ? "loading…" : `${allTodos.length} todos`}</p>
      <ul style={styles.list}>
        {allTodos.map((t) => (
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
