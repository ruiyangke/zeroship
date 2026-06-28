-- Platform-owned (system) app flag (ISS-12b).
--
-- The ISS-12 account-erase reaper (auth) hard-deletes a user; app_members
-- cascade-deletes, leaving that user's apps OWNER-LESS. zeroship.apps has no FK
-- to users, so the orphaned apps row (plus its bundle/blobs in object storage)
-- is never torn down. The control-side orphaned_app_reaper sweeps owner-less
-- apps and purges them (VFS + DB cascade + Hydra client).
--
-- The landmine: bootstrap_console seeds the platform console into zeroship.apps
-- with NO app_members owner row (the console is owner-less BY CONSTRUCTION).
-- A naive "delete owner-less apps" sweep would DELETE THE PLATFORM'S OWN
-- CONSOLE. This column marks platform-owned apps so the reaper can exclude
-- them. The console seed sets system = true (idempotent, every control boot).
--
-- DEFAULT false: every creator app is a tenant app and a reap candidate once
-- owner-less. No backfill needed (pre-launch, no rows; the console seed upserts
-- the flag on the next boot).

ALTER TABLE zeroship.apps ADD COLUMN system BOOLEAN NOT NULL DEFAULT false;
