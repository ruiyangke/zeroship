# db-v2-chat

Reactive-query example. Three related collections, channel-scoped live
message stream. Exercises the C1 + P8b + React layers shipped in v2.

## What this demonstrates

| Feature | Commit | Surface in this example |
|---|---|---|
| `@zeroship/db` schema with t.ref refs | B2 — `eab163a` | `messages.channelId: t.ref("channels")`, `messages.authorId: t.ref("users")` |
| query / mutation / action capability kinds | B3 — `7b8074e` + `6df9097` | `listMessages` is `query`, `sendMessage`/`flagMessage` are `mutation`, `moderateMessage` is `action` |
| Subscription v8_class with Weak finalizer | P8a — `a7261cf` | Broker handle reclaimed at GC; closes the AsyncIterable leak |
| Streaming WAL consumer | P8a.2 — `a061a1e` | Cross-worker propagation; insert on worker A reaches subscriber on worker B |
| Read-set narrowing | P8b — `ee83fa5` | `listMessages` records `{channelId}` predicate; broker skips events that don't match |
| WAL supervisor + auto-spawn | F2 — `e5e3aa5` | Consumer reconnects on transient errors; auto-starts at boot |
| React `useQuery` hook | F1 — `fbb0e31` | Auto-rerender on broker events; StrictMode-safe |
| Auto-tx per kind | F4 — `cc9ddb1` | `listMessages` runs READ ONLY; `sendMessage` runs SERIALIZABLE |

## Architecture

```
                ┌─────────────┐
   useQuery ───►│  React hook │ subscribe via @zeroship/db
                └──────┬──────┘    Subscription (P8a v8_class)
                       │
                       │ AsyncIterable
                       │
                ┌──────▼──────┐
                │   Broker    │◄── local-emit (P8a)
                └──────┬──────┘◄── WAL pgoutput (P8a.2)
                       │
                       │ predicate-evaluated (P8b read-set)
                       ▼
                  re-render
```

A `listMessages({channelId})` query records `channelId` as a
predicate. When `sendMessage` fires a broker event, the broker walks
subscribers; only the one for that channel re-runs and re-renders.

## Run locally

Prereqs:
- Postgres with `wal_level=logical` (in postgresql.conf; restart needed).
  Without it, only single-worker local-emit fires; cross-worker
  propagation is silent.

```bash
npm install
npm run typecheck     # static verification
npm run dev           # vite-plugin bootstraps worker + WAL consumer
```

Open `http://localhost:3000/?channel=1` and `?channel=2` in two tabs.
Send messages in each; the other tab does NOT re-render — that's the
read-set narrowing working. Two tabs on the same channel re-render
together.

```bash
npm run smoke         # in a third shell — exercises 5 named checks
```

## Smoke test checks

1. `sendMessage` happy path (mutation auto-tx)
2. `listMessages` returns rows (query auto-tx READ ONLY)
3. **Read-set narrowing**: a message in #random doesn't appear in #general's list
4. FK enforcement: sendMessage with non-existent channel fails (B2)
5. WAL consumer status reachable

## What's NOT in this example

- P8c admin role / HMAC session init (deployment-tier; not visible from app code)
- `t.union` discriminated documents — see `examples/db-v2-todos/` (also doesn't have it; not yet illustrated)
- `withVersioning` optimistic concurrency
- Real-WS multiplexing — the React hook uses the AsyncIterable path today;
  multiplexed transport is a perf follow-up
