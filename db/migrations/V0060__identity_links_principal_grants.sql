-- Platform identity bridge for provider-native control/deploy bearers.
--
-- GoTrue `sub` values are provider-local identities, not zeroship principals.
-- The read-side control authz path resolves pre-linked subjects through
-- `identity_links`, then derives deploy/read policy from `principal_grants`.
-- P-S2 owns the device-flow approval write path that creates these rows.

CREATE TABLE zeroship.identity_links (
    principal_id     UUID        NOT NULL REFERENCES zeroship.users(id),
    provider         TEXT        NOT NULL,
    provider_subject TEXT        NOT NULL,
    email            TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (provider, provider_subject)
);

CREATE INDEX identity_links_principal_id_idx
    ON zeroship.identity_links (principal_id);

CREATE TABLE zeroship.principal_grants (
    principal_id UUID NOT NULL REFERENCES zeroship.users(id),
    grant_name   TEXT NOT NULL,
    PRIMARY KEY (principal_id, grant_name)
);

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.identity_links TO zeroship_control';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON zeroship.principal_grants TO zeroship_control';
  END IF;
END $g$;
