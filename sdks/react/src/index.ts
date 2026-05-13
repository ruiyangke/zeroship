//
// `@zeroship/react` — React bindings on top of @zeroship/db reactive
// queries (P8b stage 4 of the @zeroship/db v2 proposal).
//
// Surface:
//
//   import { useQuery, QueryClientProvider, createDefaultClient }
//     from "@zeroship/react";
//   import { subscribe } from "@zeroship/db";
//
//   const client = createDefaultClient(subscribe);
//
//   function App() {
//     return (
//       <QueryClientProvider client={client}>
//         <Inbox userId={1} />
//       </QueryClientProvider>
//     );
//   }
//
//   function Inbox({ userId }: { userId: number }) {
//     const messages = useQuery(() =>
//       db.messages.find({ userId }).sort({ createdAt: -1 }).limit(50)
//     );
//     if (!messages) return <div>Loading…</div>;
//     return <ul>{messages.map(m => <li key={m.id}>{m.body}</li>)}</ul>;
//   }
//
// The hook is intentionally minimal — no global cache, no retry
// policy, no devtools surface. It bridges the broker subscription to
// React state and nothing more. Heavier-weight integrations (TanStack
// Query, devtools) layer on top.

export {
  useQuery,
  useSuspenseQuery,
  QueryClientProvider,
  createDefaultClient,
} from "./useQuery.js";

export type {
  QueryFactory,
  QueryLike,
  UseQueryOptions,
  UseSuspenseQueryOptions,
  QueryClient,
  QueryClientProviderProps,
  SubscriptionLike,
} from "./useQuery.js";
