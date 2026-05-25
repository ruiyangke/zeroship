import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import type { Todo } from "../api";
import { listTodos, searchTodos } from "../api";

const TODOS_QUERY_KEY = ["todos", "list", 50] as const;

export function App({ navigate }: { navigate: (to: string) => void }) {
  const queryClient = useQueryClient();
  const { data: todos = [], isLoading } = useQuery({
    queryKey: TODOS_QUERY_KEY,
    queryFn: () => listTodos({ limit: 50 }),
  });
  const [localTodos, setLocalTodos] = useState<Todo[]>([]);
  const [search, setSearch] = useState("build");
  const [streamedTodos, setStreamedTodos] = useState<Todo[]>([]);
  const [isStreaming, setIsStreaming] = useState(false);
  const [streamError, setStreamError] = useState<string | null>(null);

  useEffect(() => {
    const q = search.trim();
    if (!q) {
      setStreamedTodos([]);
      setIsStreaming(false);
      setStreamError(null);
      return;
    }

    let cancelled = false;
    let iterator: AsyncIterator<Todo> | undefined;
    setStreamedTodos([]);
    setIsStreaming(true);
    setStreamError(null);

    void (async () => {
      try {
        iterator = searchTodos({ query: q })[Symbol.asyncIterator]();
        while (!cancelled) {
          const next = await iterator.next();
          if (next.done) break;
          setStreamedTodos((prev) => [...prev, next.value]);
        }
      } catch (err) {
        if (!cancelled) {
          setStreamError(err instanceof Error ? err.message : String(err));
        }
      } finally {
        if (!cancelled) setIsStreaming(false);
      }
    })();

    return () => {
      cancelled = true;
      setIsStreaming(false);
      void iterator?.return?.();
    };
  }, [search]);

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
      <p style={styles.subtitle}>Client-side rendered SPA · typed RPC endpoints</p>
      <p>{loading ? "loading…" : `${allTodos.length} todos`}</p>
      <ul style={styles.list}>
        {allTodos.map((t) => (
          <li key={t.id} style={styles.item}>
            <span style={t.done ? styles.done : undefined}>{t.text}</span>
          </li>
        ))}
      </ul>
      <div style={styles.actions}>
        <button onClick={addLocal} style={styles.btn}>Add local</button>
        <button
          onClick={() => void queryClient.invalidateQueries({ queryKey: TODOS_QUERY_KEY })}
          style={styles.btn}
        >
          Refresh RPC
        </button>
      </div>
      <section style={styles.streamBox}>
        <label style={styles.label}>
          Search stream
          <input
            value={search}
            onChange={(e) => setSearch(e.currentTarget.value)}
            style={styles.input}
          />
        </label>
        <p style={styles.streamStatus}>
          {isStreaming ? "streaming…" : `${streamedTodos.length} matches`}
          {streamError ? ` · ${streamError}` : ""}
        </p>
        <ul style={styles.list}>
          {streamedTodos.map((t) => (
            <li key={`stream-${t.id}`} style={styles.item}>
              <span style={t.done ? styles.done : undefined}>{t.text}</span>
            </li>
          ))}
        </ul>
      </section>
      <p>
        <a href="/about" onClick={(e) => { e.preventDefault(); navigate("/about"); }}>
          About this demo
        </a>
      </p>
    </main>
  );
}

const styles: Record<string, React.CSSProperties> = {
  wrap: { fontFamily: "system-ui, sans-serif", maxWidth: 480, margin: "40px auto", padding: "0 16px" },
  subtitle: { color: "#666", marginTop: -8 },
  list: { listStyle: "none", padding: 0 },
  item: { padding: "6px 0", borderBottom: "1px solid #eee" },
  done: { textDecoration: "line-through", color: "#999" },
  actions: { display: "flex", gap: 8, flexWrap: "wrap" },
  btn: { padding: "8px 14px", border: "1px solid #ccc", background: "#fafafa", borderRadius: 6, cursor: "pointer" },
  streamBox: { marginTop: 24, paddingTop: 16, borderTop: "1px solid #eee" },
  label: { display: "grid", gap: 6, fontWeight: 600 },
  input: { padding: "8px 10px", border: "1px solid #ccc", borderRadius: 6, font: "inherit" },
  streamStatus: { color: "#666" },
};
