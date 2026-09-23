import { kv } from "@zeroship/kv";
import { fail } from "@gather/meal-kit/domain";
import { draftLifetimeMs } from "@gather/meal-kit/draft-domain";
import { must } from "@gather/meal-kit/server/core";

const encoder = new TextEncoder();
let keyPromise: Promise<CryptoKey> | undefined;
function encode(bytes: Uint8Array) {
  return btoa(String.fromCharCode(...bytes))
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replaceAll("=", "");
}
function decode(value: string) {
  if (!/^[a-zA-Z0-9_-]+$/.test(value)) throw new Error("Invalid signature");
  return Uint8Array.from(
    atob(value.replaceAll("-", "+").replaceAll("_", "/")),
    (c) => c.charCodeAt(0),
  );
}
async function signingKey() {
  if (!keyPromise)
    keyPromise = (async () => {
      const keyName = "gather:draft-signing-key";
      let stored = must(await kv.get<string>(keyName));
      if (!stored) {
        const candidate = encode(crypto.getRandomValues(new Uint8Array(32)));
        must(await kv.setIfAbsent(keyName, candidate));
        stored = must(await kv.get<string>(keyName));
      }
      if (!stored) throw new Error("Draft signing key unavailable");
      return crypto.subtle.importKey(
        "raw",
        decode(stored),
        { name: "HMAC", hash: "SHA-256" },
        false,
        ["sign", "verify"],
      );
    })().catch((error) => {
      keyPromise = undefined;
      throw error;
    });
  return keyPromise;
}
async function sign(value: string) {
  return encode(
    new Uint8Array(
      await crypto.subtle.sign(
        "HMAC",
        await signingKey(),
        encoder.encode(value),
      ),
    ),
  );
}
async function verify(value: string, signature: string) {
  try {
    return await crypto.subtle.verify(
      "HMAC",
      await signingKey(),
      decode(signature),
      encoder.encode(value),
    );
  } catch {
    return false;
  }
}
function cookieOptions(request: Request) {
  const local = ["localhost", "127.0.0.1", "[::1]"].includes(
    new URL(request.url).hostname,
  );
  return {
    name: local ? "gather_draft" : "__Host-gather-draft",
    secure: !local,
  };
}
async function readSession(request: Request) {
  const { name } = cookieOptions(request);
  const values = (request.headers.get("cookie") ?? "")
    .split(";")
    .map((part) => part.trim())
    .filter((part) => part.startsWith(name + "="));
  if (values.length !== 1) return null;
  const parts = values[0].slice(name.length + 1).split(".");
  if (parts.length !== 3) return null;
  const [id, expiry, signature] = parts;
  if (
    !/^[a-f0-9-]{36}$/.test(id) ||
    !/^\d+$/.test(expiry) ||
    Number(expiry) <= Date.now()
  )
    return null;
  const payload = `${id}.${expiry}`;
  return (await verify("draft-session:" + payload, signature))
    ? { id, payload }
    : null;
}
export async function draftVisitor(request: Request, csrf: string) {
  const session = await readSession(request);
  if (!session || !(await verify("draft-csrf:" + session.payload, csrf)))
    fail(
      /* i18n */ "Your saved box session has ended. Reload this page to continue.",
      "DRAFT_SESSION",
      403,
    );
  return `visitor:${session.id}`;
}
export async function draftSessionFetch(request: Request): Promise<Response> {
  if (new URL(request.url).pathname !== "/api/draft-session")
    return new Response("Not found", { status: 404 });
  if (request.method !== "POST")
    return new Response("Method not allowed", {
      status: 405,
      headers: { Allow: "POST", "Cache-Control": "no-store" },
    });
  if (
    request.headers.get("X-Gather-Session") !== "init" ||
    ["same-site", "cross-site"].includes(
      request.headers.get("Sec-Fetch-Site") ?? "",
    )
  )
    return new Response("Forbidden", {
      status: 403,
      headers: { "Cache-Control": "no-store" },
    });
  let session = await readSession(request);
  const headers = new Headers({
    "Content-Type": "application/json",
    "Cache-Control": "private, no-store",
    Vary: "Cookie",
  });
  if (!session) {
    const id = crypto.randomUUID();
    const payload = `${id}.${Date.now() + draftLifetimeMs}`;
    const signature = await sign("draft-session:" + payload);
    const { name, secure } = cookieOptions(request);
    headers.set(
      "Set-Cookie",
      `${name}=${payload}.${signature}; Path=/; HttpOnly; SameSite=Lax; Max-Age=${Math.floor(draftLifetimeMs / 1000)}${secure ? "; Secure" : ""}`,
    );
    session = { id, payload };
  }
  return Response.json(
    { csrf: await sign("draft-csrf:" + session.payload) },
    { headers },
  );
}
