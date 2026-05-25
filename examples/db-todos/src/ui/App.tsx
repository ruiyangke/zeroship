import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  archiveTodo,
  completeTodo,
  createTodo,
  deleteTodo,
  listTodos,
  RpcError,
  seedUser,
  subscribeTodos,
  type Priority,
  type Todo,
  type User,
} from "./api";

const USER_KEY = "ledger.session.user";
const PRIORITIES: Priority[] = ["low", "medium", "high"];

// ── session ───────────────────────────────────────────────────────────────
function loadUser(): User | null {
  try {
    const raw = localStorage.getItem(USER_KEY);
    return raw ? (JSON.parse(raw) as User) : null;
  } catch {
    return null;
  }
}
function saveUser(u: User) {
  localStorage.setItem(USER_KEY, JSON.stringify(u));
}
function freshIdentity() {
  const tag = Date.now().toString(36) + Math.random().toString(36).slice(2, 5);
  const handle = `guest_${tag}`;
  return { handle, email: `${handle}@ledger.local`, name: `Operator ${tag.slice(-4).toUpperCase()}` };
}

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
  const [user, setUser] = useState<User | null>(loadUser);
  const [todos, setTodos] = useState<Todo[]>([]);
  const [loading, setLoading] = useState(true);
  const [booting, setBooting] = useState(!loadUser());
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

  const refresh = useCallback(
    async (uid: string) => {
      try {
        const rows = await listTodos(uid);
        setTodos(rows);
      } catch (e) {
        flash({ kind: "error", text: e instanceof RpcError ? `${e.code}: ${e.message}` : String(e) });
      } finally {
        setLoading(false);
      }
    },
    [flash],
  );

  // Bootstrap a session user on first visit.
  useEffect(() => {
    if (user) return;
    let cancelled = false;
    (async () => {
      try {
        const u = await seedUser(freshIdentity());
        if (cancelled) return;
        saveUser(u);
        setUser(u);
      } catch (e) {
        flash({ kind: "error", text: e instanceof RpcError ? `${e.code}: ${e.message}` : String(e) });
      } finally {
        if (!cancelled) setBooting(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [user, flash]);

  // Load + live-subscribe once we have a user.
  useEffect(() => {
    if (!user) return;
    setLoading(true);
    void refresh(user.id);
    let t: number | undefined;
    const stop = subscribeTodos(
      (ev) => {
        if (ev.collection && ev.collection !== "todos") return;
        setPulse((p) => p + 1);
        window.clearTimeout(t);
        t = window.setTimeout(() => void refresh(user.id), 180); // debounce bursts
      },
      (isLive) => setLive(isLive),
    );
    return () => {
      window.clearTimeout(t);
      stop();
    };
  }, [user, refresh]);

  const add = useCallback(
    async (e?: React.FormEvent) => {
      e?.preventDefault();
      const text = title.trim();
      if (!text || !user) return;
      setTitle("");
      // optimistic insert
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
        flash({ kind: "error", text: err instanceof RpcError ? `${err.code}: ${err.message}` : String(err) });
      }
      inputRef.current?.focus();
    },
    [title, priority, user, refresh, flash],
  );

  const mutate = useCallback(
    async (id: string, fn: (id: string) => Promise<unknown>, optimisticDrop: boolean) => {
      if (optimisticDrop) {
        setRemoving((s) => new Set(s).add(id));
        await new Promise((r) => setTimeout(r, 220)); // let the exit animation play
      }
      try {
        await fn(id);
        if (user) await refresh(user.id);
      } catch (e) {
        flash({ kind: "error", text: e instanceof RpcError ? `${e.code}: ${e.message}` : String(e) });
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

  const newSession = useCallback(async () => {
    localStorage.removeItem(USER_KEY);
    setTodos([]);
    setLoading(true);
    setBooting(true);
    setUser(null);
  }, []);

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
          <span className="sub">db-todos · zeroship runtime</span>
        </div>
        <div className="status">
          <span className={`live ${live ? "on" : "off"}`} key={pulse}>
            <i className="dot" />
            {live ? "LIVE" : "OFFLINE"}
          </span>
          {user && (
            <button className="session" onClick={newSession} title="Start a fresh session user">
              <span className="op">{user.name}</span>
              <span className="handle">@{user.handle}</span>
              <span className="uid">{shortId(user.id)}</span>
            </button>
          )}
        </div>
      </header>

      <main className="stage">
        <div className="lede">
          <h1>
            What needs <em>doing</em>.
          </h1>
          <p>
            Every keystroke here rides the real <code>@zeroship/db</code> surface —
            typed ids, optimistic concurrency, soft-delete, and a live change feed
            streamed over SSE. Open a second tab; watch it keep pace.
          </p>
        </div>

        <form className="composer" onSubmit={add}>
          <input
            ref={inputRef}
            className="title-input"
            placeholder="Draft a task…"
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
              <p>The ledger is clean. Commit your first entry above.</p>
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
        <span>SQLite dev backend · all writes autocommit · realtime via broker SSE</span>
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
