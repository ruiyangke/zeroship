--liquibase formatted sql

-- platform.* schema. Transcribed verbatim from
-- crates/auth/src/store/migrations.rs. platform.roles FKs into auth.users,
-- so it must run after 0002_auth.sql.

--changeset zeroship:platform-roles splitStatements:true
CREATE TABLE platform.roles (
    user_id    UUID PRIMARY KEY REFERENCES auth.users(id) ON DELETE CASCADE,
    role       TEXT NOT NULL CHECK (role IN ('admin','support','billing','readonly')),
    granted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    granted_by UUID REFERENCES auth.users(id)
);
--rollback DROP TABLE platform.roles;
