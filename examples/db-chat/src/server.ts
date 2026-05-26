"use server";

// db-chat (server module) — exercises C1 reactive queries + P8b
// read-set narrowing end-to-end.
//
// What this file demonstrates:
//   • export default { schema } with three related collections + t.ref FKs (B2)
//   • query() handler captures read-set via the B3 CURRENT_KIND gate
//     and the per-query Active capture guard — the broker's predicate
//     evaluator (P8b) uses it to skip events that don't match
//   • mutation() writes go through the local-emit path (P8a) AND the
//     WAL pgoutput consumer (P8a.2) when wal_level=logical is on
//   • Subscription cleanup via the Weak finalizer (P8a + native
//     Subscription v8_class in a7261cf) — handle reclaimed at GC
//
// The client (src/App.tsx) uses @zeroship/react's useQuery against
// the `listMessages` proc and auto-rerenders on broker events.

import { t, schema, type Db } from "@zeroship/db";
import { env } from "zeroship";
import { query, mutation, action } from "@zeroship/rpc/server";
import { runQuery, runMutation } from "@zeroship/server";

// ---------------------------------------------------------------------------
// Schema — the `export default { schema }` convention
// ---------------------------------------------------------------------------

const dbSchema = {
  users: {
    handle: t.string().required().unique().pattern(/^[a-z0-9_]+$/),
    name:   t.string().required().max(100),
  },

  channels: schema({
    slug:  t.string().required().unique().pattern(/^[a-z0-9-]+$/),
    name:  t.string().required().max(120),
    topic: t.string(),
  }),

  messages: schema({
    channelId: t.ref("channels").required(),
    authorId:  t.ref("users").required(),
    body:      t.string().required().min(1).max(4000),
    // For P8c moderation flow — content actions can flip this.
    flagged:   t.boolean().default(false),
  }),
};

export default { schema: dbSchema };

const db = env.db as Db<typeof dbSchema>;

type UserId    = typeof db.users.Id;
type ChannelId = typeof db.channels.Id;
type MessageId = typeof db.messages.Id;

// ---------------------------------------------------------------------------
// Queries — read-only. listMessages records its read-set ({channelId}) so
// the broker only fires events for messages in this specific channel.
// ---------------------------------------------------------------------------

// Outside `db.transaction(...)` every Collection method returns
// `Result<T> = { data, error }`. Each handler unwraps so the RPC wire
// carries the bare value; the platform error envelope renders thrown
// errors uniformly.

export const listMessages = query(
  async ({ channelId, limit = 50 }: { channelId: ChannelId; limit?: number }) => {
    const { data, error } = await db.messages
      .find({ channelId, flagged: false })
      .sort({ created_at: -1 })
      .limit(limit);
    if (error) throw error;
    return data ?? [];
  },
);

export const getMessage = query(
  async ({ id }: { id: MessageId }) => {
    const { data, error } = await db.messages.get(id);
    if (error) throw error;
    return data;
  },
);

export const listChannels = query(async (_input: Record<string, never>) => {
  const { data, error } = await db.channels.find({}).sort({ slug: 1 });
  if (error) throw error;
  return data ?? [];
});

// ---------------------------------------------------------------------------
// Mutations — write paths. Each write fires a broker event; if a
// Subscription read-set matches, the subscriber's useQuery re-renders.
// ---------------------------------------------------------------------------

export const sendMessage = mutation(
  async (
    { channelId, authorId, body }: { channelId: ChannelId; authorId: UserId; body: string },
  ) => {
    const { data, error } = await db.messages.insert({ channelId, authorId, body, flagged: false });
    if (error) throw error;
    return data;
  },
);

export const flagMessage = mutation(
  async ({ id }: { id: MessageId }) => {
    const { data, error } = await db.messages.update(id, { flagged: true });
    if (error) throw error;
    return data;
  },
);

export const createChannel = mutation(
  async (
    { slug, name, topic }: { slug: string; name: string; topic?: string },
  ) => {
    const { data, error } = await db.channels.insert({ slug, name, topic });
    if (error) throw error;
    return data;
  },
);

// Seed helper — used by smoke.sh to provision a user.
export const createUser = mutation(
  async ({ handle, name }: { handle: string; name: string }) => {
    const { data, error } = await db.users.insert({ handle, name });
    if (error) throw error;
    return data;
  },
);

// ---------------------------------------------------------------------------
// Action — moderation call to an external service. Cannot directly
// write the DB; composes via ctx.runMutation.
// ---------------------------------------------------------------------------

export const moderateMessage = action(
  async ({ id, moderationUrl }: { id: MessageId; moderationUrl: string }) => {
    const message = (await runQuery(getMessage, { id })) as
      | { body: string }
      | null;
    if (!message) return { handled: false, reason: "not_found" };

    const resp = await fetch(moderationUrl, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ body: message.body }),
    });
    const result = (await resp.json()) as { unsafe?: boolean };

    if (result.unsafe) {
      await runMutation(flagMessage, { id });
      return { handled: true, flagged: true };
    }
    return { handled: true, flagged: false };
  },
);
