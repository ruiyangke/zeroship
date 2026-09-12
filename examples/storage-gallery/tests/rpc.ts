export type Row = Record<string, unknown>;

export async function rpc(origin: string, operation: string, input: Row = {}, timeout = 90_000): Promise<Row> {
  const response = await fetch(`${origin.replace(/\/$/, "")}/__zeroship/v1/${operation}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ json: input }),
    signal: AbortSignal.timeout(timeout),
  });
  if (!response.ok) throw new Error(`${operation}: HTTP ${response.status}: ${await response.text()}`);
  const body = await response.json();
  if (!body?.json || typeof body.json !== "object" || Array.isArray(body.json)) {
    throw new Error(`${operation}: missing RPC result envelope: ${JSON.stringify(body)}`);
  }
  return body.json;
}
