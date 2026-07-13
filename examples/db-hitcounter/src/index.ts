import { env } from "zeroship";

type JsonBody = Record<string, unknown>;

function json(body: JsonBody, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

export default {
  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const inserted = await env.db.hits.insert({ path: url.pathname });
    const wrote = !inserted.error;

    let readBack = 0;
    let readError: string | null = null;
    if (wrote) {
      const found = await env.db.hits.find({}).limit(1);
      if (found.error) {
        readError = found.error.message;
      } else {
        readBack = found.data?.length ?? 0;
      }
    }

    return json({
      wrote,
      readBack,
      insertedId: inserted.data?.id ?? null,
      path: url.pathname,
      error: inserted.error?.message ?? readError,
    });
  },
};
