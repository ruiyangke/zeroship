--liquibase formatted sql

-- Every platform/system table lives in ONE `zeroship` schema. Later files
-- create schema-qualified objects (zeroship.*), so the extension and schema
-- must exist first.
--
-- citext backs case-insensitive emails (zeroship.users.email and friends).
-- uuid-ossp is deliberately NOT installed: it is unused; every UUID default
-- in the schema is gen_random_uuid() (pgcrypto, built into Postgres 13+).

--changeset zeroship:0001-extensions splitStatements:true
CREATE EXTENSION IF NOT EXISTS citext;
--rollback DROP EXTENSION IF EXISTS citext;

--changeset zeroship:0001-schemas splitStatements:true
CREATE SCHEMA IF NOT EXISTS zeroship;
--rollback DROP SCHEMA IF EXISTS zeroship CASCADE;
