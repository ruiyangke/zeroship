--liquibase formatted sql

-- auth.app_session_anchors — the DEDICATED SDK reload-recovery anchor store
-- (auth-sdk Slice 1b-anchors, spec §8.1). A SEPARATE credential from the
-- 12h/30-min interactive auth.gateway_sessions:
--   - NO idle column: a reload-recovery anchor MUST survive long idle gaps.
--   - abs_expires_at = created_at + 30d, SET ONCE at create, NEVER slid. The
--     720h Hydra family ceiling is enforced ONLY by Hydra invalid_grant on a
--     ?mint=1 refresh (gateway deletes the anchor + clears the breadcrumb),
--     NOT mirrored into abs_expires_at.
--   - refresh_token_enc holds the encrypted (AES-256-GCM, core::crypto) server-
--     held rotating refresh family — the browser never holds a refresh token in
--     the default server_anchor mode.
--   - cached_access_token holds the last minted WRAPPER access token (sub=pws_,
--     the per-app pairwise subject) shared by concurrent ?mint=1; cached_access_exp is the
--     SECONDS-scale single-flight window (min(600, ANCHOR_MINT_CACHE_TTL)) ≪ the
--     10-min wrapper exp the browser holds.
--
-- NOTE: auth.token_revocations already exists (changeset
-- zeroship:auth-token-revocations in 0002_auth.sql) — NOT recreated here.

--changeset zeroship:auth-app-session-anchors splitStatements:true
CREATE TABLE auth.app_session_anchors (
    id                  UUID        PRIMARY KEY DEFAULT gen_random_uuid(),  -- the __Host-zs_app_session cookie value
    app_id              TEXT        NOT NULL,            -- TEXT, consistent with gateway_sessions/app_user_identities
    client_id           TEXT        NOT NULL,            -- the per-app OAuth client_id (oac_<base62>) bound at mint time
    global_user_id      UUID        NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,  -- the GLOBAL user (pws_ is a projection, never stored)
    refresh_token_enc   BYTEA       NOT NULL,            -- AES-256-GCM encrypted server-held rotating refresh family
    refresh_family_id   TEXT        NOT NULL,            -- gateway-generated lineage id (rfam_<base62>), set ONCE at create and carried verbatim across every rotation; Hydra exposes no usable family-lineage field (§1.2)
    cached_access_token TEXT,                            -- last minted WRAPPER access token, shared by concurrent ?mint=1 (§1.2)
    cached_access_exp   TIMESTAMPTZ,                     -- server-side cache window = min(600, ANCHOR_MINT_CACHE_TTL), SECONDS-scale ≪ the 10-min wrapper exp
    granted_scopes      TEXT[]      NOT NULL DEFAULT '{}',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- NO idle column: a reload-recovery anchor MUST survive long idle gaps.
    abs_expires_at      TIMESTAMPTZ NOT NULL,            -- = created_at + 30d, SET ONCE at create, NEVER slid.
    revoked_at          TIMESTAMPTZ
);
CREATE INDEX app_session_anchors_user_idx
    ON auth.app_session_anchors (app_id, global_user_id) WHERE revoked_at IS NULL;
--rollback DROP TABLE auth.app_session_anchors;
