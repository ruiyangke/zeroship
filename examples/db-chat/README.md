# db-chat

Reactive-query example with three related collections and channel-scoped
message reads. It is useful for checking the older `@zeroship/react` hook
surface against the current DB and RPC runtime.

## What this demonstrates

| Feature | Surface in this example |
| --- | --- |
| `@zeroship/db` schema with refs | `messages.channelId: t.ref("channels")`, `messages.authorId: t.ref("users")` |
| RPC capabilities | `listMessages` is a `query`, write paths are `mutation`, moderation is an `action` |
| Result unwrapping | Handlers unwrap `Result<T>` before returning through RPC |
| Reactive reads | The client hook re-runs a query when a matching DB event is published |
| Action composition | `moderateMessage` calls `runQuery` and `runMutation` from `@zeroship/server` |

## Architecture

```
                ┌─────────────┐
   useQuery ───►│  React hook │ subscribe via @zeroship/react
                └──────┬──────┘
                       │
                       │ AsyncIterable
                       │
                ┌──────▼──────┐
                │   Broker    │◄── DB change publication
                └──────┬──────┘
                       │
                       │ predicate-evaluated (P8b read-set)
                       ▼
                  re-render
```

A `listMessages({ channelId })` query scopes the UI to one channel. When
`sendMessage` writes a row, the reactive layer re-runs interested queries.

## Run locally

```bash
pnpm install
pnpm typecheck        # static verification
pnpm dev              # vite-plugin bootstraps the dev runtime
```

Open `http://localhost:3000/?channel=1` and `?channel=2` in two tabs.
Send messages in each; the other tab does NOT re-render — that's the
read-set narrowing working. Two tabs on the same channel re-render
together.

```bash
pnpm smoke            # in a third shell
```

## Smoke test checks

1. `sendMessage` happy path (mutation capability)
2. `listMessages` returns rows (query capability)
3. **Read-set narrowing**: a message in #random doesn't appear in #general's list
4. FK enforcement: sendMessage with non-existent channel fails (B2)
5. WAL consumer status reachable

## What's NOT in this example

- Production auth/session setup.
- `t.union` discriminated documents — see `examples/db-todos/` (also doesn't have it; not yet illustrated)
- `withVersioning` optimistic concurrency
- The current `@zeroship/rpc/client` API. This example still uses the
  older `@zeroship/react` path and raw `fetch` calls for simplicity.
