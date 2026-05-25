import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  archiveTodo,
  completeTodo,
  createTodo,
  deleteTodo,
  errCode,
  listTodos,
  publicUser,
  subscribeTodos,
  type Priority,
  type Todo,
} from "./api";

const PRIORITIES: Priority[] = ["low", "medium", "high"];
const qk = (uid: string) => ["todos", uid] as const;

function ago(ms: number): string {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 5) return "now";
  if (s < 60) return `${s}s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h`;
  return `${Math.round(h / 24)}d`;
}
const errText = (e: unknown) => {
  const c = errCode(e);
  const msg = (e as { message?: string } | null)?.message ?? String(e);
  return c ? `${c} · ${msg}` : msg;
};

type Banner = { kind: "error"; text: string; code?: string } | null;

export function App() {
  const qc = useQueryClient();

  const [userId, setUserId] = useState<string | null>(null);
  const [banner, setBanner] = useState<Banner>(null);
  const [live, setLive] = useState(false);
  const [pulse, setPulse] = useState(0);
  const [title, setTitle] = useState("");
  const [priority, setPriority] = useState<Priority>("low");
  const [removing, setRemoving] = useState<Set<string>>(new Set());
  const inputRef = useRef<HTMLInputElement>(null);
  const keyMap = useRef(new Map<string, string>());
  const keySeq = useRef(0);
  const keyOf = (t: Todo) => t._key ?? keyMap.current.get(t.id) ?? t.id;

  const flash = useCallback((text: string, code?: string) => {
    const b: Banner = { kind: "error", text, code };
    setBanner(b);
    window.setTimeout(() => setBanner((cur) => (cur === b ? null : cur)), 3400);
  }, []);

  useEffect(() => {
    let cancelled = false;
    publicUser({})
      .then((u) => !cancelled && setUserId(u.id))
      .catch((e) => !cancelled && flash(errText(e), errCode(e)));
    return () => {
      cancelled = true;
    };
  }, [flash]);

  const uid = userId ?? "";

  const todosQ = useQuery({
    queryKey: qk(uid),
    queryFn: () => listTodos({ userId: uid }),
    enabled: !!userId,
  });
  const todos = (todosQ.data ?? []) as Todo[];

  // Live feed → invalidate (own connection per tab via the nonce reader).
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
            if (prev && now - prev < 600) continue;
            seen.set(pk, now);
            if (seen.size > 200) seen.clear();
          }
          setPulse((p) => p + 1);
          window.clearTimeout(t);
          t = window.setTimeout(() => void qc.invalidateQueries({ queryKey: qk(userId) }), 180);
        }
      } catch {
        /* aborted / ended */
      } finally {
        setLive(false);
      }
    })();
    return () => {
      window.clearTimeout(t);
      controller.abort();
      setLive(false);
    };
  }, [userId, qc]);

  // Optimistic create (raw React Query over the direct-imported caller).
  const createM = useMutation({
    mutationFn: createTodo,
    onMutate: async (input) => {
      const key = qk(input.userId);
      await qc.cancelQueries({ queryKey: key });
      const prev = qc.getQueryData<Todo[]>(key);
      const ck = `k${++keySeq.current}`;
      const optimistic: Todo = {
        id: `tmp_${ck}`,
        _key: ck,
        created_at: Date.now(),
        updated_at: Date.now(),
        version: 1,
        userId: input.userId,
        title: input.title,
        priority: input.priority ?? "low",
        tags: [],
        done: false,
        archived: false,
        deleted_at: null,
      };
      qc.setQueryData<Todo[]>(key, (old = []) => [optimistic, ...old]);
      return { prev, ck, userId: input.userId };
    },
    onError: (e, _input, ctx) => {
      if (ctx) qc.setQueryData<Todo[]>(qk(ctx.userId), ctx.prev ?? []);
      flash(errText(e), errCode(e));
    },
    onSuccess: (real, _input, ctx) => {
      keyMap.current.set(real.id, ctx.ck);
      qc.setQueryData<Todo[]>(qk(ctx.userId), (old = []) =>
        old.map((t) => (t._key === ctx.ck ? { ...real, _key: ctx.ck } : t)),
      );
    },
    onSettled: (_d, _e, _input, ctx) => {
      if (ctx) void qc.invalidateQueries({ queryKey: qk(ctx.userId) });
    },
  });

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

  const onComplete = useCallback(
    async (id: string) => {
      qc.setQueryData<Todo[]>(qk(uid), (old = []) =>
        old.map((t) => (t.id === id ? { ...t, done: true } : t)),
      );
      try {
        await completeTodo({ id });
      } catch (e) {
        flash(errText(e), errCode(e));
      } finally {
        void qc.invalidateQueries({ queryKey: qk(uid) });
      }
    },
    [uid, qc, flash],
  );

  const onRemove = useCallback(
    async (id: string, fn: (i: { id: string }) => Promise<unknown>) => {
      setRemoving((s) => new Set(s).add(id));
      await new Promise((r) => setTimeout(r, 220));
      qc.setQueryData<Todo[]>(qk(uid), (old = []) => old.filter((t) => t.id !== id));
      try {
        await fn({ id });
      } catch (e) {
        flash(errText(e), errCode(e));
      } finally {
        setRemoving((s) => {
          const n = new Set(s);
          n.delete(id);
          return n;
        });
        void qc.invalidateQueries({ queryKey: qk(uid) });
      }
    },
    [uid, qc, flash],
  );

  const { active, done } = useMemo(() => {
    const a: Todo[] = [];
    const d: Todo[] = [];
    for (const t of todos) (t.done ? d : a).push(t);
    return { active: a, done: d };
  }, [todos]);

  const booting = !userId || todosQ.isLoading;

  return (
    <div className="app">
      <header className="top">
        <h1>Todos</h1>
        <div className="meta">
          <span className="count">{active.length} open</span>
          <span className={`live ${live ? "on" : ""}`} key={pulse}>
            <i className="dot" />
            {live ? "live" : "offline"}
          </span>
        </div>
      </header>

      <p className="tagline">
        A shared list on the real <code>@zeroship/db</code> surface. Open another
        window — it stays in sync over realtime SSE.
      </p>

      <form className="new" onSubmit={add}>
        <input
          ref={inputRef}
          placeholder="Add a task…"
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          disabled={!userId}
          maxLength={200}
          autoFocus
        />
        <div className="prio" role="radiogroup" aria-label="priority">
          {PRIORITIES.map((p) => (
            <button
              type="button"
              key={p}
              className={p}
              aria-pressed={priority === p}
              aria-label={`${p} priority`}
              title={`${p} priority`}
              onClick={() => setPriority(p)}
            />
          ))}
        </div>
        <button className="add" type="submit" disabled={!userId || !title.trim()}>
          Add
        </button>
      </form>

      {booting ? (
        <div className="skeleton">
          <div className="sk" />
          <div className="sk" />
          <div className="sk" />
        </div>
      ) : todos.length === 0 ? (
        <div className="empty">
          <div className="ring" />
          <p>Nothing yet. Add the first task — everyone sees it.</p>
        </div>
      ) : (
        <ul className="list">
          {[...active, ...done].map((t, i) => (
            <Item
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

      <footer className="foot">
        Realtime via broker SSE · SQLite dev backend · writes autocommit
      </footer>

      {banner && (
        <div className="toast">
          {banner.code && <span className="tcode">{banner.code}</span>}
          {banner.text.replace(new RegExp(`^${banner.code} · `), "")}
        </div>
      )}
    </div>
  );
}

const Check = () => (
  <svg viewBox="0 0 24 24" fill="none" aria-hidden>
    <path d="M5 12.5l4.5 4.5L19 7.5" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" />
  </svg>
);
const ArchiveIcon = () => (
  <svg viewBox="0 0 24 24" fill="none" aria-hidden>
    <rect x="3.5" y="4.5" width="17" height="4" rx="1.2" stroke="currentColor" strokeWidth="1.7" />
    <path d="M5 8.5V18a1.5 1.5 0 0 0 1.5 1.5h11A1.5 1.5 0 0 0 19 18V8.5" stroke="currentColor" strokeWidth="1.7" />
    <path d="M10 12h4" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" />
  </svg>
);
const TrashIcon = () => (
  <svg viewBox="0 0 24 24" fill="none" aria-hidden>
    <path d="M4.5 6.5h15M9 6.5V5a1.5 1.5 0 0 1 1.5-1.5h3A1.5 1.5 0 0 1 15 5v1.5M7 6.5 7.7 19a1.5 1.5 0 0 0 1.5 1.4h5.6a1.5 1.5 0 0 0 1.5-1.4L17 6.5" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" />
  </svg>
);

function Item({
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
      className={`item ${todo.done ? "done" : ""} ${removing ? "leaving" : ""} ${pending ? "pending" : ""}`}
      style={{ animationDelay: `${Math.min(index, 14) * 28}ms` }}
    >
      <button
        className="box"
        aria-label={todo.done ? "completed" : "mark complete"}
        onClick={onComplete}
        disabled={todo.done || pending}
      >
        <Check />
      </button>

      <span className={`pri-tick ${todo.priority}`} title={`${todo.priority} priority`} aria-hidden />
      <span className="label">{todo.title}</span>

      <div className="right">
        <span className="when">{pending ? "…" : ago(todo.created_at)}</span>
        <div className="actions">
          <button className="icon-btn" onClick={onArchive} disabled={pending} aria-label="archive" title="Archive">
            <ArchiveIcon />
          </button>
          <button className="icon-btn danger" onClick={onDelete} disabled={pending} aria-label="delete" title="Delete">
            <TrashIcon />
          </button>
        </div>
      </div>
    </li>
  );
}
