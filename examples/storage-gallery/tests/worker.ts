import { createHash, createPrivateKey, createPublicKey, randomBytes, sign } from "node:crypto";
import { inject } from "vitest";
import type { Row } from "./rpc";

function appPath(uuid: string): string {
  const alphabet = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
  let value = BigInt(`0x${uuid.replaceAll("-", "")}`);
  let encoded = "";
  while (value) {
    encoded = alphabet[Number(value % 62n)] + encoded;
    value /= 62n;
  }
  return `app_${encoded.padStart(22, "0")}`;
}

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
  const response = await fetch(`${worker.url}/dispatch/${appPath(worker.appId)}`, {
    method: "POST", headers: { authorization: `Bearer ${assertion}`, "content-type": "application/octet-stream" },
    body, signal: AbortSignal.timeout(timeout),
  });
  if (!response.ok) throw new Error(`${operation}: worker HTTP ${response.status}: ${await response.text()}`);
  const result = await response.json();
  if (!result?.json || typeof result.json !== "object" || Array.isArray(result.json)) {
    throw new Error(`${operation}: missing RPC result envelope: ${JSON.stringify(result)}`);
  }
  return result.json;
}
