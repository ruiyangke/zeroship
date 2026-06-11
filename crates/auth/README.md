# zeroship-auth

The zeroship Identity Provider login surface + identity flows + hydra admin client.

Companion process: `oryd/hydra` (OIDC kernel). Public host: `auth.zeroship.ai`.

See `docs/archive/auth-server.md` for the design.

## Build & run (local dev)

```bash
# 1. Start hydra (docker-compose up hydra)
# 2. Start auth:
AUTH_DB_URL=postgres://zeroship@localhost/zeroship \
AUTH_BOOTSTRAP=1 \
AUTH_INSECURE_DEV=1 \
cargo run -p zeroship-auth
```

## Important files
- `src/main.rs` — binary entrypoint.
- `src/hydra_client/` — hand-rolled admin-API client.
- `src/bootstrap/` — first-boot JWK + client reconciliation.
- `src/store/` — `auth.*` schema migrations and CRUD.
- `src/identity/` — password / federation / magic-link (Phase 2 onward).
- `src/ui/` — server-rendered HTML (Phase 2 onward).
