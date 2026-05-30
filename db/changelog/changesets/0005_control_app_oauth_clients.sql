--liquibase formatted sql

-- control.app_oauth_clients — per-app extension of control.oauth_clients
-- (auth-sdk Slice 1d, spec §1.1 / §8.1).
--
-- The OAuth client identity itself — client_id (the control.oauth_grants FK
-- target), redirect_uris, scopes, skip_consent, hydra_client_id — lives in the
-- EXISTING control.oauth_clients (ensure_app_client writes it there, exactly
-- like bootstrap_builder.rs does for the builder client). This table holds only
-- the per-app-specific bits oauth_clients lacks: the app_id ↔ client_id link
-- and the sector_identifier (apex origin) for pairwise/relay scoping.
--
-- FK order: control.apps and control.oauth_clients both already exist
-- (0004_control.sql) so this 0005 changeset runs cleanly after them.

--changeset zeroship:control-app-oauth-clients splitStatements:true
CREATE TABLE control.app_oauth_clients (
    app_id              UUID PRIMARY KEY REFERENCES control.apps(id) ON DELETE CASCADE,
    client_id           TEXT NOT NULL UNIQUE
                          REFERENCES control.oauth_clients(client_id) ON DELETE CASCADE,
    sector_identifier   TEXT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- redirect_uris, scopes, skip_consent, hydra_client_id are NOT here — they live
-- on control.oauth_clients (the control.oauth_grants FK target + the
-- skip_consent the consent fast path reads). ensure_app_client (§1.1) writes
-- BOTH the oauth_clients row (skip_consent=FALSE) and this extension row.
--rollback DROP TABLE control.app_oauth_clients;
