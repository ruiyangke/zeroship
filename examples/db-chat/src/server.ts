"use server";

// db-chat server module.
//
// What this file demonstrates:
//   • export default { schema } with three related collections + t.ref FKs
//   • query/mutation/action wrappers from @zeroship/rpc/server
//   • Result<T> unwrapping before returning through the RPC wire
//   • channel-scoped queries that the React example can refresh
//
// The client (src/App.tsx) uses the older @zeroship/react hook path
// against `listMessages`.

import { t, schema } from "@zeroship/db";
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

const db = env.db; // typed Db<typeof dbSchema> via @zeroship/db/env

type UserId    = typeof db.users.Id;
type ChannelId = typeof db.channels.Id;
type MessageId = typeof db.messages.Id;

// ---------------------------------------------------------------------------
// Queries.
// ---------------------------------------------------------------------------

// Outside `db.transaction(...)` every Collection method returns
// `Result<T> = { data, error }`. Each handler unwraps so the RPC wire
// carries the bare value; the platform error envelope renders thrown
// errors uniformly.

export const listMessages = query(
  async ({ channelId, limit = 50 }: { channelId: ChannelId; limit?: number }) => {
    const { data, error } = await db.messages
      .find({ channelId, flagged: false })
      .sort({ id: -1 })
      .limit(limit);
    if (error) throw error;
    return data ?? [];
  },
  { id: "listMessages" },
);

export const getMessage = query(
  async ({ id }: { id: MessageId }) => {
    const { data, error } = await db.messages.get(id);
    if (error) throw error;
    return data;
  },
  { id: "getMessage" },
);

export const listChannels = query(
  async (_input: Record<string, never>) => {
    const { data, error } = await db.channels.find({}).sort({ slug: 1 });
    if (error) throw error;
    return data ?? [];
  },
  { id: "listChannels" },
);

// ---------------------------------------------------------------------------
// Mutations.
// ---------------------------------------------------------------------------

export const sendMessage = mutation(
  async (
    { channelId, authorId, body }: { channelId: ChannelId; authorId: UserId; body: string },
  ) => {
    const { data, error } = await db.messages.insert({ channelId, authorId, body, flagged: false });
    if (error) throw error;
    return data;
  },
  { id: "sendMessage" },
);

export const flagMessage = mutation(
  async ({ id }: { id: MessageId }) => {
    const { data, error } = await db.messages.update(id, { flagged: true });
    if (error) throw error;
    return data;
  },
  { id: "flagMessage" },
);

export const createChannel = mutation(
  async (
    { slug, name, topic }: { slug: string; name: string; topic?: string },
  ) => {
    const { data, error } = await db.channels.insert({ slug, name, topic });
    if (error) throw error;
    return data;
  },
  { id: "createChannel" },
);

// Seed helper — used by smoke.sh to provision a user.
export const createUser = mutation(
  async ({ handle, name }: { handle: string; name: string }) => {
    const { data, error } = await db.users.insert({ handle, name });
    if (error) throw error;
    return data;
  },
  { id: "createUser" },
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
  { id: "moderateMessage" },
);
