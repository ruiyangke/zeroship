export type Row = Record<string, unknown>;

export async function rpc(origin: string, operation: string, input: Row = {}): Promise<Row> {
  const response = await fetch(`${origin.replace(/\/$/, "")}/__zeroship/v1/${operation}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ json: input }),
    signal: AbortSignal.timeout(10_000),
  });
  if (!response.ok) throw new Error(`${operation}: HTTP ${response.status}: ${await response.text()}`);
  const body = await response.json();
  if (!body?.json || typeof body.json !== "object" || Array.isArray(body.json)) {
    throw new Error(`${operation}: missing RPC result envelope: ${JSON.stringify(body)}`);
  }
  return body.json;
}

export async function listKeys(origin: string, prefix = ""): Promise<string[]> {
  const keys = new Set<string>();
  const cursors = new Set<string>();
  let cursor: string | null = null;
  do {
    const page = await rpc(origin, "kv.keys.list", { prefix, cursor, limit: 2 });
    if (!Array.isArray(page.keys) || page.keys.some((key) => typeof key !== "string")) {
      throw new Error("Listing must return string keys");
    }
    for (const key of page.keys) keys.add(key);
    if (page.cursor !== null && typeof page.cursor !== "string") throw new Error("Missing listing cursor");
    cursor = page.cursor;
    if (cursor !== null) {
      if (cursors.has(cursor)) throw new Error("Listing repeated a cursor without reaching exhaustion");
      cursors.add(cursor);
    }
  } while (cursor !== null);
  return [...keys].sort();
}
