import { randomUUID } from "node:crypto";
import { expect, test } from "vitest";
import { rpc } from "./rpc";
import { targets } from "./targets";

test("gallery CRUD preserves bytes, metadata, ordering and absence on every backend", async () => {
  for (const target of targets()) {
    const call = (op: string, input = {}) => rpc(target.apiUrl, `gallery.${op}`, input);
    const prefix = `acceptance/${randomUUID()}/`;
    const key = `${prefix}hello.txt`;
    const text = "hello storage — 中文";
    const bytes = Buffer.from(text);
    expect(await call("put", { key, text, contentType: "text/plain" })).toMatchObject({ key, size: bytes.length, bucket: "gallery" });
    expect(await call("get", { key })).toEqual({ key, found: true, text, bytesBase64: bytes.toString("base64"), contentType: "text/plain", size: bytes.length });
    expect(await call("list", { prefix })).toMatchObject({ entries: [expect.objectContaining({ key, size: bytes.length })], cursor: null });
    expect(await call("put", { key, text: "short", contentType: "application/json" })).toMatchObject({ size: 5 });
    expect(await call("get", { key })).toMatchObject({ text: "short", size: 5, contentType: "application/json" });
    expect(await call("delete", { key })).toEqual({ key, deleted: true });
    expect(await call("delete", { key })).toEqual({ key, deleted: false });
    expect(await call("get", { key })).toMatchObject({ found: false });
    expect(await call("list", { prefix })).toEqual({ entries: [], cursor: null });
    const binary = Buffer.from(Array.from({ length: 256 }, (_, i) => i));
    expect(await call("put", { key, bytesBase64: binary.toString("base64") })).toMatchObject({ size: binary.length });
    expect(await call("get", { key })).toMatchObject({ bytesBase64: binary.toString("base64"), text: null });
    await call("delete", { key });
  }
});

test("multipart objects stream through the SDK and deployed worker without corruption", async () => {
  const target = targets().find((target) => target.name === "s3")!;
  const key = `multipart/${randomUUID()}.bin`;
  const size = 20 * 1024 * 1024;
  const uploaded = await rpc(target.apiUrl, "gallery.putLarge", { key, sizeBytes: size, seed: 7 });
  expect(uploaded).toMatchObject({ key, size });
  expect(uploaded.checksum).toMatch(/^[0-9a-f]+$/);
  const downloaded = await rpc(target.apiUrl, "gallery.getLargeHash", { key });
  expect(downloaded).toEqual({ key, found: true, size, checksum: uploaded.checksum });
  expect(await rpc(target.apiUrl, "gallery.delete", { key })).toEqual({ key, deleted: true });
});
