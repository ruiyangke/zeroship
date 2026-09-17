# Object storage

Store and retrieve files for your app. Reach it as `bucket(name)`, which returns a `Bucket`;
the package also exports the result and entry types (`Result`, `PutResult`, `GetResult`,
`GetStreamResult`, `ListPage`, `ListEntry`, `ListOptions`) so you can name them in your own
signatures.

```ts
import { bucket } from "@zeroship/storage";

const uploads = bucket("uploads");
```

Each operation resolves `{ data, error }` — a failure resolves with `error` set and `data` set
to `null`, rather than throwing. The one exception is `bucket()`: it throws a plain `Error`
when the name is empty. Check `error` before using `data`: it is a plain `Error` and carries no
code field, so the message text is the only discriminator the SDK gives you.
Buckets are per-app — a bucket name only ever addresses **your** app's objects, and objects
are private: there is no public URL for one.

You do not create a bucket: the first `put` under a name creates it, and there is no create- or
delete-bucket operation. Reading or listing a bucket nobody has written to is an empty result,
not an error.

`bucket(name)` is synchronous and hands you the handle — it does not return an envelope. A
bucket name is non-empty, contains no `/`, `\` or NUL, and is not exactly `.` or `..`.
`bucket()` throws a plain `Error` only if the name is empty; any other invalid name is refused
at the first operation, which resolves `{ data: null, error }`.

---

## 1. Store an object

```ts
const written = await uploads.put("notes/hello.txt", "hello", {
  contentType: "text/plain",
});
if (written.error) throw written.error;
written.data; // { bucket: "uploads", key: "notes/hello.txt", size: 5 }
```

`put` accepts a `string`, `Uint8Array`, `ArrayBuffer`, `Blob`, or a
`ReadableStream<Uint8Array>`.

Omit `contentType` and the object is recorded — and read back — as `application/octet-stream`.
An empty string counts as omitted, and a `Blob` body supplies its own type first. The SDK types
it `string | null`, but an object written through this SDK always reads back a value.

**Writing an existing key replaces it.** The previous bytes and content type are gone,
nothing is versioned, and no error is raised — `data.size` reports the object you just
wrote. There is no conditional or "fail if it exists" write, no locking and no conflict
error: the last write wins, and a reader never sees a half-written object.

**Keys.** Use `/` for structure. A key is refused — as an error in `{ error }`, not a thrown
exception — when it is empty, contains a backslash or a NUL, or has an empty, `.` or `..`
path segment. So a leading, trailing or doubled `/` is refused. Nothing else is: there are
no length, character-set or reserved-prefix rules, and any other string is a valid key.

## 2. Read it back

```ts
const read = await uploads.get("notes/hello.txt");
if (read.error) throw read.error;

if (read.data === null) {
  // the key does not exist — this is NOT an error
} else {
  read.data.bytes;       // Uint8Array
  read.data.contentType; // e.g. "text/plain"
  read.data.size;        // bytes
}
```

**A missing key is `{ data: null }`, not an error.** `getText(key)` decodes UTF-8 for you and
returns that same envelope: `{ data: string | null, error }`. Bytes that are not valid UTF-8
become the Unicode replacement character in the string rather than an error — use `get` when
you need the exact bytes.

## 3. Delete, and list a prefix

```ts
const removed = await uploads.delete("notes/hello.txt");
if (removed.error) throw removed.error;
removed.data.deleted; // false if the key was not there
```

```ts
let cursor: string | null = null;
do {
  const { data, error } = await uploads.list("notes/", { cursor: cursor ?? undefined });
  if (error) throw error;
  for (const entry of data.entries) {
    entry.key;        // string
    entry.size;       // bytes
    entry.modifiedAt; // Date
  }
  cursor = data.cursor; // non-null means more pages exist
} while (cursor !== null);
```

`list(prefix?, { cursor?, limit? })` takes a **literal key prefix, not a glob**, and returns
**ascending key order**. Both arguments are optional: `list()` walks the whole bucket from the
first key. Pass `limit` to change the page size — default 1000 entries, clamped to 10000, and
asking for more than remains is fine (the page simply reports `cursor: null`). The cursor is
opaque and not tied to the request that produced it: pass it back unchanged, and you may
persist it and resume the listing later.

**`list` is also the only way to a key's size or existence without downloading it** — there is
no `head`, `stat` or `exists`. Pass the full key as the prefix and match it in the entries;
keys that merely start with it match too. `getText` is capped like `get`, so text larger than
the buffered cap has to be read with `getStream`.

## 4. Large objects: stream both ways

`put` with a `Blob` or a `ReadableStream` never holds the whole object, and `getStream`
pulls at your own pace. `fetch`, `ReadableStream` and `Blob` are runtime globals, so a
response body is one such stream:

```ts
const source = await fetch("https://example.com/big.csv");
await uploads.put("imports/big.csv", source.body!, { contentType: "text/csv" });

const out = await uploads.getStream("imports/big.csv");
if (out.data) {
  out.data.size;        // bytes
  out.data.contentType; // e.g. "text/csv"
  const reader = out.data.body.getReader();
  for (;;) {
    const { done, value } = await reader.read(); // value: Uint8Array
    if (done) break;
  }
}
```

Use the streamed path for anything larger than the buffered cap. The buffered path holds the
whole object in memory, and an object past the cap is **refused**, never truncated. A refused
upload — or a source stream that fails mid-upload — resolves `{ data: null, error }` and
leaves no new object behind: a key that already held an object keeps it. A streamed `put`
resolves the same `{ bucket, key, size }` result as a buffered one.

`getStream` on a missing key is `{ data: null }`, exactly like `get`.

## 5. Limits

There is no API that reports these caps, and a deployment may be configured away from the
defaults below. Treat a refusal message — not a hard-coded number — as the signal: the
refusals that carry a number report the effective one.

| Cap | Default |
| --- | --- |
| One buffered read or write | **16 MiB** |
| One streamed upload, counted across the whole stream | **32 GiB** |
| Open downloads per app | **64** |

Because errors carry no code field, you branch on the message text with
`error.message.includes(...)`. The stable part of each refusal is:

| Refusal | Message contains |
| --- | --- |
| A buffered `put` past its cap | `storage: buffered object exceeds size limit` |
| A buffered `get` past its cap | `exceeds buffered-get cap` |
| A streamed upload past its cap | `storage: streamed object exceeds size limit` |
| A `getStream` past the open-download cap | `storage: too many live download streams` |
| A key that breaks the key rules | `storage: invalid key` |
| A bucket name that breaks the bucket rules | `storage: invalid bucket name` |

Any other failure carries only the backend's own message; treat it as opaque rather than
matching on it.

The download that exceeds the open-download cap is the one that fails: `getStream` resolves to
`{ data: null, error }` carrying

> `storage: too many live download streams (64 max per app); read one to the end, or cancel it`

Downloads already open are unaffected. Only `getStream` opens a download — a buffered `get`
completes inside the call and never counts against the 64. To release one early, read it to
the end or cancel it — `out.data.body.cancel()` before you call `getReader()`,
`reader.cancel()` after — and await the cancel before retrying. Only the request that opened a
download can release it, so a request that hits the cap because other requests hold the 64
cannot free them; the allowance returns as those requests release their downloads or finish.

These are **per-operation** caps: they do not limit how much your app stores in total.

## 6. What you can rely on

- **App scoping is not yours to set.** Your app can only address its own objects, under a
  bucket name of your choosing.
- **A download belongs to the request that opened it.** No other app can read or cancel it.
  When your app is finished, a download you did not drain is released for you.
- **The live-download quota is per app, not per request**, so concurrent requests share one
  allowance.