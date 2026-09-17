import { createHash, createPrivateKey, createPublicKey, randomBytes, sign } from "node:crypto";
import { request } from "node:http";
import { text } from "node:stream/consumers";
import { inject } from "vitest";
import type { Row } from "./rpc";


// Match the gateway's service assertion and dispatch-frame wire contracts.
// Long transfers use the worker directly; browser and multipart cases use the gateway.
export async function workerRpc(operation: string, input: Row, timeout: number): Promise<Row> {
  const worker = inject("storageWorker");
  const key = createPrivateKey(worker.gatewayKey);
  const publicKey = createPublicKey(key).export({ format: "jwk" });
  const kid = createHash("sha256")
    .update(JSON.stringify({ crv: "Ed25519", kty: "OKP", x: publicKey.x }))
    .digest("base64url");
  const encode = (value: unknown) => Buffer.from(JSON.stringify(value)).toString("base64url");
  const now = Math.floor(Date.now() / 1000);
  const issuer = "spiffe://zeroship.ai/svc/gateway";
  const unsigned = `${encode({ alg: "EdDSA", typ: "svc-assertion+jwt", kid })}.${encode({
    iss: issuer, sub: issuer, aud: "spiffe://zeroship.ai/svc/worker",
    iat: now, exp: now + 60, jti: randomBytes(16).toString("base64url"),
  })}`;
  const assertion = `${unsigned}.${sign(null, Buffer.from(unsigned), key).toString("base64url")}`;
  const metadata = Buffer.from(JSON.stringify({
    method: "POST", url: `http://gallery.localhost/__zeroship/v1/${operation}`,
    headers: [["content-type", "application/json"]],
  }));
  const length = Buffer.alloc(4);
  length.writeUInt32LE(metadata.length);
  const body = Buffer.concat([length, metadata, Buffer.from(JSON.stringify({ json: input }))]);
  // Use the test's deadline for the entire operation, including response headers.
  const response = await new Promise<{ status: number | undefined; body: string }>((resolve, reject) => {
    const req = request(`${worker.url}/dispatch/${worker.appId}`, {
      method: "POST", headers: {
        authorization: `Bearer ${assertion}`, "content-type": "application/octet-stream", "content-length": body.length,
      },
      signal: AbortSignal.timeout(timeout),
    }, (incoming) => {
      text(incoming).then((body) => resolve({ status: incoming.statusCode, body }), reject);
    });
    req.once("error", reject);
    req.end(body);
  });
  if (response.status !== 200) throw new Error(`${operation}: worker HTTP ${response.status}: ${response.body}`);
  const result = JSON.parse(response.body);
  if (!result?.json || typeof result.json !== "object" || Array.isArray(result.json)) {
    throw new Error(`${operation}: missing RPC result envelope: ${JSON.stringify(result)}`);
  }
  return result.json;
}
