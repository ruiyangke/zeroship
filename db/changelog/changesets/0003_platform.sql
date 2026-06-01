--liquibase formatted sql

-- platform.* schema. Transcribed verbatim from
-- crates/auth/src/store/migrations.rs. zeroship.platform_admin_roles FKs into
-- zeroship.users, so it must run after 0002_auth.sql.

--changeset zeroship:platform-roles splitStatements:true
CREATE TABLE zeroship.platform_admin_roles (
    user_id    UUID PRIMARY KEY REFERENCES zeroship.users(id) ON DELETE CASCADE,
    role       TEXT NOT NULL CHECK (role IN ('admin','support','billing','readonly')),
    granted_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    granted_by UUID REFERENCES zeroship.users(id)
);
--rollback DROP TABLE zeroship.platform_admin_roles;
