# Builder Sandbox `prj_` Derivation

## Context

`crates/sandbox` requires `POST /sandboxes` to receive typed ids:
`user_id=usr_<base62>` and `project_id=prj_<base62>`. The builder was
sending `user_id="builder"` and a sanitized plain project string, so every
create request failed the controller's boundary validation.

The controller deduplicates live sandboxes on `(user_id, project_id)`, so the
builder must derive the same `prj_` id for the same project across process
restarts.

## Decision

Use deterministic derivation from the existing project/thread id:

- If the source is already a typed-id, decode its embedded UUID and re-tag it
  into the `prj_` namespace.
- If the source is a hyphenated UUID, base62-encode that UUID and prefix it as
  `prj_`.
- If the source is an opaque thread id, hash it deterministically into an
  RFC-4122-shaped UUID value and prefix it as `prj_`.

The workspace passes the app id as the project source for chat, files, and
read paths. App-less dev/test chats fall back to the chat thread id, but still
use the same deterministic derivation.

## Rationale

This keeps sandbox identity stateless: no extra storage lookup is needed before
the first sandbox create, and the controller's `(user_id, project_id)` dedup
continues to work after builder restarts. It also preserves the natural app to
project relationship when the app id is already the platform's durable project
identifier.

The JS codec lives in `@zeroship/server/typed-id` so builder code does not
inline a second base62 implementation.

## Rejected Alternative

Persisting a `threadId -> prj_...` mapping in `@zeroship/kv` was rejected for
this fix. KV would add a bootstrapping write before sandbox creation, introduce
another failure mode in the critical chat path, and still require a typed-id
generator. Deterministic derivation is simpler and matches the controller's
dedup contract without durable mapping state.
