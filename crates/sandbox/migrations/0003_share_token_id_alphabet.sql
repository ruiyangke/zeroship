-- 0003_share_token_id_alphabet.sql — accept base64url alphabet for
-- share token_id values.
--
-- The Phase-3 mint handler produces a raw `tid` claim using the
-- base64url alphabet (A–Z, a–z, 0–9, '-', '_'). Migration 0001's
-- token_id CHECK accepted only base62 (`^tok_[0-9A-Za-z]{20,40}$`),
-- so the handler had to munge `-`/`_` → `x` before INSERT — a lossy
-- mapping that collapses tokens differing only in `-` vs `_`.
--
-- This migration widens the CHECK to the base64url alphabet so the
-- handler can store `tok_<raw_tid>` byte-exactly, restoring 1:1
-- between the API-returned `share_token_id` and the pg row.
--
-- Forward-only and idempotent.

-- Pg auto-names anonymous CHECKs as `<table>_<column>_check`. The
-- 0001 migration declared an inline `CHECK (token_id ~ ...)` so the
-- live constraint name is `shares_token_id_check`.
ALTER TABLE sandbox.shares
    DROP CONSTRAINT IF EXISTS shares_token_id_check;
ALTER TABLE sandbox.shares
    DROP CONSTRAINT IF EXISTS sandbox_shares_token_id_check;

ALTER TABLE sandbox.shares
    ADD CONSTRAINT shares_token_id_check
    CHECK (token_id ~ '^tok_[A-Za-z0-9_-]{20,40}$');
