// Official-SDK S3 multipart oracle for ISS-32.
//
// Uploads a `sizeBytes` object to an S3/MinIO endpoint via @aws-sdk/lib-storage
// `Upload` (the production-grade managed multipart uploader: configurable part
// size + parallel `queueSize`), then HeadObject-verifies the stored size and
// (optionally) GET-rehashes to compare the content checksum. The generated
// bytes + rolling checksum REPLICATE zeroship's `examples/storage-gallery`
// `putLarge`, so this is a faithful correctness AND throughput comparison:
//
//   - Correctness oracle: does the official SDK store a correct, full-size
//     object on THIS MinIO? (If yes, a 0-byte/short object from our impl is OUR
//     bug, not the environment.)
//   - Throughput oracle: official parallel multipart wall-time vs our
//     compio-s3 sequential `put_stream`.
//
// Config via env (all optional except the endpoint defaults are MinIO-dev):
//   S3_ENDPOINT (http://127.0.0.1:9000)  S3_BUCKET  S3_KEY
//   S3_ACCESS (minioadmin)  S3_SECRET (minioadmin)  S3_REGION (us-east-1)
//   SIZE_BYTES (5368709120)  PART_SIZE (8388608)  QUEUE_SIZE (4)
//   SEED (7)  CHUNK_BYTES (1048576)  VERIFY (0|1)
//
//   node upload.mjs

import { Readable } from "node:stream";
import { S3Client, HeadObjectCommand, GetObjectCommand } from "@aws-sdk/client-s3";
import { Upload } from "@aws-sdk/lib-storage";

const env = (k, d) => process.env[k] ?? d;
const num = (k, d) => Number(env(k, d));

const ENDPOINT = env("S3_ENDPOINT", "http://127.0.0.1:9000");
const BUCKET = env("S3_BUCKET", "zeroship-e2e-large");
const KEY = env("S3_KEY", `js-oracle/stream-${num("SEED", 7)}.bin`);
const ACCESS = env("S3_ACCESS", "minioadmin");
const SECRET = env("S3_SECRET", "minioadmin");
const REGION = env("S3_REGION", "us-east-1");
const SIZE_BYTES = num("SIZE_BYTES", 5368709120); // 5 GiB, > u32::MAX
const PART_SIZE = num("PART_SIZE", 8 * 1024 * 1024); // match compio-s3 PART_SIZE
const QUEUE_SIZE = num("QUEUE_SIZE", 4); // parallel parts in flight
const SEED = num("SEED", 7);
const CHUNK_BYTES = num("CHUNK_BYTES", 1024 * 1024);
const VERIFY = env("VERIFY", "0") === "1";

// ── gallery pattern + checksum, replicated byte-for-byte ───────────────────
const PATTERN_PERIOD = 4099;
function buildPattern(seed) {
  const p = new Uint8Array(PATTERN_PERIOD);
  for (let i = 0; i < PATTERN_PERIOD; i++) p[i] = (seed * 31 + i) & 0xff;
  return p;
}
function fillFromPattern(out, pattern, start) {
  let written = 0;
  while (written < out.length) {
    const phase = (start + written) % PATTERN_PERIOD;
    const slice = pattern.subarray(phase, Math.min(PATTERN_PERIOD, phase + (out.length - written)));
    out.set(slice, written);
    written += slice.length;
  }
}
function updateChecksum(acc, chunk, absStart) {
  let { a, b } = acc;
  for (let i = 0; i < chunk.length; i++) {
    a = (a + chunk[i] * (((absStart + i) % 65521) + 1)) % 0xfffffffb;
    b = (b + a) % 0xfffffffb;
  }
  acc.a = a;
  acc.b = b;
}
function checksumHex(acc) {
  return (acc.a >>> 0).toString(16).padStart(8, "0") + (acc.b >>> 0).toString(16).padStart(8, "0");
}

// Bounded-memory generator: a Node Readable that produces the pattern in
// CHUNK_BYTES slices until SIZE_BYTES, computing the upload checksum as it goes.
function makeBody(sizeBytes, seed, chunkBytes, acc) {
  const pattern = buildPattern(seed);
  let offset = 0;
  return new Readable({
    read() {
      if (offset >= sizeBytes) {
        this.push(null);
        return;
      }
      const len = Math.min(chunkBytes, sizeBytes - offset);
      const chunk = new Uint8Array(len);
      fillFromPattern(chunk, pattern, offset);
      updateChecksum(acc, chunk, offset);
      offset += len;
      this.push(Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength));
    },
  });
}

function mkClient() {
  return new S3Client({
    endpoint: ENDPOINT,
    region: REGION,
    forcePathStyle: true, // MinIO / path-style, like our dev config
    credentials: { accessKeyId: ACCESS, secretAccessKey: SECRET },
  });
}

async function main() {
  const client = mkClient();
  const upAcc = { a: 1, b: 0 };
  const body = makeBody(SIZE_BYTES, SEED, CHUNK_BYTES, upAcc);

  const t0 = process.hrtime.bigint();
  const up = new Upload({
    client,
    params: { Bucket: BUCKET, Key: KEY, Body: body, ContentType: "application/octet-stream" },
    partSize: PART_SIZE,
    queueSize: QUEUE_SIZE,
    leavePartsOnError: false, // abort on error — no orphaned parts
  });
  await up.done();
  const elapsedSec = Number(process.hrtime.bigint() - t0) / 1e9;
  const uploadChecksum = checksumHex(upAcc);

  // HeadObject — the stored size must equal what we streamed (the exact thing
  // our impl got wrong: 200 returned but 0 bytes stored).
  const head = await client.send(new HeadObjectCommand({ Bucket: BUCKET, Key: KEY }));
  const storedSize = Number(head.ContentLength);
  const sizeOk = storedSize === SIZE_BYTES;

  let downloadChecksum = null;
  let contentOk = null;
  if (VERIFY) {
    const get = await client.send(new GetObjectCommand({ Bucket: BUCKET, Key: KEY }));
    const dAcc = { a: 1, b: 0 };
    let dOff = 0;
    for await (const chunk of get.Body) {
      const u8 = chunk instanceof Uint8Array ? chunk : new Uint8Array(chunk);
      updateChecksum(dAcc, u8, dOff);
      dOff += u8.length;
    }
    downloadChecksum = checksumHex(dAcc);
    contentOk = downloadChecksum === uploadChecksum && dOff === SIZE_BYTES;
  }

  const result = {
    impl: "official @aws-sdk/lib-storage",
    endpoint: ENDPOINT,
    bucket: BUCKET,
    key: KEY,
    sizeBytes: SIZE_BYTES,
    partSize: PART_SIZE,
    queueSize: QUEUE_SIZE,
    elapsedSec: Number(elapsedSec.toFixed(3)),
    throughputMiBs: Number((SIZE_BYTES / (1024 * 1024) / elapsedSec).toFixed(1)),
    uploadChecksum,
    storedSize,
    sizeOk,
    downloadChecksum,
    contentOk,
  };
  console.log(JSON.stringify(result, null, 2));
  if (!sizeOk || (VERIFY && !contentOk)) process.exit(1);
}

main().catch((e) => {
  console.error("UPLOAD FAILED:", e?.message ?? e);
  process.exit(2);
});
