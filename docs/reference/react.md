# `@zeroship/react` — React bindings for live queries

`@zeroship/react` turns a database query into React state that stays in step
with the database. You hand it a query factory; it runs the factory, renders
the result, and re-runs the factory whenever the underlying collection
changes. [db.md](db.md#live-queries-dblive) owns the query and subscription
contracts underneath; this page is the React layer on top of them.

The package is ESM-only and exports named bindings only — there is no default
export. It declares two peer dependencies: `@zeroship/db` and `react`
(`^18.0.0 || ^19.0.0`). `react-dom` is not a peer.

| Export | What it is |
| --- | --- |
| `useQuery` | Subscribe a component to a reactive query. |
| `useSuspenseQuery` | The Suspense-friendly variant. |
| `QueryClientProvider` | Supplies the query client to the hooks below it. |
| `createDefaultClient` | Builds the default query client. |

Exported types: `QueryFactory`, `QueryLike`, `UseQueryOptions`,
`UseSuspenseQueryOptions`, `QueryClient`, `QueryClientProviderProps`,
`SubscriptionLike`.

The query client is the object that opens a collection subscription. Build one
with `createDefaultClient()`, which takes no required arguments and returns a
`QueryClient`; it does not connect on creation — the first hook that subscribes
opens the subscription. Its one optional argument overrides the subscription
source and exists for tests and stubs. When the default subscription cannot be
opened, the component observes the failure exactly as described under
[The factory's collection](#the-factorys-collection).

## The simplest component

```tsx
import { env } from "zeroship";
import {
  QueryClientProvider,
  createDefaultClient,
  useQuery,
} from "@zeroship/react";

// One client for the app. Build it once, at module scope.
const client = createDefaultClient();

// The app's typed database handle, from the generated env.db module.
const db = env.db;

function Inbox() {
  const messages = useQuery(() =>
    db.messages.find({}).sort({ createdAt: -1 }).limit(50),
  );

  if (messages === undefined) return <p>Loading…</p>;

  return (
    <ul>
      {messages.map((m) => (
        <li key={m.id}>{m.body}</li>
      ))}
    </ul>
  );
}

export default function App() {
  return (
    <QueryClientProvider client={client}>
      <Inbox />
    </QueryClientProvider>
  );
}
```

Three things make this work:

1. **Wrap the tree in `QueryClientProvider`.** A `useQuery` with no provider
   in context logs `@zeroship/react useQuery: no QueryClient in context. Wrap
   your tree in <QueryClientProvider client={...}>.` and stays at `undefined`
   forever.
2. **Pass a factory, not a value.** `useQuery` calls the function; it does not
   accept an already-awaited result.
3. **Return a database query or a `Promise`.** When the factory returns a
   database query, the hook subscribes to the collection that query targets.
   When it returns a plain `Promise`, name the collection with the
   `collection` option (below).

`db` is the app's typed `env.db` handle, imported from the `zeroship` runtime
module. Its `Env.db` typing comes from the generated module and its query and
subscription contracts belong to [db.md](db.md). A factory that instead calls an
RPC and returns a plain `Promise` names the collection explicitly; see
[The factory's collection](#the-factorys-collection).

## `useQuery`

```ts
const result = useQuery<T>(factory, options?);
```

`result` is `T | undefined`. There is no error field and no status field: the
last resolved result is the hook's only output.

- `collection` — collection to subscribe to. Required when the factory returns
  a plain `Promise`; ignored when it returns a database query.
- `initialData` — value returned on the very first render, before the factory
  resolves. Defaults to `undefined`; use it for SSR hydration.

### What the component sees

- **First render** returns `undefined`, unless `initialData` is supplied.
- **After the factory resolves** the component renders the result.
- **While a change is being refetched** the previous result stays on screen —
  the hook does not fall back to `undefined` between snapshots.
- **On failure with no result yet rendered** the next render throws so the
  nearest React error boundary catches it. The thrown value is the underlying
  error when it is an `Error` — including any `code` it carries — and otherwise
  an `Error` wrapping its string form.
- **On failure after a result has rendered** the last good result stays on
  screen and the failure is not surfaced on that render. Nothing is thrown,
  returned, logged or otherwise exposed: there is no error field, no callback
  and no staleness flag, so a component cannot tell a live result from one
  whose updates have stopped. See [Limits and non-goals](#limits-and-non-goals).

The result is unwrapped the same way for both factory shapes: an object carrying
a `data` or `error` key is treated as an envelope — a truthy `error` is the
failure, otherwise `data` is the result — and any other value is used directly.

### The factory's collection

A **collection** is a table in your app's schema. Its name is the table name
you declared in `migrations/`; the naming rules and limits are under
[Collection names](db.md#collection-names) in [db.md](db.md). The React layer
treats the name as an opaque string: for a database query it reads the target
collection from the query, and for a plain `Promise` you supply it.

- **Database query** — the collection is detected automatically; the
  `collection` option is ignored.
- **Plain `Promise`** — pass `{ collection: "messages" }`. Without it the hook
  still resolves the first result, but opens no subscription, so the component
  never updates.
- **Opening the subscription fails** — the hook still resolves the first
  result, then clears the failure once it lands. The component renders data
  that never updates, and the failure is not surfaced: there is no error, no
  log and no liveness signal to observe it.

### Inputs and re-subscription

The hook subscribes once per component instance and does not re-subscribe on
its own. A new factory closure — for example one that closes over a changed
`userId` prop — does **not** refetch by itself; the component keeps rendering
the previous result until the next change event, at which point the new
closure runs. To switch collections or force an immediate refetch when an
input changes, remount the component with a `key` prop:

```tsx
<Inbox key={userId} userId={userId} />
```

### StrictMode

Under React StrictMode in development, the mount/unmount/re-mount cycle leaves
exactly one live subscription per component instance, and unmount closes it.

## `useSuspenseQuery`

```ts
const result = useSuspenseQuery<T>(factory, {
  suspenseKey: `messages-user-${userId}`,
  // any other useQuery option; initialData comes from the cache
});
```

The Suspense-friendly variant. On the first render it throws a promise; a
surrounding `<Suspense>` renders its fallback until the promise resolves, then
the component re-renders with the result. From then on it behaves like
`useQuery`, re-running the factory on every change.

`suspenseKey` is required and must be a non-empty string; anything else throws
a `TypeError`. Derive it from the collection and the query's inputs so each
distinct query gets its own key.

**The cache behind `suspenseKey` is process-wide and never evicted.** The
first query resolved for a key becomes that key's cached value for the life of
the page. Two components that share a key share that value — and a key reused
for a different query starts from the wrong value before the refetch lands.
Give every distinct query its own key.

## Reconnect and resubscribe

The hook is a consumer of the subscription; it does not reconnect on its own,
and there is nothing in the React layer to configure to make it. Transport-level
reconnect and resynchronization belong to the subscription surface, not the
React layer; [db.md](db.md#live-queries-dblive) documents that lifecycle. What
the hook does per event:

- **A change** re-runs the factory and commits the fresh result. This is
  coarse-grained: every insert, update or delete on the subscribed collection
  re-runs the factory, even when the changed row does not match the query's
  filter. See [db.md](db.md#live-queries-dblive).
- **A resync** — emitted after connection loss, queue overflow or a transport
  reconnect — re-runs the factory and commits a fresh snapshot.
- **A closed event** triggers one final refetch, then the hook stops
  listening.
- **A subscription that ends or throws** stops the hook permanently. It does
  not reopen. The last rendered result stays on screen, and the stop is not
  surfaced: nothing is thrown or returned, and there is no liveness signal to
  distinguish a live result from one that has stopped updating. Recovery is the
  caller's responsibility — remount the component or rebuild the client.

## Limits and non-goals

- **One subscription per component instance.** There is no multiplexing across
  components.
- **No shared query cache** other than the Suspense cache. Two components
  running the same query keep independent state.
- **No retry and no devtools.** The hook exposes no error handle once a result
  has rendered, so recovery from a failed or stopped subscription is left to
  the caller: remount the component or rebuild the client.
- **No automatic refetch on input change.** Remount with a `key` to switch
  queries.
- **The collection must be nameable.** A factory that transforms data fetched
  elsewhere must pass `collection` explicitly; see
  [The factory's collection](#the-factorys-collection).
