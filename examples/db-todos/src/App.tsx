import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createRpcClient } from "@zeroship/rpc/client";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  archiveTodo,
  createTodo,
  deleteTodo,
  listTodos,
  publicUser,
  setTodoDone,
} from "./index";
import type { Priority, Todo, TodoSnapshot, User } from "./types";
import { TodoRow } from "./TodoRow";
import { TODO_PRIORITIES, partitionTodos } from "./util";

const qk = (uid: string): ["todos", string] => ["todos", uid];

// Demo seed: plausible-looking random tasks.
const DEMO_VERBS = ["Review", "Ship", "Draft", "Refactor", "Test", "Deploy", "Sync", "Plan", "Polish", "Migrate", "Benchmark", "Triage"];
const DEMO_NOUNS = ["the auth flow", "the API docs", "the landing page", "the billing webhook", "the search index", "onboarding", "the cache layer", "the CI pipeline", "the schema", "the dashboard", "the rate limiter", "the changelog"];
const pick = <T,>(a: readonly T[]) => a[Math.floor(Math.random() * a.length)];
const randomTask = () => ({
  title: `${pick(DEMO_VERBS)} ${pick(DEMO_NOUNS)}`,
  priority: pick(TODO_PRIORITIES),
});
const todoEvents = createRpcClient().stream<{ userId: string }, TodoSnapshot>("todos.subscribe");

const isRecord = (value: unknown): value is Record<string, unknown> =>
  value !== null && typeof value === "object";

const errCode = (e: unknown): string | undefined => {
  if (!isRecord(e)) return undefined;
  return typeof e.code === "string" ? e.code : undefined;
};

const errText = (e: unknown) => {
  const c = errCode(e);
  const msg = isRecord(e) && typeof e.message === "string" ? e.message : String(e);
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
  const [demoLeft, setDemoLeft] = useState(0); // >0 while the demo seeder runs
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
    Promise.resolve(publicUser({}))
      .then((u: User) => !cancelled && setUserId(u.id))
      .catch((e: unknown) => !cancelled && flash(errText(e), errCode(e)));
    return () => {
      cancelled = true;
    };
  }, [flash]);

  const uid = userId ?? "";

  const todosQ = useQuery({
    queryKey: qk(uid),
    queryFn: async (): Promise<Todo[]> => listTodos({ userId: uid }),
    enabled: !!userId,
  });
  const todos = todosQ.data ?? [];

  // Live feed → invalidate (own connection per tab via the nonce reader).
  useEffect(() => {
    if (!userId) return;
    const controller = new AbortController();
    const seen = new Map<string, number>();
    let t: number | undefined;
    (async () => {
      try {
        const stream = todoEvents({ userId }, { signal: controller.signal });
        setLive(true);
        for await (const snapshot of stream) {
          if (controller.signal.aborted) break;
          const sig = snapshot.rows.map((row) => row.id).join(",");
          const now = Date.now();
          const prev = seen.get(sig);
          if (prev && now - prev < 600) continue;
          seen.set(sig, now);
          if (seen.size > 200) seen.clear();
          qc.setQueryData<Todo[]>(qk(userId), snapshot.rows);
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
  const createM = useMutation<
    Todo,
    unknown,
    { userId: string; title: string; priority?: Priority },
    { prev: Todo[] | undefined; ck: string; userId: string }
  >({
    mutationFn: async (input) => createTodo(input),
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
        created_by: null,
        updated_by: null,
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

  // Demo seeder: fire 10 random tasks, one every 500ms. Each goes through the
  // same optimistic create path, so they cascade into the list (and into every
  // other open window via realtime).
  const runDemo = useCallback(async () => {
    if (!userId || demoLeft > 0) return;
    const N = 10;
    for (let i = 0; i < N; i++) {
      setDemoLeft(N - i);
      const { title: t, priority: p } = randomTask();
      createM.mutate({ userId, title: t, priority: p });
      await new Promise((r) => setTimeout(r, 500));
    }
    setDemoLeft(0);
  }, [userId, demoLeft, createM]);

  const onSetDone = useCallback(
    async (id: string, done: boolean) => {
      qc.setQueryData<Todo[]>(qk(uid), (old = []) =>
        old.map((t) => (t.id === id ? { ...t, done } : t)),
      );
      try {
        await setTodoDone({ id, done });
      } catch (e) {
        flash(errText(e), errCode(e));
      } finally {
        void qc.invalidateQueries({ queryKey: qk(uid) });
      }
    },
    [uid, qc, flash],
  );

  const onRemove = useCallback(
    async (id: string, fn: (i: { id: string }) => unknown) => {
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

  const { active, done } = useMemo(() => partitionTodos(todos), [todos]);

  const booting = !userId || todosQ.isLoading;

  return (
    <div className="app">
      <header className="top">
        <h1>Todos</h1>
        <div className="meta">
          <button
            className="ghost"
            onClick={() => void runDemo()}
            disabled={!userId || demoLeft > 0}
            title="Create 10 random tasks, 500ms apart"
          >
            {demoLeft > 0 ? `seeding ${demoLeft}…` : "demo"}
          </button>
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
          {TODO_PRIORITIES.map((p) => (
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
            <TodoRow
              key={keyOf(t)}
              todo={t}
              index={i}
              removing={removing.has(t.id)}
              onSetDone={(done) => void onSetDone(t.id, done)}
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
