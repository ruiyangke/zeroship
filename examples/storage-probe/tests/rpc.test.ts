import { expect, test } from "vitest";
import { rpc, type Row } from "./rpc";
import { targets } from "./targets";

async function exercise(origin: string): Promise<Row[]> {
  const rows: Row[] = [];
  const call = async (name: string) => {
    const row = await rpc(origin, `probe.${name}`);
    rows.push(row);
    return row;
  };
  expect(await call("reset")).toEqual({ remaining: 0, moreAfter: false });
  const text = await call("text");
  expect(text).toMatchObject({ found: true, contentType: "text/plain; charset=utf-8", textMatches: true });
  expect(text.putSize).toBeGreaterThan(0);
  expect(text.getSize).toBe(text.putSize);
  expect(await call("binary")).toMatchObject({ putSize: 256, getSize: 256, bytesIdentical: true, contentType: "application/octet-stream" });
  expect(await call("overwrite")).toMatchObject({ secondSize: 6, secondText: "second", secondType: "application/json", entriesUnderPrefix: 1 });
  expect(await call("deleteAbsent")).toMatchObject({ firstDeleted: true, secondDeleted: false, getAfterDeleteFound: false, entriesUnderPrefix: 0 });
  const types = await call("contentTypes");
  expect(types.rows).toEqual(expect.arrayContaining([
    expect.objectContaining({ given: "text/plain; charset=utf-8", got: "text/plain; charset=utf-8" }),
    expect.objectContaining({ given: "application/json", got: "application/json" }),
    expect.objectContaining({ given: "image/png", got: "image/png" }),
  ]));
  expect(await call("listPrefix")).toMatchObject({
    aKeys: ["sp/list/a/1.txt", "sp/list/a/2.txt", "sp/list/a/3.txt"],
    aSizes: [1, 2, 3], aCursorNull: true,
    baseKeys: ["sp/list/a/1.txt", "sp/list/a/2.txt", "sp/list/a/3.txt", "sp/list/ab.txt", "sp/list/b/1.txt"],
  });
  expect(await call("listPaginate")).toEqual({
    pages: [["sp/page/k0.txt", "sp/page/k1.txt"], ["sp/page/k2.txt", "sp/page/k3.txt"], ["sp/page/k4.txt"]],
    cursorNonNull: [true, true, false], total: 5, unique: 5, listAllMatches: true,
  });
  expect(await call("listOvershoot")).toEqual({ count: 5, cursorNull: true });
  const uploaded = await call("streamPut");
  expect(uploaded).toMatchObject({ size: 1024 * 1024, expectedSize: 1024 * 1024, chunksSent: 16 });
  expect(uploaded.checksum).toMatch(/^[0-9a-f]+$/);
  const downloaded = await call("streamGet");
  expect(downloaded).toMatchObject({ found: true, multiChunk: true, size: uploaded.size, declaredSize: uploaded.size, checksum: uploaded.checksum });
  expect(await call("streamGetAbsent")).toEqual({ found: false });
  expect(await call("streamThenBuffered")).toMatchObject({
    found: true, size: uploaded.size, checksum: uploaded.checksum,
    listedKeys: ["sp/stream/blob.bin"], listedSizes: [uploaded.size],
  });
  return rows;
}

test("LocalFs development and deployed S3 satisfy the storage contract and agree", async () => {
  const [local, s3] = targets();
  expect(local.name).toBe("local");
  expect(s3.name).toBe("s3");
  const localRows = await exercise(local.apiUrl);
  const deployedRows = await exercise(s3.apiUrl);
  expect(deployedRows).toEqual(localRows);
});
