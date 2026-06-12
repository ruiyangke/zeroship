# s3-js-compare — official-SDK multipart oracle (ISS-32)

A standalone Node uploader using the **official `@aws-sdk/lib-storage`** managed
multipart `Upload` (parallel parts, configurable part size + concurrency). It
**replicates `examples/storage-gallery` `putLarge`'s exact byte pattern + rolling
checksum**, so it is a faithful comparison against our `compio-s3` `env.storage`
multipart path on two axes:

- **Correctness oracle** — does the official SDK store a correct, full-size
  object on the *same* MinIO? If yes, a 0-byte/short object from our impl is
  *our* bug, not the environment.
- **Throughput oracle** — official parallel multipart wall-time (and at
  `QUEUE_SIZE=1`, an apples-to-apples sequential comparison) vs our sequential
  `put_stream`.

Not a workspace member — install + run standalone:

```bash
cd tests/s3-js-compare
npm install                      # @aws-sdk/client-s3 + lib-storage (gitignored node_modules)

# point at a running MinIO (the e2e harnesses spin one up; or run your own)
S3_ENDPOINT=http://127.0.0.1:9000 S3_BUCKET=zeroship-e2e-large \
S3_ACCESS=minioadmin S3_SECRET=minioadmin \
SIZE_BYTES=5368709120 PART_SIZE=8388608 QUEUE_SIZE=4 VERIFY=1 \
  node upload.mjs
```

Output is JSON: `elapsedSec`, `throughputMiBs`, `storedSize`/`sizeOk`,
`uploadChecksum`, and (with `VERIFY=1`) `downloadChecksum`/`contentOk`.

Compare `QUEUE_SIZE=1` (sequential, matches our `put_stream`) and `QUEUE_SIZE=4+`
(parallel, the headroom #1 — parallel parts — would unlock) against the
zeroship numbers from `tests/e2e_s3_large_stream.sh`.
