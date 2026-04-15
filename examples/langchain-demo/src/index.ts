"use server";

import { createHash, randomUUID } from "node:crypto";
import { Buffer } from "node:buffer";

export function ping(): string {
  return "pong from V8!";
}

export function testCrypto(): any {
  const hash = createHash("sha256").update("hello world").digest("hex");
  const uuid = randomUUID();
  const buf = Buffer.from("hello", "utf-8").toString("hex");
  return { hash, uuid, buf };
}

export function testBuffer(): any {
  const b = Buffer.from([1, 2, 3, 4]);
  return {
    hex: b.toString("hex"),
    base64: b.toString("base64"),
    length: b.length,
    isBuffer: Buffer.isBuffer(b),
  };
}
