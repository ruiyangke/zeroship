--liquibase formatted sql

-- accept GCP-zone-suffixed regions. Transcribed from
-- crates/sandbox/migrations/0008_relax_hosts_region_regex.sql
-- (sandbox.* → zeroship.*).
--
-- The 0011 CHECK `region ~ '^[a-z]{2}-[a-z]+-[0-9]+$'` only accepts
-- canonical region names like `us-central-1` — it rejects real GCP zone
-- names like `asia-northeast3-a` that the production controller naturally
-- produces. Relax to `^[a-z][a-z0-9-]{1,63}$` — still bounded + lowercase-
-- ASCII + dash-only, but accepts canonical AWS-shape, GCP zone, AWS AZ
-- shape, Azure compact, and private DC names.
--
-- Pg auto-names the inline CHECK `hosts_region_check` regardless of schema.

--changeset zeroship-sandbox:sandbox-relax-hosts-region-regex splitStatements:true
ALTER TABLE zeroship.hosts
    DROP CONSTRAINT IF EXISTS hosts_region_check;
ALTER TABLE zeroship.hosts
    ADD CONSTRAINT hosts_region_check
    CHECK (region ~ '^[a-z][a-z0-9-]{1,63}$');
--rollback ALTER TABLE zeroship.hosts DROP CONSTRAINT IF EXISTS hosts_region_check;
