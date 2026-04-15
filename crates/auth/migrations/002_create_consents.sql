-- Per-app consent grants.
-- Records that a user has authorized an app to access their profile.

CREATE TABLE auth_app_consents (
    id          SERIAL PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES auth_users(id),
    app_id      UUID NOT NULL,
    granted_at  TIMESTAMPTZ DEFAULT NOW(),
    revoked_at  TIMESTAMPTZ,
    UNIQUE (user_id, app_id)
);
