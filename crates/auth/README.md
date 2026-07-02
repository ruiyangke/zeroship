# zeroship-auth

The zeroship Identity Provider login surface, identity flows, and native OIDC OP.

Public host: `auth.zeroship.ai`.

See `docs/reference/auth.md` for the current architecture.

## Build & run (local dev)

```bash
# Start auth:
AUTH_DB_URL=postgres://zeroship@localhost/zeroship \
AUTH_BOOTSTRAP=1 \
AUTH_INSECURE_DEV=1 \
cargo run -p zeroship-auth
```

## Important files
- `src/main.rs` — binary entrypoint.
- `src/oidc/` — native OAuth/OIDC protocol endpoints, tokens, metadata, signing, and client flows.
- `src/store/` — `auth.*` schema CRUD.
- `src/identity/` — password / federation / magic-link (Phase 2 onward).
- `src/ui/` — server-rendered HTML (Phase 2 onward).
