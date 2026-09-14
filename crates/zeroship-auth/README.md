# zeroship-auth

The zeroship Identity Provider login surface, identity flows, and native OIDC OP.

Public host: `auth.zeroship.ai`.

See `docs/reference/auth.md` for the current architecture.

Native ORM schemas and domain conversions live in `src/store/native.rs`;
`src/store/users.rs` defines the typed user inputs and read models. The host
opens the ORM with its provisioned auth role. Platform migrations own physical
table creation; native Rust mappings need no runtime descriptor file.

## Build & run (local dev)

```bash
# Start auth:
AUTH_DB_URL=postgres://zeroship@localhost/zeroship \
AUTH_BOOTSTRAP=1 \
AUTH_STASH_SIGNING_KEY="$(openssl rand -hex 32)" \
AUTH_TOTP_ENC_KEY="$(openssl rand -hex 32)" \
cargo run -p zeroship-auth
```

Browser flows always use Secure `__Host-*` cookies. The compose topology uses
`*.zeroship.localhost`, which browsers treat as potentially trustworthy; local
runs use the same cookie shape and have no plaintext-cookie mode.

## Important files

- `src/main.rs` — binary entrypoint.
- `src/oidc/` — native OAuth/OIDC protocol endpoints, tokens, metadata, signing, and client flows.
- `src/store/` — persistence in the `zeroship` schema.
- `src/identity/` — password, federation, and magic-link flows.
- `src/ui/` — server-rendered HTML.
