import { expect, test } from "@playwright/test";

import { signIn } from "./session";

/**
 * An attachment survives a round trip through `env.storage`, byte for byte.
 *
 * This is the only cluster in the app that touches `env.storage`, and until
 * now nothing exercised it succeeding. scripts/smoke.sh covers the denial
 * arms thoroughly -- content and filenames are both refused to someone who
 * cannot read the bug -- but a refusal proves the guard, not the storage path.
 * A wrong bucket, a mangled key, a base64 round trip that dropped the last
 * block, or a delete that removed the row and left the object would all have
 * passed every check this app had.
 *
 * The content assertion is on the BYTES, not the length or the prefix. A
 * truncated or re-encoded payload has the right size and the right first line.
 */

const RUNTIME_PORT = Number(process.env.ISSUE_TRACKER_API_PORT ?? 3007);

const RUN = `${process.pid}-${Date.now()}`;

test("an attachment uploads, reads back byte-identical, and deletes", async ({
  page,
  baseURL,
  context,
}) => {
  await signIn(context, { runtimePort: RUNTIME_PORT, baseURL: baseURL! });

  const rpc = async (proc: string, json: unknown) => {
    const res = await page.request.post(`${baseURL}/__zeroship/v1/${proc}`, { data: { json } });
    expect(res.status(), `${proc} should succeed`).toBe(200);
    return (await res.json()).json;
  };

  const product = await rpc("products.create", { name: `Attach ${RUN}`, description: "attach" });
  const component = await rpc("components.create", {
    productId: product.id,
    name: "Core",
    description: "core",
  });
  const version = await rpc("versions.create", { productId: product.id, name: "1.0" });
  const bug = await rpc("bugs.create", {
    productId: product.id,
    componentId: component.id,
    versionId: version.id,
    summary: `Has an attachment ${RUN}`,
    description: "d",
  });

  // Deliberately awkward content: a trailing newline, a non-ASCII byte, and a
  // length that is not a multiple of three so the base64 has real padding.
  // A round trip that silently drops the tail passes on tidier input.
  const payload = `crash log ${RUN}\n\tstack: § frame 1\nend\n`;
  const encoded = Buffer.from(payload, "utf8").toString("base64");

  const uploaded = await rpc("attachments.upload", {
    bugId: bug.id,
    filename: "crash.log",
    contentBase64: encoded,
    contentType: "text/plain",
    description: "the log",
  });
  expect(uploaded.sizeBytes, "the recorded size is the byte length, not the base64 length").toBe(
    Buffer.byteLength(payload, "utf8"),
  );

  const listed = await rpc("attachments.list", { bugId: bug.id });
  expect(listed.map((row: { id: string }) => row.id)).toContain(uploaded.id);

  const fetched = await rpc("attachments.get", { id: uploaded.id });
  expect(
    Buffer.from(fetched.contentBase64, "base64").toString("utf8"),
    "the bytes must come back exactly as they went in",
  ).toBe(payload);
  expect(fetched.contentType).toBe("text/plain");

  await rpc("attachments.setObsolete", { id: uploaded.id, isObsolete: true });
  const afterObsolete = await rpc("attachments.list", { bugId: bug.id });
  expect(
    afterObsolete.find((row: { id: string }) => row.id === uploaded.id).isObsolete,
    "obsolete hides it in Bugzilla's UI but must not remove it",
  ).toBe(true);

  await rpc("attachments.delete", { id: uploaded.id });
  const afterDelete = await rpc("attachments.list", { bugId: bug.id });
  expect(afterDelete.map((row: { id: string }) => row.id)).not.toContain(uploaded.id);

  // The object goes with the row. `attachments.delete` removes the stored
  // bytes first and compensates if the transaction then fails, so a delete
  // that only unlinked the row would leave readable content behind -- which
  // for a confidential attachment is the whole point of deleting it.
  const res = await page.request.post(`${baseURL}/__zeroship/v1/attachments.get`, {
    data: { json: { id: uploaded.id } },
  });
  expect(res.status(), "reading a deleted attachment must not succeed").not.toBe(200);
});
