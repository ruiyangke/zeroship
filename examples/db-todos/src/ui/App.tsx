import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
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
} from "./api";

const PRIORITIES: Priority[] = ["low", "medium", "high"];

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
const errText = (e: unknown) => (e instanceof RpcError ? `${e.code}: ${e.message}` : String(e));

type Banner = { kind: "error" | "live"; text: string } | null;

export function App() {
  const qc = useQueryClient();

  // The single shared "public ledger" user — no per-window identity.
  const [userId, setUserId] = useState<string | null>(null);
  const [banner, setBanner] = useState<Banner>(null);
  const [live, setLive] = useState(false);
  const [pulse, setPulse] = useState(0);
  const [title, setTitle] = useState("");
  const [priority, setPriority] = useState<Priority>("medium");
  const [removing, setRemoving] = useState<Set<string>>(new Set());
  const inputRef = useRef<HTMLInputElement>(null);
  // realId → optimistic key, so the optimistic→real swap keeps one React
  // element (no re-mount, no entrance-animation "double flash").
  const keyMap = useRef(new Map<string, string>());
  const keySeq = useRef(0);
  const keyOf = (t: Todo) => t._key ?? keyMap.current.get(t.id) ?? t.id;

  const flash = useCallback((b: Banner) => {
    setBanner(b);
    if (b) window.setTimeout(() => setBanner((cur) => (cur === b ? null : cur)), 3200);
  }, []);

  // Resolve the shared ledger user once (get-or-create).
  useEffect(() => {
    let cancelled = false;
    publicUser({})
      .then((u) => !cancelled && setUserId(u.id))
      .catch((e) => !cancelled && flash({ kind: "error", text: errText(e) }));
    return () => {
      cancelled = true;
    };
  }, [flash]);

  // The list — React Query owns the cache; everything below reads/writes it.
  const todosQ = listTodos.useQuery(
    { userId: userId ?? "" },
    { enabled: !!userId },
  ) as { data?: Todo[]; isLoading: boolean };
  const todos = todosQ.data ?? [];
  const uid = userId ?? "";

  // Live feed → invalidate (React Query refetches + dedupes). Own connection
  // per tab via the nonce in subscribeTodos.
  useEffect(() => {
    if (!userId) return;
    const controller = new AbortController();
    const seen = new Map<string, number>();
    let t: number | undefined;
    (async () => {
      try {
        const stream = subscribeTodos(controller.signal);
        setLive(true);
        for await (const ev of stream) {
          if (controller.signal.aborted) break;
          if (ev.collection && ev.collection !== "todos") continue;
          const pk = String(ev.pk ?? "");
          const now = Date.now();
          if (pk) {
            const prev = seen.get(pk);
            if (prev && now - prev < 600) continue; // collapse the broker's double frame
            seen.set(pk, now);
            if (seen.size > 200) seen.clear();
          }
          setPulse((p) => p + 1);
          window.clearTimeout(t);
          t = window.setTimeout(() => void listTodos.invalidate({ userId }), 180);
        }
      } catch {
        /* aborted on teardown / stream ended */
      } finally {
        setLive(false);
      }
    })();
    return () => {
      window.clearTimeout(t);
      controller.abort();
      setLive(false);
    };
  }, [userId]);

  // ── Optimistic create (the React Query pattern) ───────────────────────
  const createM = createTodo.useMutation({
    onMutate: async (input: { userId: string; title: string; priority?: Priority }) => {
      const key = listTodos.queryKey({ userId: input.userId });
      await qc.cancelQueries({ queryKey: key });
      const prev = qc.getQueryData<Todo[]>(key);
      const k = `k${++keySeq.current}`;
      const optimistic: Todo = {
        id: `tmp_${k}`,
        _key: k,
        created_at: Date.now(),
        updated_at: Date.now(),
        version: 1,
        userId: input.userId,
        title: input.title,
        priority: input.priority ?? "medium",
        tags: [],
        done: false,
        archived: false,
        deleted_at: null,
      };
      listTodos.setData({ userId: input.userId }, (old: Todo[] = []) => [optimistic, ...old]);
      return { prev, k, userId: input.userId };
    },
    onError: (e: unknown, _input: unknown, ctx: { prev?: Todo[]; userId: string } | undefined) => {
      if (ctx) listTodos.setData({ userId: ctx.userId }, ctx.prev ?? []);
      flash({ kind: "error", text: errText(e) });
    },
    onSuccess: (real: Todo, _input: unknown, ctx: { k: string; userId: string } | undefined) => {
      if (!ctx) return;
      keyMap.current.set(real.id, ctx.k); // bridge real id → optimistic key
      listTodos.setData({ userId: ctx.userId }, (old: Todo[] = []) =>
        old.map((t) => (t._key === ctx.k ? { ...real, _key: ctx.k } : t)),
      );
    },
    onSettled: (_d: unknown, _e: unknown, _input: unknown, ctx: { userId: string } | undefined) => {
      if (ctx) void listTodos.invalidate({ userId: ctx.userId });
    },
  }) as { mutate: (input: { userId: string; title: string; priority?: Priority }) => void };

  const add = useCallback(
    (e?: React.FormEvent) => {
      e?.preventDefault();
      const text = title.trim();
      if (!text || !userId) return;
      setTitle("");
      createM.mutate({ userId, title: text, priority });
      inputRef.current?.focus();
    },
    [title, priority, userId, createM],
  );

  // complete: optimistic flip, then sync.
  const onComplete = useCallback(
    async (id: string) => {
      listTodos.setData({ userId: uid }, (old: Todo[] = []) =>
        old.map((t) => (t.id === id ? { ...t, done: true } : t)),
      );
      try {
        await completeTodo({ id });
      } catch (e) {
        flash({ kind: "error", text: errText(e) });
      } finally {
        void listTodos.invalidate({ userId: uid });
      }
    },
    [uid, flash],
  );

  // archive / delete: play the leave animation, optimistically drop, then sync.
  const onRemove = useCallback(
    async (id: string, fn: (i: { id: string }) => Promise<unknown>) => {
      setRemoving((s) => new Set(s).add(id));
      await new Promise((r) => setTimeout(r, 220));
      listTodos.setData({ userId: uid }, (old: Todo[] = []) => old.filter((t) => t.id !== id));
      try {
        await fn({ id });
      } catch (e) {
        flash({ kind: "error", text: errText(e) });
      } finally {
        setRemoving((s) => {
          const n = new Set(s);
          n.delete(id);
          return n;
        });
        void listTodos.invalidate({ userId: uid });
      }
    },
    [uid, flash],
  );

  const { active, done } = useMemo(() => {
    const a: Todo[] = [];
    const d: Todo[] = [];
    for (const t of todos) (t.done ? d : a).push(t);
    return { active: a, done: d };
  }, [todos]);

  const booting = !userId || todosQ.isLoading;

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
            no logins, no per-window identity. Creates are optimistic via React
            Query; every commit streams to every open window over SSE. Open a
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
            disabled={!userId}
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
          <button className="commit" type="submit" disabled={!userId || !title.trim()}>
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

          {booting ? (
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
                  key={keyOf(t)}
                  todo={t}
                  index={i}
                  removing={removing.has(t.id)}
                  onComplete={() => void onComplete(t.id)}
                  onArchive={() => void onRemove(t.id, archiveTodo)}
                  onDelete={() => void onRemove(t.id, deleteTodo)}
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
          <span className="mono">{pending ? "committing…" : shortId(todo.id)}</span>
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
