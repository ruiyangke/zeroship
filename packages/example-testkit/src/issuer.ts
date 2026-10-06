import { generateKeyPairSync, randomUUID, sign } from "node:crypto";
import { GenericContainer, Wait } from "testcontainers";

export interface IssuerOptions {
  /** Key id the JWKS document and every access token carry. */
  kid: string;
  /** The scope claim narrowing the actions the bearer may take. */
  scope: string;
}

/**
 * The platform identity provider as Control sees it: a JWKS document it
 * fetches, and access tokens signed by the matching key. The creator bearer
 * the deploy uses is one of those tokens.
 */
export function issuer(options: IssuerOptions) {
  const { publicKey, privateKey } = generateKeyPairSync("ed25519");
  const jwks = { keys: [{ ...publicKey.export({ format: "jwk" }), alg: "EdDSA", use: "sig", kid: options.kid }] };
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
      const body = `${encode({ alg: "EdDSA", typ: "at+jwt", kid: options.kid })}.${encode({
        iss: url, aud: "control.zeroship.ai", sub: owner,
        iat: now, nbf: now - 1, exp: now + 3600,
        client_id: "zeroship-console", jti: randomUUID(),
        scope: options.scope,
      })}`;
      return `${body}.${sign(null, Buffer.from(body), privateKey).toString("base64url")}`;
    },
  };
}
