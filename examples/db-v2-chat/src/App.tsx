// db-v2-chat client — exercises @zeroship/react useQuery on top of the
// server's reactive query primitives.
//
// What this file demonstrates:
//   • useQuery wraps a Subscription that auto-rerenders on broker
//     events matching the read-set captured by the query handler.
//   • Two clients viewing different channels do NOT see each other's
//     messages (P8b read-set narrowing).
//   • A mutation on worker A delivers events to a subscriber on worker
//     B via the pgoutput WAL consumer (P8a.2). Cross-worker propagation
//     is invisible to the React layer — it just re-renders.
//
// To run:
//   npm run dev       — vite-plugin bootstraps the worker + WAL consumer
//   open localhost:3000 in two tabs, each with a different ?channel=
//   query param; send messages from one; only that tab's UI updates.

import React, { useEffect, useState } from "react";
import {
  QueryClientProvider,
  createDefaultClient,
  useQuery,
  type SubscriptionLike,
} from "@zeroship/react";
import { subscribe } from "@zeroship/db";

// `subscribe(collection)` returns the SubscriptionLike the React client
// drives. The default-client adapter coerces our @zeroship/db Subscription
// to the shape useQuery expects.
const client = createDefaultClient(
  (collection: string): SubscriptionLike =>
    subscribe(collection) as unknown as SubscriptionLike,
);

// In a real app, the RPC procedures are imported from a generated
// client. This example calls them via plain fetch() to /_zs/v1/<name>
// for simplicity — the server module's exports are in `./server.ts`.
async function rpc<T>(proc: string, input: unknown): Promise<T> {
  const r = await fetch(`/_zs/v1/${proc}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ json: input }),
  });
  const env = await r.json();
  if (env.error) throw new Error(env.error);
  return env.json as T;
}

type Message = {
  id: number;
  channelId: number;
  authorId: number;
  body: string;
  createdAt: number;
};

function ChannelView({ channelId }: { channelId: number }) {
  // The hook re-runs the factory on every broker event whose tuple
  // matches the read-set captured by listMessages (which records
  // {channelId} via the B3 CURRENT_KIND gate + read_set::Active guard).
  const messages = useQuery<Message[]>(
    () => rpc<Message[]>("listMessages", { channelId }),
    { collection: "messages" },
  );

  const [draft, setDraft] = useState("");
  const send = async () => {
    if (!draft.trim()) return;
    await rpc("sendMessage", { channelId, authorId: 1, body: draft });
    setDraft("");
  };

  if (messages === undefined) return <div>Loading…</div>;

  return (
    <div>
      <h2>Channel #{channelId}</h2>
      <ul>
        {messages.map((m) => (
          <li key={m.id}>
            <strong>#{m.authorId}</strong>: {m.body}
          </li>
        ))}
      </ul>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          send();
        }}
      >
        <input
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          placeholder="Type a message…"
        />
        <button type="submit">Send</button>
      </form>
    </div>
  );
}

export default function App() {
  const params = new URLSearchParams(window.location.search);
  const channelId = Number(params.get("channel") ?? "1");

  return (
    <QueryClientProvider client={client}>
      <ChannelView channelId={channelId} />
    </QueryClientProvider>
  );
}
