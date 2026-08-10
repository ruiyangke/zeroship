# Deployment Pipeline Security + Correctness Review — 2026-07-27

Scope: the `.zship` artifact from CLI pack to live gateway routing.
Files: `crates/bundle/src/{manifest,rule,unpack,blob,s3_blob,limits}.rs`,
`crates/cli/src/{main,auth,secrets}.rs`, `crates/control/src/{api,registry,authz_guard,deploy}.rs`,
`crates/gateway/src/sync.rs`.

Context honored: pre-launch, no back-compat. No findings about migrations, deprecation shims,
or old-format support. Zero-tokio is deliberate and not flagged.

## Executive summary

The untrusted-artifact handling is, on the whole, **well built**. Zip-slip is structurally
impossible (the unpacker never writes tar entry paths to disk — it strips the `blobs/` prefix,
validates the remainder as a 64-char lowercase sha256, and uses *that* as the storage key).
Content-addressed integrity is verified end-to-end (write path, read path, and dedup path all
re-hash). Decompressed size is capped with a streaming `Read::take`, per-blob and per-deploy
counts are capped, and deploy authz runs before a single body byte is consumed. IDOR on deploy
is gated by `Action::AppsDeploy` on `Resource::App { id }` through `authz::enforce` (membership).

No CRITICAL issues were found in the reviewed code paths. The findings below are correctness
hardening and manifest-validation gaps (cross-tenant impact is limited because a manifest only
governs its own app's routing — it carries no host field, so route hijack of another app is not
reachable from a malicious manifest).

---

## HIGH

### H1 — CORS `allow_origins: ["*"]` + `allow_credentials: true` is accepted by `validate()`
`crates/bundle/src/rule.rs:26-44` (`Cors`), `crates/bundle/src/manifest.rs:458-548` (`validate()`).

`Cors` allows the literal `"*"` origin and a separate `allow_credentials: bool`, and
`Manifest::validate()` performs **no** cross-field check. A deployed manifest can therefore
declare `allow_origins: ["*"]` together with `allow_credentials: true`. If the gateway honors
that combination it reflects credentialed cross-origin access from any origin — the canonical
CORS credential-leak misconfiguration (a malicious page can read the authenticated victim's
responses). The comment on `Cors` even says `"*"` matches any origin "subject to credentials
rules", but those rules are not enforced at validation time.

Attack path: creator (or an AI that generated a plausible-looking config) ships a manifest with
`{ allow_origins: ["*"], allow_credentials: true }` on an `auth: user` resource → any origin can
issue credentialed reads against that app's end users.

Fix direction: in `validate()`, reject `allow_credentials == true` when `allow_origins` contains
`"*"` (mirror the Fetch spec: wildcard origin is incompatible with credentials). Verify the
gateway's CORS emitter also refuses to echo `*` with credentials as defense-in-depth.

---

## MEDIUM

### M1 — `RedirectAction.to` / `rewrite` / `redirect` targets are unvalidated (open redirect)
`crates/bundle/src/rule.rs:189-196` (`RedirectAction`), `:263-270` (`Action::Redirect`/`Rewrite`),
`crates/bundle/src/manifest.rs:552-583` (only the *status* is validated, not `to`).

`validate()` checks that `redirect.status` is 3xx but never inspects the destination. A manifest
can set `to: "https://evil.example/phish"` on a resource. This is an app-scoped open redirect:
the app author controls their own routing, so it is not cross-tenant, but it turns every deployed
app into an open-redirector that phishers can abuse under the platform's own subdomain
(`{app}.zeroship.ai/...` → attacker site), damaging platform reputation and defeating
same-origin trust users place in the subdomain. `rewrite.to` restarts rule walking (rule.rs:268)
with only a "hop limit" mentioned in a comment — confirm that limit actually exists in the gateway
compiler, or a `rewrite` cycle is a per-request DoS.

Fix direction: constrain `redirect.to`/`rewrite.to` to same-origin/relative paths (or an explicit
allowlist of external hosts), and verify the rewrite hop-limit is enforced in the compiled path.

### M2 — Passthrough fallback on manifest-validation failure can silently downgrade a deploy to public
`crates/gateway/src/sync.rs:90-100` and `crates/control/src/registry.rs:644-659`.

When a stored manifest fails `validate()` at route-sync time, both the gateway (`sync.rs:97`) and
the control registry (`registry.rs:659`) substitute `Manifest::passthrough()`. `passthrough()`
(`manifest.rs:403-437`) installs a single `*` resource with `auth: Anon, publicly_accessible:
true` — i.e. **everything becomes public and hits the worker as SSR**. The deploy handler
validates on ingest, so this should not normally fire; but any post-ingest divergence (a schema
change that makes a previously-valid stored manifest invalid, a partial column write, an
out-of-band DB edit) flips a formerly auth-gated app to fully public instead of failing closed.
For a routing/authz surface, fail-open is the wrong default.

Fix direction: on validation failure, fail closed — serve 503/blackhole the route rather than an
anon-public passthrough. At minimum log at `error` (not `warn`) and emit a metric so a downgraded
app is loud.

### M3 — Deploy pre-check TOCTOU: blobs are written before the app-existence commit
`crates/control/src/api.rs:497` (ingest → writes blobs + manifest to the blob store) then
`:594-607` (`set_deploy_with_manifest` returns `Ok(false)` = "app not found").

Authz runs first (good), but `ingest()` streams every blob and the manifest object into the blob
store *before* the final `UPDATE ... WHERE id = $3` discovers the app row does not exist
(`Ok(false)` → 404). A caller with the `AppsDeploy` grant but targeting a since-deleted app id
writes arbitrary content-addressed blobs (up to `MAX_BLOBS_PER_DEPLOY` × `MAX_BLOB_BYTES` = 10k ×
16 MiB) plus a `manifests/<app_id>/…` object that is now orphaned (no app row, so `purge_app`'s
`delete_app_manifests` will never be called for it). This is a storage-exhaustion / orphan-litter
vector, not a takeover.

Fix direction: check app existence (a cheap `SELECT 1`) *after* authz and *before* ingest, or make
the manifest write conditional on the row update and GC orphaned `manifests/<app_id>/` prefixes for
non-existent apps.

### M4 — `is_uuid` (CLI) accepts malformed UUIDs that the server must re-reject
`crates/cli/src/main.rs:579-586`.

`is_uuid` only checks length 36 and that positions 8/13/18/23 are `-` and the rest are hex — it
does not validate the UUID version/variant and, more importantly, treats any 36-char hex-with-dashes
string as an id, sending it straight to `POST /apps/{app}/deploy`. This is a client-side ergonomics
bug, not a server vuln (the server parses with `Uuid::parse` at `api.rs:389` and 400s), but it means
`--app=<garbage-that-looks-uuid-ish>` skips the name-based create/resolve path and produces a
confusing 400/404 instead of resolving-or-creating by name. Low blast radius; noting for correctness.

Fix direction: use the same `Uuid` parse the server uses, or drop the hand-rolled check.

---

## LOW

### L1 — `has_legacy_deploy_migration_query` substring parse is brittle
`crates/control/src/api.rs:622-627`. Splits the raw query string on `&`/`=` by hand. Correct for
the two sentinel keys today, but a percent-encoded key (`approved%5Fversions`) slips past. Only
matters as a guard that returns a helpful 400; not security-relevant. Consider parsing with the
same querystring parser used elsewhere.

### L2 — `net.requests` wildcard host allows `*.` with an empty label suffix
`crates/bundle/src/manifest.rs:257-278`. `NetRequest::validate` rejects bare `*` and requires the
`*.` prefix for any `*`, but `"*."` (prefix only, empty domain) passes: it is not `"*"`, has one
`*`, and starts with `"*."`. These entries are inert hints (they never become grants without an
operator writing `app_net_grants`), so impact is nil today, but a `"*."` hint is meaningless and
should be rejected at the source. Fix: require at least one non-empty label after `*.`.

### L3 — Secret/var CLI interpolates `KEY` directly into the URL path
`crates/cli/src/secrets.rs:109` (`{control_url}/api/apps/{app}/{resource}/{key}`). `key` comes from
`args.get(3)` unencoded, so a key with `/`, `?`, or `#` produces a wrong request path. Client-side
only (the server owns real validation), but a key like `a/b` deletes the wrong resource path
silently. Fix: percent-encode the path segment (the auth module already pulls in `url`).

### L4 — Manifest `schemas` values are unbounded arbitrary JSON with no size ceiling beyond the 1 MB manifest cap
`crates/bundle/src/manifest.rs:64-65`. `schemas: HashMap<String, serde_json::Value>` is parsed and
stored verbatim; only the overall 1 MB `MAX_MANIFEST_BYTES` bounds it. Deeply nested JSON within
1 MB can still be a parser/validator CPU sink when the gateway compiles per-resource policy. Low
because 1 MB caps the absolute size; note it if schema validation ever walks these recursively.

---

## Things checked and found SOUND (no action)

- **Zip-slip / path traversal (`unpack.rs:211-241`)**: entry paths are never written to disk. The
  first entry must be exactly `manifest.json`; every other entry must be `blobs/<hash>` where
  `<hash>` passes `validate_hash_format` (64 lowercase hex). The storage key is derived from the
  validated hash, not the tar path. `..`, absolute paths, and symlinks cannot escape because
  nothing is unpacked by path. Non-file entry types are skipped (`:216`).
- **Decompression bomb (`unpack.rs:99`)**: `Read::take(decoder, MAX_DECOMPRESSED_BYTES + 1)` bounds
  decompressed flow; per-blob (`MAX_BLOB_BYTES`), per-deploy count (`MAX_BLOBS_PER_DEPLOY`), and
  manifest (`MAX_MANIFEST_BYTES`) caps are all enforced during streaming, and peak memory is
  O(64 KiB) chunks. Compressed body is capped at `MAX_COMPRESSED_BYTES` while streaming to tmp
  (`api.rs:435`, `stream_body_to_tmp_file` at `api.rs:2092`).
- **Content-addressed integrity**: verified on write (`blob.rs:317-328`, `s3_blob.rs:256-262`), on
  read (`blob.rs:224-231`, `s3_blob.rs:407-413`), and — notably — on the dedup path
  (`blob.rs:259-265` re-hashes the local file via `verify_local_blob` before trusting a dedup hit;
  S3 relies on the original writer's verified-before-complete guarantee). Backend checksums are
  explicitly not trusted; the store always re-hashes.
- **Deploy authz / IDOR**: `authz.require(Action::AppsDeploy, Resource::App { id }, …)` runs before
  any body is consumed (`api.rs:396-401`). `Resource::validate_ids()` guards the id. (The
  `authz::enforce` membership logic itself was out of scope and not audited here.)
- **Route hijack via manifest**: not reachable. The routing key / subdomain is the app `name`, set
  at `create_app` (`registry.rs:191-206`, charset-restricted, DB-UNIQUE) and surfaced by
  `get_routes` from `apps.name` (`registry.rs:663`). The manifest carries no host field, so a
  malicious manifest cannot match another app's host.
- **Atomic deploy commit**: `set_deploy_with_manifest` sets `deploy_hash` + `manifest_json` in one
  UPDATE (`registry.rs:393-408`); the gateway pulls both together (`sync.rs:161-170`). Blobs +
  manifest object are written before the DB commit, so a published route never precedes its blobs.
- **S3 key injection**: keys are `blobs/<hash>` and `manifests/<uuid>/<deploy_hash>.json` built only
  from a validated sha256 hash and a real `Uuid`/deploy-hash — no user-controlled string reaches an
  S3 key.
- **CLI credential storage**: `write_private_file` opens `0o600` and re-chmods (`auth.rs:419-434`).
- **Error sanitization**: infrastructure errors are collapsed to `{"error":"internal error"}`
  before leaving the control plane (tests at `api.rs:2173-2198`); internal paths/DSNs are not leaked.
</content>
</invoke>
