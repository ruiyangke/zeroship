--liquibase formatted sql

-- The platform shares ONE database with per-service schemas. Later files
-- create schema-qualified objects (control.*, auth.*, platform.*), so the
-- extensions and schemas must exist first.
--
-- citext backs case-insensitive emails (auth.users.email and friends).
-- uuid-ossp is deliberately NOT installed: it is unused; every UUID default
-- in the schema is gen_random_uuid() (pgcrypto, built into Postgres 13+).

--changeset zeroship:0001-extensions splitStatements:true
CREATE EXTENSION IF NOT EXISTS citext;
--rollback DROP EXTENSION IF EXISTS citext;

--changeset zeroship:0001-schemas splitStatements:true
CREATE SCHEMA IF NOT EXISTS control;
CREATE SCHEMA IF NOT EXISTS auth;
CREATE SCHEMA IF NOT EXISTS platform;
--rollback DROP SCHEMA IF EXISTS platform CASCADE;
--rollback DROP SCHEMA IF EXISTS auth CASCADE;
--rollback DROP SCHEMA IF EXISTS control CASCADE;
