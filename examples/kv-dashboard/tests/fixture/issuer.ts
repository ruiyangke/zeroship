import { generateKeyPairSync, randomUUID, sign } from "node:crypto";
import { GenericContainer, Wait } from "testcontainers";

export function issuer() {
  const { publicKey, privateKey } = generateKeyPairSync("ed25519");
  const jwks = { keys: [{ ...publicKey.export({ format: "jwk" }), alg: "EdDSA", use: "sig", kid: "kv-acceptance" }] };
  return {
    container: new GenericContainer("nginx:1")
      .withExposedPorts(80)
      .withWaitStrategy(Wait.forLogMessage("start worker processes"))
      .withCopyContentToContainer([{
        content: JSON.stringify(jwks),
        target: "/usr/share/nginx/html/.well-known/jwks.json",
      }]),
    bearer(url: string, owner: string): string {
      const now = Math.floor(Date.now() / 1000);
      const encode = (value: unknown) => Buffer.from(JSON.stringify(value)).toString("base64url");
      const body = `${encode({ alg: "EdDSA", typ: "at+jwt", kid: "kv-acceptance" })}.${encode({
        iss: url, aud: "control.zeroship.ai", sub: owner,
        iat: now, nbf: now - 1, exp: now + 3600,
        client_id: "zeroship-console", jti: randomUUID(),
        scope: "organization:create apps:read apps:write apps:deploy deployments:read secrets:read",
      })}`;
      return `${body}.${sign(null, Buffer.from(body), privateKey).toString("base64url")}`;
    },
  };
}
