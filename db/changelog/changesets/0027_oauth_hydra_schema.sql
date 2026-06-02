--liquibase formatted sql

-- Provision a DEDICATED namespace + least-privileged login role for Ory Hydra.
-- Hydra's tables (hydra_client, hydra_oauth2_*, hydra_jwk, networks,
-- schema_migration, …) live in their OWN `oauth_hydra` schema instead of being
-- mixed into `zeroship` / `public` (the `postgres` role default search_path is
-- `zeroship, public`, so a Hydra that connected as `postgres` would scatter its
-- tables into the platform schema — and races the platform `migrate`).
--
-- DIVISION OF OWNERSHIP: Liquibase provisions ONLY the empty schema + role +
-- search_path + grants here — version-stable infrastructure it owns. Hydra
-- still CREATES AND OWNS its TABLES via `hydra migrate sql` (the hydra-migrate
-- compose service), connecting AS this `oauth_hydra` role. Do NOT hand-author
-- Hydra's table DDL here — it is version-locked to the Hydra binary.
--
-- The DEV password literal below matches the dev `POSTGRES_PASSWORD`; in
-- production inject it via Liquibase property substitution / a secret backend,
-- never a literal.

--changeset zeroship:0027-oauth-hydra-role splitStatements:false
DO $$
BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'oauth_hydra') THEN
    CREATE ROLE oauth_hydra LOGIN PASSWORD 'zeroship';
  END IF;
END
$$;
--rollback DROP ROLE IF EXISTS oauth_hydra;

--changeset zeroship:0027-oauth-hydra-schema splitStatements:true
CREATE SCHEMA IF NOT EXISTS oauth_hydra AUTHORIZATION oauth_hydra;
GRANT CONNECT ON DATABASE zeroship TO oauth_hydra;
GRANT USAGE ON SCHEMA public TO oauth_hydra;
-- Hydra connects as this role; unqualified DDL/DML resolves to oauth_hydra
-- first (its own schema), then public for any shared extensions.
ALTER ROLE oauth_hydra SET search_path = oauth_hydra, public;
--rollback ALTER ROLE oauth_hydra RESET search_path;
--rollback DROP SCHEMA IF EXISTS oauth_hydra CASCADE;

--changeset zeroship:0027-hydra-uuid-ossp splitStatements:true
-- Hydra's first migration (networks) runs `CREATE EXTENSION "uuid-ossp"`, which
-- requires privileges the least-privileged oauth_hydra role lacks. Pre-create it
-- here as the superuser (into oauth_hydra, so it stays self-contained and
-- resolves via oauth_hydra's search_path) — Hydra's `CREATE EXTENSION IF NOT
-- EXISTS` then short-circuits without needing the privilege. (Changeset 0001
-- keeps the zeroship schema on gen_random_uuid(); this extension exists SOLELY
-- for Hydra.)
CREATE EXTENSION IF NOT EXISTS "uuid-ossp" WITH SCHEMA oauth_hydra;
--rollback DROP EXTENSION IF EXISTS "uuid-ossp";
