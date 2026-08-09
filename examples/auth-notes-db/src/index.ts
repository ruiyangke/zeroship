"use server";

// auth-notes-db — per-user notes backed by env.db, scoped by env.auth.
//
// This is the combination example: `env.auth` (who is calling) + `env.db`
// (durable rows) + RPC (the wire), in one app. Each of those has its own
// single-primitive example already; none of them can show the seam that
// matters in almost every real app:
//
//   a signed-in user writes rows owned by them, and CANNOT read anyone else's.
//
// The ownership rule is enforced in exactly one place — `owner()` reads the
// identity from `env.auth`, and every query filters on it. The client never
// sends a user id, so there is nothing for it to forge.
//
// Procedures (explicit dotted RPC ids → /__zeroship/v1/<id>):
//
//   notes.create   mutation — insert a note owned by the caller
//   notes.list     query    — the caller's own notes, newest first
//   notes.get      query    — one note BY ID, still filtered by owner
//
// `notes.get` is the interesting one. It would be very easy to write it as
// `db.notes.get(id)` and let the list query carry the scoping — which reads
// fine, passes a single-user test, and hands every note in the table to anyone
// who can guess an id. The owner predicate belongs in the query, not in a
// comment.

import { env } from "zeroship";
import { auth } from "@zeroship/auth";
import { mutation, query } from "@zeroship/rpc/server";
import { z } from "@zeroship/server";

/** A note as it goes over the wire. `owner_id` is deliberately NOT included:
 *  the caller already knows who they are, and it is not the client's business. */
export type Note = {
  id: string;
  title: string;
  body: string;
  createdAt: number;
};

/**
 * The authenticated caller's per-app subject (`pws_…`), or a 401.
 *
 * `auth.requireUser()` exists, but it throws a plain `Error` with no `.status`,
 * which the RPC error mapper turns into a 500. A 401 is the honest answer for
 * an anonymous caller, so gate on `getUser()` and throw a status-bearing error.
 */
function owner(): string {
  const user = auth.getUser();
  if (!user) {
    throw Object.assign(new Error("Sign in to use notes"), {
      status: 401,
      code: "UNAUTHENTICATED",
    });
  }
  return user.id;
}

function notFound(): never {
  // 404, not 403: telling a stranger "that note exists but is not yours" leaks
  // the existence of other people's rows. The filter below cannot distinguish
  // the two cases anyway, which is the point.
  throw Object.assign(new Error("Note not found"), {
    status: 404,
    code: "NOT_FOUND",
  });
}

/** env.db rows carry the physical column names; the wire shape is ours. */
function toWire(row: { id: string; title: string; body: string; created_at: number }): Note {
  return { id: row.id, title: row.title, body: row.body, createdAt: row.created_at };
}

export const createNote = mutation(
  async ({ title, body }: { title: string; body: string }): Promise<Note> => {
    const ownerId = owner();
    const { data, error } = await env.db.notes.insert({
      owner_id: ownerId,
      title,
      body,
    });
    if (error) throw error;
    return toWire(data!);
  },
  {
    id: "notes.create",
    input: z.object({
      title: z.string().min(1).max(120),
      body: z.string().max(4000),
    }),
  },
);

export const listNotes = query(
  async (): Promise<{ owner: string; notes: Note[] }> => {
    const ownerId = owner();
    // The owner filter is the query, not a post-filter. A note that is not
    // mine is never fetched in the first place.
    const { data, error } = await env.db.notes
      .find({ owner_id: ownerId })
      .sort({ id: -1 })
      .limit(100);
    if (error) throw error;
    return { owner: ownerId, notes: (data ?? []).map(toWire) };
  },
  { id: "notes.list" },
);

export const getNote = query(
  async ({ id }: { id: string }): Promise<Note> => {
    const ownerId = owner();
    // BOTH predicates, in one filter. Someone else's id simply does not match.
    const { data, error } = await env.db.notes.get({ id, owner_id: ownerId });
    if (error) throw error;
    if (!data) notFound();
    return toWire(data);
  },
  {
    id: "notes.get",
    input: z.object({ id: z.string().min(1) }),
  },
);
