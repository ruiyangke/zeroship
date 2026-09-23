import { auth } from "@zeroship/auth";
import { z } from "zod";
import { cartSchema, fail, marketSchema } from "@gather/meal-kit/domain";
import {
  draftContents,
  draftLoadSchema,
  draftRevisionSchema,
  draftSaveSchema,
  sameRevision,
  type DraftLoad,
  type DraftSave,
} from "@gather/meal-kit/draft-domain";
import { draftVisitor } from "./draft-session";
import { activeDraft, writeDraft } from "./draft-store";
import { transact, type Tx } from "@gather/meal-kit/server/core";

const identityInput = z.object({
  csrf: z.string().min(1).max(100),
  owner: z.string().min(1).max(100).nullable(),
  market: marketSchema,
});
const saveInput = identityInput.extend({
  cart: cartSchema,
  expected: draftRevisionSchema.nullable(),
  requestKey: z.string().uuid(),
});
const claimInput = identityInput.extend({
  source: z.enum(["guest", "account"]),
  expected: draftRevisionSchema.nullable(),
  guest: draftRevisionSchema,
  requestKey: z.string().uuid(),
});

async function identity(
  request: Request,
  input: z.infer<typeof identityInput>,
) {
  const visitor = await draftVisitor(request, input.csrf);
  const owner = auth.getUser()?.id ?? null;
  if (owner !== input.owner)
    fail(
      /* i18n */ "Your sign-in has changed. Reload your saved box to continue.",
      "DRAFT_IDENTITY",
      409,
    );
  return {
    visitor,
    principal: owner ? `user:${owner}` : visitor,
    market: input.market,
    owner,
  };
}
type Identity = Awaited<ReturnType<typeof identity>>;
async function readState(tx: Tx, scope: Identity): Promise<DraftLoad> {
  return {
    draft: activeDraft(
      await tx.meal_carts.get({
        principal: scope.principal,
        market: scope.market,
      }),
    ),
    guest: scope.owner
      ? activeDraft(
          await tx.meal_carts.get({
            principal: scope.visitor,
            market: scope.market,
          }),
        )
      : null,
  };
}
async function loadDraft(
  request: Request,
  input: z.infer<typeof identityInput>,
) {
  const scope = await identity(request, input);
  return transact((tx) => readState(tx, scope));
}

async function saveDraft(
  request: Request,
  input: z.infer<typeof saveInput>,
): Promise<DraftSave> {
  const scope = await identity(request, input);
  if (input.cart.market !== scope.market)
    fail(
      /* i18n */ "Choose a box for this delivery country.",
      "INVALID_MARKET",
    );
  try {
    return await transact(async (tx) => {
      const state = await readState(tx, scope);
      const row = await tx.meal_carts.get({
        principal: scope.principal,
        market: scope.market,
      });
      const key = `save:${input.requestKey}`;
      if (state.draft && row?.last_request_key === key) {
        if (draftContents(state.draft.cart) !== draftContents(input.cart))
          fail(
            /* i18n */ "This save has already been used. Reload your saved box to continue.",
            "KEY_REUSED",
            409,
          );
        return { saved: true, state };
      }
      if (!sameRevision(state.draft, input.expected) || state.guest)
        return { saved: false, state };
      const saved = await writeDraft(tx, row, scope.principal, input.cart, key);
      return { saved: true, state: { draft: activeDraft(saved), guest: null } };
    });
  } catch (error) {
    const state = await transact((tx) => readState(tx, scope));
    if (!sameRevision(state.draft, input.expected))
      return { saved: false, state };
    throw error;
  }
}

async function attachDraft(
  request: Request,
  input: z.infer<typeof claimInput>,
): Promise<DraftSave> {
  const scope = await identity(request, input);
  if (!scope.owner)
    fail(/* i18n */ "Sign in to continue.", "UNAUTHENTICATED", 401);
  try {
    return await transact(async (tx) => {
      const state = await readState(tx, scope);
      const row = await tx.meal_carts.get({
        principal: scope.principal,
        market: scope.market,
      });
      const key = `claim:${input.requestKey}:${input.source}`;
      if (!state.guest && row?.last_request_key === key)
        return { saved: true, state };
      if (
        !state.guest ||
        !sameRevision(state.draft, input.expected) ||
        !sameRevision(state.guest, input.guest)
      )
        return { saved: false, state };
      const cart =
        input.source === "guest" ? state.guest.cart : state.draft?.cart;
      if (!cart) return { saved: false, state };
      const saved = await writeDraft(tx, row, scope.principal, cart, key);
      await tx.meal_carts.purge({
        id: state.guest.id,
        version: state.guest.version,
      });
      return { saved: true, state: { draft: activeDraft(saved), guest: null } };
    });
  } catch (error) {
    const state = await transact((tx) => readState(tx, scope));
    if (
      !sameRevision(state.draft, input.expected) ||
      !sameRevision(state.guest, input.guest)
    )
      return { saved: false, state };
    throw error;
  }
}

export async function draftFetch(request: Request): Promise<Response> {
  const headers = { "Cache-Control": "private, no-store", Vary: "Cookie" };
  const path = new URL(request.url).pathname;
  if (
    !["/api/drafts/load", "/api/drafts/save", "/api/drafts/attach"].includes(
      path,
    )
  )
    return new Response("Not found", { status: 404, headers });
  if (request.method !== "POST")
    return new Response("Method not allowed", {
      status: 405,
      headers: { ...headers, Allow: "POST" },
    });
  if (
    request.headers.get("Content-Type")?.split(";")[0].trim() !==
    "application/json"
  )
    return new Response("Unsupported media type", { status: 415, headers });
  if (
    ["same-site", "cross-site"].includes(
      request.headers.get("Sec-Fetch-Site") ?? "",
    )
  )
    return new Response("Forbidden", { status: 403, headers });
  try {
    const reader = request.body?.getReader();
    let length = 0;
    const chunks: Uint8Array[] = [];
    if (reader)
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        length += value.length;
        if (length > 8192) {
          await reader.cancel();
          return new Response("Request too large", { status: 413, headers });
        }
        chunks.push(value);
      }
    const body = new Uint8Array(length);
    let offset = 0;
    for (const chunk of chunks) {
      body.set(chunk, offset);
      offset += chunk.length;
    }
    const input = JSON.parse(new TextDecoder().decode(body));
    const result = path.endsWith("/load")
      ? draftLoadSchema.parse(
          await loadDraft(request, identityInput.parse(input)),
        )
      : path.endsWith("/save")
        ? draftSaveSchema.parse(
            await saveDraft(request, saveInput.parse(input)),
          )
        : draftSaveSchema.parse(
            await attachDraft(request, claimInput.parse(input)),
          );
    return Response.json(result, { headers });
  } catch (error) {
    const expected =
      error instanceof Error && "status" in error && "code" in error;
    const status = expected
      ? Number(error.status)
      : error instanceof z.ZodError || error instanceof SyntaxError
        ? 400
        : 500;
    return Response.json(
      {
        message: expected ? error.message : "Saved box unavailable",
        code: expected ? error.code : "DRAFT_UNAVAILABLE",
      },
      { status, headers },
    );
  }
}
