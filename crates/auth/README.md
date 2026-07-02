# zeroship-auth

The zeroship Identity Provider login surface + identity flows + native OP.

Public host: `auth.zeroship.ai`.

See `docs/archive/auth-server.md` for the design.

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
- `src/bootstrap/` — first-boot JWK + client reconciliation.
- `src/store/` — `auth.*` schema migrations and CRUD.
- `src/identity/` — password / federation / magic-link (Phase 2 onward).
- `src/ui/` — server-rendered HTML (Phase 2 onward).
