"use server";

// db-v2-chat (server module) — exercises C1 reactive queries + P8b
// read-set narrowing end-to-end.
//
// What this file demonstrates:
//   • createDb({...}) with three related collections + t.ref FKs (B2)
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

import { createDb, t, schema, type Id } from "@zeroship/db";
import { query, mutation, action } from "@zeroship/server";

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

export const db = createDb({
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
});

type UserId    = Id<"users">;
type ChannelId = Id<"channels">;
type MessageId = Id<"messages">;

// ---------------------------------------------------------------------------
// Queries — read-only. listMessages records its read-set ({channelId}) so
// the broker only fires events for messages in this specific channel.
// ---------------------------------------------------------------------------

export const listMessages = query(
  async ({ channelId, limit = 50 }: { channelId: ChannelId; limit?: number }, ctx) => {
    const col = (ctx.db as any).messages;
    return col
      .find({ channelId, flagged: false })
      .sort({ createdAt: -1 })
      .limit(limit);
  },
);

export const getMessage = query(
  async ({ id }: { id: MessageId }, ctx) => {
    const col = (ctx.db as any).messages;
    return col.findOne({ id });
  },
);

export const listChannels = query(async (_input: Record<string, never>, ctx) => {
  const col = (ctx.db as any).channels;
  return col.find({}).sort({ slug: 1 });
});

// ---------------------------------------------------------------------------
// Mutations — write paths. Each write fires a broker event; if a
// Subscription read-set matches, the subscriber's useQuery re-renders.
// ---------------------------------------------------------------------------

export const sendMessage = mutation(
  async (
    { channelId, authorId, body }: { channelId: ChannelId; authorId: UserId; body: string },
    ctx,
  ) => {
    const col = (ctx.db as any).messages;
    return col.create({ channelId, authorId, body, flagged: false });
  },
);

export const flagMessage = mutation(
  async ({ id }: { id: MessageId }, ctx) => {
    const col = (ctx.db as any).messages;
    return col.updateOne({ id }, { flagged: true });
  },
);

export const createChannel = mutation(
  async (
    { slug, name, topic }: { slug: string; name: string; topic?: string },
    ctx,
  ) => {
    const col = (ctx.db as any).channels;
    return col.create({ slug, name, topic });
  },
);

// ---------------------------------------------------------------------------
// Action — moderation call to an external service. Cannot directly
// write the DB; composes via ctx.runMutation.
// ---------------------------------------------------------------------------

export const moderateMessage = action(
  async ({ id, moderationUrl }: { id: MessageId; moderationUrl: string }, ctx) => {
    const message = (await ctx.runQuery(getMessage as any, { id })) as
      | { body: string }
      | null;
    if (!message) return { handled: false, reason: "not_found" };

    const resp = await ctx.fetch(moderationUrl, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ body: message.body }),
    });
    const result = (await resp.json()) as { unsafe?: boolean };

    if (result.unsafe) {
      await ctx.runMutation(flagMessage as any, { id });
      return { handled: true, flagged: true };
    }
    return { handled: true, flagged: false };
  },
);
