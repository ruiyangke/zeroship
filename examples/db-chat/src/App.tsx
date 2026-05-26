// db-chat client — exercises @zeroship/react useQuery on top of the
// server's RPC procedures.
//
// What this file demonstrates:
//   • useQuery re-runs the supplied fetcher when the DB layer publishes
//     a relevant change.
//   • Two clients viewing different channels keep independent state.
//
// To run:
//   pnpm dev          — vite-plugin bootstraps the dev runtime
//   open localhost:3000 in two tabs, each with a different ?channel=
//   query param; send messages from one; only that tab's UI updates.

import React, { useEffect, useState } from "react";
import {
  QueryClientProvider,
  createDefaultClient,
  useQuery,
} from "@zeroship/react";

const client = createDefaultClient();

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
  // The hook re-runs the fetcher when the reactive DB layer publishes
  // a matching change.
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
