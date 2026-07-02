-- zeroship.app_scope_defs — declared-scope registry (auth-sdk Slice 3, spec
-- §5.1 / §8.1).
--
-- Each hosted creator app declares its CUSTOM end-user OAuth scopes in its
-- manifest (`auth.scopes` — { id, label, description }). On deploy the control
-- plane validates each id (format + platform-vocabulary collision) and
-- persists the set here, ATOMICALLY mirroring the same set into the per-app
-- OAuth client's `scope` allowlist (sync_app_scopes, §5.1). The single
-- atomicity invariant is what keeps the consent classifier sound: /authorize
-- can never accept a scope that this registry hasn't learned.
--
-- These are namespace-(b) end-user scopes (self-grantable, never
-- platform-delegated). The consent screen renders each declared scope with its
-- label + description from this table.
--
-- FK order: zeroship.apps already exists (0004_control.sql), so this 0007
-- runs cleanly after it.

CREATE TABLE zeroship.app_scope_defs (
    app_id       UUID NOT NULL REFERENCES zeroship.apps(id) ON DELETE CASCADE,
    scope_id     TEXT NOT NULL,
    label        TEXT NOT NULL,
    description  TEXT,
    PRIMARY KEY (app_id, scope_id)
);
