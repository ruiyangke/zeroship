--liquibase formatted sql

-- accept base64url alphabet for share token_id values. Transcribed from
-- crates/sandbox/migrations/0003_share_token_id_alphabet.sql
-- (sandbox.* → zeroship.*).
--
-- The Phase-3 mint handler produces a raw `tid` claim using the
-- base64url alphabet (A–Z, a–z, 0–9, '-', '_'). The 0011 token_id CHECK
-- accepted only base62 (`^tok_[0-9A-Za-z]{20,40}$`), so the handler had
-- to munge `-`/`_` → `x` before INSERT — a lossy mapping. This widens the
-- CHECK to the base64url alphabet so the handler can store
-- `tok_<raw_tid>` byte-exactly.
--
-- Pg auto-names anonymous CHECKs as `<table>_<column>_check`, so the live
-- constraint name is `shares_token_id_check`.

--changeset zeroship-sandbox:sandbox-share-token-id-alphabet splitStatements:true
ALTER TABLE zeroship.shares
    DROP CONSTRAINT IF EXISTS shares_token_id_check;
ALTER TABLE zeroship.shares
    ADD CONSTRAINT shares_token_id_check
    CHECK (token_id ~ '^tok_[A-Za-z0-9_-]{20,40}$');
--rollback ALTER TABLE zeroship.shares DROP CONSTRAINT IF EXISTS shares_token_id_check;
