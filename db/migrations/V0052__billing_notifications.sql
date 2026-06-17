-- NOTIFICATIONS (billing-ops gap #26 F; design `0051` — renumbered 0052 to follow
-- the landed 0046..0051 billing changesets). Domains FIRST (referenced by the table
-- below, mirroring 0037/0048). `billing_notification_kind` is the full set the design
-- inventories; PR-6 wires the cron for payment_failed/past_due/suspended/recovered/
-- invoice_finalized/refunded. The spend_* and disputed kinds are reserved in the
-- domain (their source tables exist) so enabling them later is code-only, not a
-- domain ALTER.
CREATE DOMAIN zeroship.billing_notification_kind AS TEXT
    CHECK (VALUE IN ('payment_failed','past_due','suspended','recovered',
                     'invoice_finalized','refunded','disputed',
                     'spend_warn','spend_degrade','spend_block'));
-- TWO-PHASE state: 'pending' on claim, 'sent' after delivery (design CRITICAL-3).
CREATE DOMAIN zeroship.notification_status AS TEXT
    CHECK (VALUE IN ('pending','sent'));

-- The SEND-LEDGER, design CRITICAL-3: written CLAIM-BEFORE-SEND, TWO-PHASE
-- ('pending'->'sent'), so the claim INSERT — not the send — arbitrates the multi-node
-- race. Two instances scanning the same unsent transition both attempt
-- `INSERT ... status='pending' ON CONFLICT DO NOTHING RETURNING`; only the winner
-- (returns a row) is cleared to send, then flips to 'sent'. The notify cron ALSO holds
-- the notify-family advisory lock for the whole sweep (defence in depth).
--
-- Dedup key = (creator_id, kind, transition_id). `transition_id` is the STABLE identity
-- of the source row: for the history-driven kinds it is that row's surrogate id
-- (she_… spend-history / cbh_… creator-billing-history, added to 0041/0047 in PR-6); for
-- invoice/refund/dispute kinds it is the inv_…/ref_…/dsp_… id. TEXT so all sources share
-- one column. The source typed-id PREFIXES are pairwise-disjoint (design MINOR-3,
-- asserted by `typed_id::tests::notification_source_prefixes_are_pairwise_disjoint`) so a
-- transition_id from one source can never collide with another's.
--
-- Append-only re-drive: a 'pending' row that never sent is re-driven, not deleted — the
-- cron re-takes rows whose claimed_at is past NOTIFY_REDRIVE_HORIZON (= 15min, design
-- MAJOR-B) and retries the SEND for the SAME row, never a new claim. Guarantee:
-- at-least-once DELIVERY / exactly-once CLAIM, made idempotent at the provider by a
-- Mailer Idempotency-Key = (creator_id, kind, transition_id) (design MAJOR-A).
--
-- Creator-keyed control bookkeeping ⇒ NO app RLS (control is BYPASSRLS; the key is a
-- creator_id, not an app_id), uniform with creator_billing_status_history. Real FK to
-- creator_billing(creator_id) ON DELETE CASCADE so a stray writer cannot orphan a row.
CREATE TABLE zeroship.billing_notifications (
    creator_id    UUID NOT NULL REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE,
    kind          zeroship.billing_notification_kind NOT NULL,
    transition_id TEXT NOT NULL,
    status        zeroship.notification_status NOT NULL DEFAULT 'pending',
    claimed_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),   -- when the claim INSERT won; drives re-drive
    sent_at       TIMESTAMPTZ,                          -- set on the 'pending'->'sent' flip
    PRIMARY KEY (creator_id, kind, transition_id)
);
-- The re-drive scan reads pending rows by claimed_at; the backlog metric counts them.
CREATE INDEX idx_billing_notifications_pending
    ON zeroship.billing_notifications (claimed_at)
    WHERE status = 'pending';

DO $g$ BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='zeroship_control') THEN
    -- SELECT/INSERT/UPDATE: UPDATE is needed for the 'pending'->'sent' flip + the stale
    -- re-claim (design CRITICAL-3 two-phase). No DELETE (append-only; un-sent rows
    -- re-drive, never delete).
    EXECUTE 'GRANT SELECT, INSERT, UPDATE ON zeroship.billing_notifications TO zeroship_control';
  END IF;
END $g$;
