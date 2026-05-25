import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  archiveTodo,
  completeTodo,
  createTodo,
  deleteTodo,
  listTodos,
  publicUser,
  RpcError,
  subscribeTodos,
  type Priority,
  type Todo,
  type User,
} from "./api";

const PRIORITIES: Priority[] = ["low", "medium", "high"];

// ── time ────────────────────────────────────────────────────────────────
function ago(ms: number): string {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 5) return "just now";
  if (s < 60) return `${s}s ago`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.round(h / 24)}d ago`;
}
const shortId = (id: string) => {
  const [p, rest] = id.split("_");
  return rest ? `${p}_${rest.slice(0, 6)}…` : id;
};

type Banner = { kind: "error" | "live"; text: string } | null;

export function App() {
  // The single shared "public ledger" user — there's no per-window identity;
  // every window reads + writes the same list, so the realtime feed streams
  // to all viewers at once.
  const [user, setUser] = useState<User | null>(null);
  const [todos, setTodos] = useState<Todo[]>([]);
  const [loading, setLoading] = useState(true);
  const [booting, setBooting] = useState(true);
  const [banner, setBanner] = useState<Banner>(null);
  const [live, setLive] = useState(false);
  const [pulse, setPulse] = useState(0);
  const [title, setTitle] = useState("");
  const [priority, setPriority] = useState<Priority>("medium");
  const [removing, setRemoving] = useState<Set<string>>(new Set());
  const inputRef = useRef<HTMLInputElement>(null);

  const flash = useCallback((b: Banner) => {
    setBanner(b);
    if (b) window.setTimeout(() => setBanner((cur) => (cur === b ? null : cur)), 3200);
  }, []);

  const errText = (e: unknown) => (e instanceof RpcError ? `${e.code}: ${e.message}` : String(e));

  const refresh = useCallback(
    async (uid: string) => {
      try {
        setTodos(await listTodos(uid));
      } catch (e) {
        flash({ kind: "error", text: errText(e) });
      } finally {
        setLoading(false);
      }
    },
    [flash],
  );

  // Resolve the shared ledger user once on mount (get-or-create).
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const u = await publicUser();
        if (!cancelled) setUser(u);
      } catch (e) {
        if (!cancelled) flash({ kind: "error", text: errText(e) });
      } finally {
        if (!cancelled) setBooting(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [flash]);

  // Load + live-subscribe once the shared user is resolved. The platform
  // streams the AI-SDK Data Stream Protocol; `subscribeTodos()` yields parsed
  // change events (own connection per tab via a nonce) — debounce-refetch.
  useEffect(() => {
    if (!user) return;
    setLoading(true);
    void refresh(user.id);
    const controller = new AbortController();
    let t: number | undefined;
    (async () => {
      try {
        const stream = subscribeTodos(controller.signal);
        setLive(true);
        for await (const ev of stream) {
          if (controller.signal.aborted) break;
          if (ev.collection && ev.collection !== "todos") continue;
          setPulse((p) => p + 1);
          window.clearTimeout(t);
          t = window.setTimeout(() => void refresh(user.id), 180);
        }
      } catch {
        /* aborted on teardown, or the stream ended/errored */
      } finally {
        setLive(false);
      }
    })();
    return () => {
      window.clearTimeout(t);
      controller.abort();
      setLive(false);
    };
  }, [user, refresh]);

  const add = useCallback(
    async (e?: React.FormEvent) => {
      e?.preventDefault();
      const text = title.trim();
      if (!text || !user) return;
      setTitle("");
      const temp: Todo = {
        id: `tmp_${Date.now()}`,
        created_at: Date.now(),
        updated_at: Date.now(),
        version: 1,
        userId: user.id,
        title: text,
        priority,
        tags: [],
        done: false,
        archived: false,
        deleted_at: null,
      };
      setTodos((cur) => [temp, ...cur]);
      try {
        await createTodo({ userId: user.id, title: text, priority });
        await refresh(user.id);
      } catch (err) {
        setTodos((cur) => cur.filter((x) => x.id !== temp.id));
        flash({ kind: "error", text: errText(err) });
      }
      inputRef.current?.focus();
    },
    [title, priority, user, refresh, flash],
  );

  const mutate = useCallback(
    async (id: string, fn: (id: string) => Promise<unknown>, optimisticDrop: boolean) => {
      if (optimisticDrop) {
        setRemoving((s) => new Set(s).add(id));
        await new Promise((r) => setTimeout(r, 220));
      }
      try {
        await fn(id);
        if (user) await refresh(user.id);
      } catch (e) {
        flash({ kind: "error", text: errText(e) });
        if (user) await refresh(user.id);
      } finally {
        setRemoving((s) => {
          const n = new Set(s);
          n.delete(id);
          return n;
        });
      }
    },
    [user, refresh, flash],
  );

  const { active, done } = useMemo(() => {
    const a: Todo[] = [];
    const d: Todo[] = [];
    for (const t of todos) (t.done ? d : a).push(t);
    return { active: a, done: d };
  }, [todos]);

  return (
    <div className="shell">
      <div className="grain" aria-hidden />
      <div className="glow" aria-hidden />

      <header className="masthead">
        <div className="brand">
          <span className="mark">LEDGER</span>
          <span className="rule" aria-hidden />
          <span className="sub">shared · db-todos · zeroship</span>
        </div>
        <div className="status">
          <span className="public-tag">PUBLIC</span>
          <span className={`live ${live ? "on" : "off"}`} key={pulse}>
            <i className="dot" />
            {live ? "LIVE" : "OFFLINE"}
          </span>
        </div>
      </header>

      <main className="stage">
        <div className="lede">
          <h1>
            One ledger, <em>everyone</em>.
          </h1>
          <p>
            A single shared list on the real <code>@zeroship/db</code> surface —
            no logins, no per-window identity. Every change is committed to the
            same collection and streamed to every open window over SSE. Open a
            second tab and watch them move together.
          </p>
        </div>

        <form className="composer" onSubmit={add}>
          <input
            ref={inputRef}
            className="title-input"
            placeholder="Add to the shared ledger…"
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            disabled={!user}
            maxLength={200}
            autoFocus
          />
          <div className="prio-pick" role="radiogroup" aria-label="priority">
            {PRIORITIES.map((p) => (
              <button
                type="button"
                key={p}
                className={`prio ${p} ${priority === p ? "sel" : ""}`}
                aria-pressed={priority === p}
                onClick={() => setPriority(p)}
              >
                {p}
              </button>
            ))}
          </div>
          <button className="commit" type="submit" disabled={!user || !title.trim()}>
            Commit ↵
          </button>
        </form>

        <section className="ledger">
          <div className="col-head">
            <span>entry</span>
            <span className="count">
              {active.length} open · {done.length} done
            </span>
          </div>

          {loading || booting ? (
            <div className="skeleton">
              {[0, 1, 2].map((i) => (
                <div className="row sk" style={{ animationDelay: `${i * 90}ms` }} key={i} />
              ))}
            </div>
          ) : todos.length === 0 ? (
            <div className="empty">
              <span className="big">∅</span>
              <p>The ledger is clean. Commit the first entry — everyone will see it.</p>
            </div>
          ) : (
            <ul className="rows">
              {[...active, ...done].map((t, i) => (
                <TodoRow
                  key={t.id}
                  todo={t}
                  index={i}
                  removing={removing.has(t.id)}
                  onComplete={() => void mutate(t.id, completeTodo, false)}
                  onArchive={() => void mutate(t.id, archiveTodo, true)}
                  onDelete={() => void mutate(t.id, deleteTodo, true)}
                />
              ))}
            </ul>
          )}
        </section>
      </main>

      <footer className="footplate">
        <span>SQLite dev backend · all writes autocommit · realtime broadcast via broker SSE</span>
      </footer>

      {banner && <div className={`banner ${banner.kind}`}>{banner.text}</div>}
    </div>
  );
}

function TodoRow({
  todo,
  index,
  removing,
  onComplete,
  onArchive,
  onDelete,
}: {
  todo: Todo;
  index: number;
  removing: boolean;
  onComplete: () => void;
  onArchive: () => void;
  onDelete: () => void;
}) {
  const pending = todo.id.startsWith("tmp_");
  return (
    <li
      className={`row ${todo.done ? "done" : ""} ${removing ? "leaving" : ""} ${pending ? "pending" : ""} p-${todo.priority}`}
      style={{ animationDelay: `${Math.min(index, 12) * 45}ms` }}
    >
      <button
        className="check"
        aria-label={todo.done ? "completed" : "complete"}
        onClick={onComplete}
        disabled={todo.done || pending}
      >
        <svg viewBox="0 0 24 24" width="15" height="15" aria-hidden>
          <path d="M4 12.5l5 5L20 6" fill="none" stroke="currentColor" strokeWidth="2.4" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </button>

      <div className="body">
        <span className="text">{todo.title}</span>
        <div className="meta">
          <span className={`tag prio ${todo.priority}`}>{todo.priority}</span>
          {todo.tags?.map((tg) => (
            <span className="tag" key={tg}>
              #{tg}
            </span>
          ))}
          <span className="mono">{shortId(todo.id)}</span>
          <span className="mono dim">v{todo.version}</span>
          <span className="mono dim">{ago(todo.created_at)}</span>
        </div>
      </div>

      <div className="actions">
        <button className="act" onClick={onArchive} disabled={pending} title="Archive">
          archive
        </button>
        <button className="act danger" onClick={onDelete} disabled={pending} title="Soft-delete">
          delete
        </button>
      </div>
    </li>
  );
}
