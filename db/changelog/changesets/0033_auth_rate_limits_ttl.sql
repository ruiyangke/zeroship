--liquibase formatted sql

-- Bound the growth of zeroship.rate_limits with a swept TTL (security finding
-- SEC-3).
--
-- `zeroship.rate_limits` is keyed `bucket_key TEXT PRIMARY KEY` with no TTL,
-- eviction, or GC, and is written INSERT-on-conflict per attempt. Before SEC-3
-- the per-IP bucket key was derived from a spoofable, unvalidated
-- X-Forwarded-For (login:ip:{ip}, magic:ip:{ip}, signup_ip:{ip},
-- forgot_ip:{ip}, …): a forged-IP flood minted a PERMANENT row per distinct
-- value, a storage / write-load DoS on the shared auth Postgres. The gateway
-- now strips inbound forwarding headers and the auth service validates the
-- bucket key parses as an IP, so the key space is bounded to real IPs; this
-- changeset adds the durable reaping so genuinely idle buckets (and the relay
-- `relay_seen:` dedup sentinels, which share this table) do not accumulate
-- forever.
--
-- A leaky bucket is fully refilled once it has been idle long enough to reach
-- capacity, after which deleting the row is lossless — a later request simply
-- re-INSERTs it at capacity. The slowest refill in `ratelimit.rs` reaches
-- capacity well within an hour, so a row untouched for >= 24h is safely
-- reapable (and 24h is exactly the relay sentinel's own TTL, so one sweep
-- serves both). `token_sweep` (crates/auth/src/cron/token_sweep.rs) runs the
-- DELETE hourly; this index makes that `updated_at < cutoff` scan an index
-- range scan instead of a full-table sweep.

--changeset zeroship:auth-rate-limits-updated-at-idx splitStatements:true
CREATE INDEX auth_rate_limits_updated_at_idx
    ON zeroship.rate_limits (updated_at);
--rollback DROP INDEX zeroship.auth_rate_limits_updated_at_idx;
