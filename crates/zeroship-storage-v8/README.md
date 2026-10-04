# zeroship-storage-v8

Binds `zeroship-storage` to `env.storage` through `StorageBinding::new(store,
meter)`. The host selects the store; the binding captures an app-scoped Rust
handle when the runtime initializes the namespace.

This crate owns JavaScript argument conversion, promises, upload backpressure,
download handles and platform usage metering. Rust storage consumers depend on
`zeroship-storage` directly.

Download sources belong to their isolate. Stream handles cannot cross apps or
deploys, and isolate teardown releases undrained downloads. Quota counters are
shared across isolates of the same app on a worker thread, so creating another
isolate cannot multiply its download budget.

A download gathers the backend's frames into `readChunk` results of at most
`limits::DOWNLOAD_CHUNK_BYTES`, so the V8 round trip is paid per chunk rather
than per network frame. A gather also ends at the first frame to arrive once
`limits::DOWNLOAD_GATHER_BUDGET` has passed since its first byte, so a backend
that trickles frames hands over partial chunks. Frames are pulled only while a
`readChunk` waits, each under the backend's body stall bound. A cancel stops
the pull at the next frame, and a read in flight when its download is
cancelled resolves EOF however its gather ended. The chunk reaches JavaScript
as the backing store of its `Uint8Array`, without a second copy.

`cargo bench -p zeroship-storage-v8 --bench stream_read` reports the
throughput of one S3 download read by the backend alone, by raw `readChunk`
calls and through the SDK's `Bucket.getStream`.

`cargo test -p zeroship-storage-v8` exercises real V8 streaming, metering,
namespace isolation, backend binding and resource reclamation.
