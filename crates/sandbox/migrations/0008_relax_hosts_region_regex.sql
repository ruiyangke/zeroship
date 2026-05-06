-- 0008_relax_hosts_region_regex.sql — accept GCP-zone-suffixed regions
--
-- The 0001 CHECK `region ~ '^[a-z]{2}-[a-z]+-[0-9]+$'` only accepts
-- canonical region names like `us-central-1` — it rejects real GCP
-- zone names like `asia-northeast3-a` (trailing `-a`) that the
-- production controller naturally produces from its boot environment.
-- This forced operators to lie about their region (e.g., set
-- `SANDBOX_REGION=us-local-1`) just to pass the constraint, which
-- defeats the column's audit purpose.
--
-- Relax to `^[a-z][a-z0-9-]{1,63}$` — still bounded + lowercase-
-- ASCII + dash-only, but accepts:
--   - `us-central-1`               (canonical AWS-shape)
--   - `asia-northeast3-a`          (GCP zone)
--   - `us-east-1a`                 (AWS AZ shape)
--   - `westeurope`                 (Azure compact)
--   - `dc-1`                       (private DC)
--
-- Forward-only and idempotent: DROP CONSTRAINT IF EXISTS first.

ALTER TABLE sandbox.hosts
    DROP CONSTRAINT IF EXISTS hosts_region_check;

ALTER TABLE sandbox.hosts
    ADD CONSTRAINT hosts_region_check
    CHECK (region ~ '^[a-z][a-z0-9-]{1,63}$');
