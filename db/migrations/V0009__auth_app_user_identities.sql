-- zeroship.app_user_identities — the per-app pairwise + relay identity mapping
-- (auth-sdk Slice 4, spec §6.2/§6.3/§8.1). The gateway derives the per-app
-- pairwise subject `pws_… = derive_pairwise(pairwise_salt, global_user_id,
-- route.sector_identifier)` at the ZeroShip-User header boundary (F4-B) and
-- UPSERTS this row whenever it projects a pws_ for an (app, global_user). The
-- row is the ONLY place the pws_ ↔ (app, global_user) mapping is persisted, so
-- support tooling, the relay handler, and revocation can reverse
-- pws_ → (app, global_user).
--
-- Column pinning (closes the main-spec/relay-sub-spec ambiguity):
--   - app_client_id holds the per-app OAuth client_id (oac_<base62-app-id>,
--     client_id_for_app(uuid)), NOT the app uuid-as-text. The gateway has
--     route.oauth_client_id in hand when it upserts; control's explicit-revoke
--     keys on the {client_id} path param; app-delete reconstructs the SAME
--     oac_ value from the uuid — so all three sites key on ONE deterministic
--     value with no cross-schema join. The name `app_client_id` (not `app_id`)
--     makes that content explicit at the column level.
--   - pairwise_sub is the derived pws_… value (a deterministic projection of
--     (global_user_id, sector); re-login / re-grant re-derives the SAME value).
--     It is a column rather than the PK because the natural key is
--     (app_client_id, global_user_id): re-grant UPSERTS that one row (clears
--     revoked_at), and the pairwise_sub index serves the relay reverse-lookup.
--   - global_user_id is UUID (FK to zeroship.users.id), the GLOBAL Hydra subject.
--     The pws_ is NEVER stored next to the UUID in the cookie/anchor tables;
--     it lives here as the persisted projection.
--   - relay_email stays NULL until Slice 5 populates it on first email-scope
--     consent. The partial-unique index (active aliases only) frees a revoked
--     alias's slot for a recycled token; created here so Slice 5 needs no DDL.

CREATE TABLE zeroship.app_user_identities (
    -- app_client_id FKs into zeroship.oauth_clients(client_id) ON DELETE CASCADE
    -- (oauth_clients exists by 0009): deleting the per-app oauth_clients row
    -- (app-delete does this in the same txn as the apps delete) atomically drops
    -- this app's pairwise/relay identity rows — closing the orphaned-live-alias
    -- gap the old best-effort companion UPDATE used to cover.
    app_client_id   TEXT        NOT NULL REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE,  -- per-app OAuth client_id (oac_<base62>); see header
    global_user_id  UUID        NOT NULL REFERENCES zeroship.users(id) ON DELETE CASCADE,  -- GLOBAL Hydra subject
    pairwise_sub    TEXT        NOT NULL,            -- pws_… == derive_pairwise(salt, global_user_id, sector); DETERMINISTIC
    relay_email     TEXT,                            -- {token}@{relay_domain}; NULL until email scope granted (Slice 5)
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked_at      TIMESTAMPTZ,
    PRIMARY KEY (app_client_id, global_user_id)      -- one row per (app, global_user); re-grant UPSERTS it
);
-- Relay reverse-lookup (Slice 5: pws_ / alias → (app, global_user)).
CREATE INDEX app_user_identities_pairwise_sub_idx
    ON zeroship.app_user_identities (pairwise_sub);
-- Relay alias unique only among ACTIVE aliases → a revoked alias frees its slot
-- for a recycled token (Slice 5 generate-and-retry-on-conflict).
CREATE UNIQUE INDEX app_user_identities_relay_active_idx
    ON zeroship.app_user_identities (relay_email) WHERE relay_email IS NOT NULL AND revoked_at IS NULL;
