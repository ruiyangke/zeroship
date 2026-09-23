import type { MarketId } from "@gather/meal-kit/catalog";
import type { DraftTransport } from "./draft-controller";
import {
  draftRevision,
  draftLoadSchema,
  draftSaveSchema,
} from "@gather/meal-kit/draft-domain";

export function draftTransport(market: MarketId): DraftTransport {
  let session: Promise<{ csrf: string }> | undefined;
  const proof = () =>
    (session ??= fetch("/api/draft-session", {
      method: "POST",
      credentials: "same-origin",
      headers: { "X-Gather-Session": "init" },
    })
      .then(async (response) => {
        if (!response.ok) throw new Error("Draft session unavailable");
        const value = await response.json();
        if (typeof value.csrf !== "string")
          throw new Error("Draft session unavailable");
        return value as { csrf: string };
      })
      .catch((error) => {
        session = undefined;
        throw error;
      }));
  const run = async <T>(fn: (csrf: string) => PromiseLike<T>) => {
    try {
      return await fn((await proof()).csrf);
    } catch (error) {
      session = undefined;
      throw error;
    }
  };
  const post = async (path: string, body: unknown) => {
    const response = await fetch(`/api/drafts/${path}`, {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!response.ok) throw new Error("Saved box unavailable");
    return response.json();
  };
  return {
    load: (owner) =>
      run(async (csrf) =>
        draftLoadSchema.parse(await post("load", { market, owner, csrf })),
      ),
    save: (owner, cart, expected, requestKey) =>
      run(async (csrf) =>
        draftSaveSchema.parse(
          await post("save", {
            market,
            owner,
            csrf,
            cart,
            expected,
            requestKey,
          }),
        ),
      ),
    attach: (owner, source, state, requestKey) =>
      run(async (csrf) =>
        draftSaveSchema.parse(
          await post("attach", {
            market,
            owner,
            csrf,
            source,
            expected: draftRevision(state.draft),
            guest: draftRevision(state.guest)!,
            requestKey,
          }),
        ),
      ),
  };
}
