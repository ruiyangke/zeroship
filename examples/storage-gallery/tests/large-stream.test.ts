import { readFile } from "node:fs/promises";
import { AwsClient } from "aws4fetch";
import { expect, inject, test } from "vitest";
import { workerRpc } from "./worker";

const U32_BOUNDARY = 2 ** 32;
const SIZE = U32_BOUNDARY + 1024 * 1024;
const TIMEOUT = 1_200_000;

async function residentBytes(pid: number): Promise<number> {
  const status = await readFile(`/proc/${pid}/status`, "utf8");
  const match = /^VmRSS:\s+(\d+)\s+kB$/m.exec(status);
  if (!match) throw new Error("Worker RSS was not measurable");
  return Number(match[1]) * 1024;
}

test("large streams preserve wide lengths and checksums with bounded worker memory", async () => {
  const storage = inject("storageS3");
  const key = "large/wide-length.bin";
  let peak = await residentBytes(storage.workerPid);
  let samples = 1;
  let samplingError: unknown;
  let pending = Promise.resolve();
  const sampler = setInterval(() => {
    pending = pending.then(async () => {
      peak = Math.max(peak, await residentBytes(storage.workerPid));
      samples++;
    }).catch((error) => { samplingError = error; });
  }, 100);
  try {
    console.info("Gallery: uploading the wide-length stream");
    const uploaded = await workerRpc("gallery.putLarge", { key, sizeBytes: SIZE, seed: 7, chunkBytes: 1024 * 1024 }, TIMEOUT);
    expect(uploaded.size).toBe(SIZE);
    expect(uploaded.checksum).toMatch(/^[0-9a-f]+$/);
    const aws = new AwsClient({ accessKeyId: "minioadmin", secretAccessKey: "minioadmin", service: "s3", region: "us-east-1", retries: 0 });
    const objectUrl = `${storage.endpoint}/${storage.bucket}/${storage.prefix}/${storage.appId}/gallery/${key}`;
    const object = await aws.fetch(objectUrl, { method: "HEAD", signal: AbortSignal.timeout(10_000) });
    expect(object.status).toBe(200);
    expect(Number(object.headers.get("content-length"))).toBe(SIZE);
    console.info("Gallery: stored length verified; streaming download for checksum comparison");
    const downloaded = await workerRpc("gallery.getLargeHash", { key }, TIMEOUT);
    expect(downloaded).toEqual({ key, found: true, size: SIZE, checksum: uploaded.checksum });
    expect(await workerRpc("gallery.delete", { key }, TIMEOUT)).toEqual({ key, deleted: true });
  } finally {
    clearInterval(sampler);
    await pending;
    console.info({ objectBytes: SIZE, peakWorkerBytes: peak, samples });
  }
  if (samplingError) throw samplingError;
  expect(samples).toBeGreaterThan(1);
  expect(peak).toBeLessThan(Math.min(SIZE / 4, 2 * 1024 ** 3));
}, TIMEOUT * 2);
