# DSL gaps

Every `raw()` in `db/migrations-ts` is a one-object platform DDL fragment with a colocated `TODO(dsl-v2)` comment.

## 1. domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT

TODO: add structural domain CHECK support for membership arrays and SQL date/extract predicates

```sql
CREATE DOMAIN zeroship.account_state AS text
	CONSTRAINT account_state_check CHECK ((VALUE = ANY (ARRAY['active'::text, 'past_due'::text, 'suspended'::text])))
```

## 2. domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT

TODO: add structural domain CHECK support for membership arrays and SQL date/extract predicates

```sql
CREATE DOMAIN zeroship.billing_notification_kind AS text
	CONSTRAINT billing_notification_kind_check CHECK ((VALUE = ANY (ARRAY['payment_failed'::text, 'past_due'::text, 'suspended'::text, 'recovered'::text, 'invoice_finalized'::text, 'refunded'::text, 'disputed'::text, 'payout_failed'::text, 'checkout_failed'::text, 'spend_warn'::text, 'spend_degrade'::text, 'spend_block'::text])))
```

## 3. domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT

TODO: add structural domain CHECK support for membership arrays and SQL date/extract predicates

```sql
CREATE DOMAIN zeroship.billing_period AS date
	CONSTRAINT billing_period_check CHECK ((EXTRACT(day FROM VALUE) = (1)::numeric))
```

## 4. domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT

TODO: add structural domain CHECK support for membership arrays and SQL date/extract predicates

```sql
CREATE DOMAIN zeroship.credit_entry_kind AS text
	CONSTRAINT credit_entry_kind_check CHECK ((VALUE = ANY (ARRAY['grant'::text, 'promo'::text, 'goodwill'::text, 'refund_to_credit'::text, 'consumed'::text, 'void_reversal'::text, 'refund_clawback'::text])))
```

## 5. domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT

TODO: add structural domain CHECK support for membership arrays and SQL date/extract predicates

```sql
CREATE DOMAIN zeroship.dispute_status AS text
	CONSTRAINT dispute_status_check CHECK ((VALUE = ANY (ARRAY['open'::text, 'won'::text, 'lost'::text])))
```

## 6. domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT

TODO: add structural domain CHECK support for membership arrays and SQL date/extract predicates

```sql
CREATE DOMAIN zeroship.invoice_payment_kind AS text
	CONSTRAINT invoice_payment_kind_check CHECK ((VALUE = ANY (ARRAY['charge'::text, 'dispute_debit'::text, 'dispute_reversal'::text])))
```

## 7. domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT

TODO: add structural domain CHECK support for membership arrays and SQL date/extract predicates

```sql
CREATE DOMAIN zeroship.invoice_status AS text
	CONSTRAINT invoice_status_check CHECK ((VALUE = ANY (ARRAY['draft'::text, 'finalized'::text, 'void'::text])))
```

## 8. domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT

TODO: add structural domain CHECK support for membership arrays and SQL date/extract predicates

```sql
CREATE DOMAIN zeroship.metric_kind AS text
	CONSTRAINT metric_kind_check CHECK ((VALUE = ANY (ARRAY['platform'::text, 'primitive'::text, 'custom'::text])))
```

## 9. domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT

TODO: add structural domain CHECK support for membership arrays and SQL date/extract predicates

```sql
CREATE DOMAIN zeroship.notification_status AS text
	CONSTRAINT notification_status_check CHECK ((VALUE = ANY (ARRAY['pending'::text, 'sent'::text])))
```

## 10. domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT

TODO: add structural domain CHECK support for membership arrays and SQL date/extract predicates

```sql
CREATE DOMAIN zeroship.reconciliation_finding_kind AS text
	CONSTRAINT reconciliation_finding_kind_check CHECK ((VALUE = ANY (ARRAY['missed_invoice_payment'::text, 'invoice_status_drift'::text, 'refund_status_drift'::text, 'dispute_status_drift'::text, 'missing_dispute'::text])))
```

## 11. domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT

TODO: add structural domain CHECK support for membership arrays and SQL date/extract predicates

```sql
CREATE DOMAIN zeroship.reconciliation_finding_severity AS text
	CONSTRAINT reconciliation_finding_severity_check CHECK ((VALUE = ANY (ARRAY['low'::text, 'medium'::text, 'high'::text])))
```

## 12. domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT

TODO: add structural domain CHECK support for membership arrays and SQL date/extract predicates

```sql
CREATE DOMAIN zeroship.refund_destination AS text
	CONSTRAINT refund_destination_check CHECK ((VALUE = ANY (ARRAY['cash'::text, 'credit'::text])))
```

## 13. domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT

TODO: add structural domain CHECK support for membership arrays and SQL date/extract predicates

```sql
CREATE DOMAIN zeroship.refund_status AS text
	CONSTRAINT refund_status_check CHECK ((VALUE = ANY (ARRAY['pending'::text, 'issued'::text, 'failed'::text, 'canceled'::text])))
```

## 14. domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT

TODO: add structural domain CHECK support for membership arrays and SQL date/extract predicates

```sql
CREATE DOMAIN zeroship.spend_state AS text
	CONSTRAINT spend_state_check CHECK ((VALUE = ANY (ARRAY['allow'::text, 'warn'::text, 'degrade'::text, 'block'::text])))
```

## 15. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.app_audit (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    app_id uuid,
    creator_id uuid,
    actor_user_id uuid,
    actor_token_id uuid,
    action text NOT NULL,
    resource text,
    source_ip inet,
    detail jsonb,
    occurred_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 16. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.app_env_expose (
    app_id uuid NOT NULL,
    key_name text NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 17. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.app_members (
    app_id uuid NOT NULL,
    user_id uuid NOT NULL,
    role text NOT NULL,
    added_at timestamp with time zone DEFAULT now() NOT NULL,
    added_by uuid,
    CONSTRAINT app_members_role_check CHECK ((role = ANY (ARRAY['owner'::text, 'editor'::text, 'viewer'::text])))
)
```

## 18. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.app_net_grants (
    app_id uuid NOT NULL,
    host text NOT NULL,
    port integer NOT NULL,
    granted_by text NOT NULL,
    granted_at timestamp with time zone DEFAULT now() NOT NULL,
    note text,
    CONSTRAINT app_net_grants_port_check CHECK (((port >= 1) AND (port <= 65535)))
)
```

## 19. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.app_oauth_clients (
    app_id uuid NOT NULL,
    client_id text NOT NULL,
    sector_identifier text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 20. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.app_scope_defs (
    app_id uuid NOT NULL,
    scope_id text NOT NULL,
    label text NOT NULL,
    description text
)
```

## 21. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.app_secrets (
    app_id uuid NOT NULL,
    key_name text NOT NULL,
    ciphertext bytea NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 22. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.app_session_anchors (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    app_id uuid NOT NULL,
    client_id text NOT NULL,
    global_user_id uuid NOT NULL,
    refresh_token_enc bytea NOT NULL,
    refresh_family_id text NOT NULL,
    granted_scopes text[] DEFAULT '{}'::text[] NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    abs_expires_at timestamp with time zone NOT NULL,
    revoked_at timestamp with time zone
)
```

## 23. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.app_spend_limit (
    app_id uuid NOT NULL,
    spend_limit_cents bigint,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT app_spend_limit_spend_limit_cents_check CHECK (((spend_limit_cents IS NULL) OR (spend_limit_cents >= 0)))
)
```

## 24. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.app_spend_state (
    app_id uuid NOT NULL,
    state zeroship.spend_state DEFAULT 'allow'::text NOT NULL,
    spend_cents bigint DEFAULT 0 NOT NULL,
    eval_limit_cents bigint DEFAULT 0 NOT NULL,
    period zeroship.billing_period NOT NULL,
    evaluated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT app_spend_state_eval_limit_cents_check CHECK ((eval_limit_cents >= 0)),
    CONSTRAINT app_spend_state_spend_cents_check CHECK ((spend_cents >= 0))
)
```

## 25. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.app_usage (
    app_id uuid NOT NULL,
    resource text NOT NULL,
    value bigint DEFAULT 0 NOT NULL
)
```

## 26. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.app_usage_history (
    app_id uuid NOT NULL,
    period text NOT NULL,
    counters jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 27. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.app_user_identities (
    app_client_id text NOT NULL,
    global_user_id uuid NOT NULL,
    pairwise_sub text NOT NULL,
    relay_email text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    revoked_at timestamp with time zone
)
```

## 28. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.app_vars (
    app_id uuid NOT NULL,
    key_name text NOT NULL,
    value text NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 29. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.apps (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    name text NOT NULL,
    plan_id text DEFAULT 'free'::text NOT NULL,
    deploy_hash text,
    api_key text NOT NULL,
    api_key_hash text DEFAULT ''::text NOT NULL,
    env_version bigint DEFAULT 0 NOT NULL,
    suspended boolean DEFAULT false NOT NULL,
    audit_locked boolean DEFAULT false NOT NULL,
    manifest_json text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    system boolean DEFAULT false NOT NULL
)
```

## 30. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.audit_events (
    id bigint NOT NULL,
    occurred_at timestamp with time zone DEFAULT now() NOT NULL,
    event_type text NOT NULL,
    outcome text NOT NULL,
    actor_user_id uuid,
    client_id text,
    request_id text,
    ip inet,
    user_agent text,
    auth_method text,
    detail jsonb
)
```

## 31. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.authz_decisions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    occurred_at timestamp with time zone DEFAULT now() NOT NULL,
    actor_user_id uuid,
    token_id uuid,
    action text NOT NULL,
    resource_type text NOT NULL,
    resource_id text,
    decision text NOT NULL,
    matched_policies text[] DEFAULT '{}'::text[] NOT NULL,
    request_ip inet,
    request_id text,
    CONSTRAINT authz_decisions_decision_check CHECK ((decision = ANY (ARRAY['allow'::text, 'deny'::text])))
)
```

## 32. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.billing_customer_refs (
    creator_id uuid NOT NULL,
    provider text NOT NULL,
    external_id text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 33. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.billing_disputes (
    id text NOT NULL,
    invoice_id text NOT NULL,
    amount_cents bigint NOT NULL,
    currency character(3) DEFAULT 'usd'::bpchar NOT NULL,
    status zeroship.dispute_status DEFAULT 'open'::text NOT NULL,
    reason text,
    evidence_due_at timestamp with time zone,
    provider_dispute_id text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    resolved_at timestamp with time zone,
    CONSTRAINT billing_disputes_amount_cents_check CHECK ((amount_cents > 0)),
    CONSTRAINT billing_disputes_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text))
)
```

## 34. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.billing_line_provider_refs (
    invoice_id text NOT NULL,
    app_id uuid NOT NULL,
    provider text NOT NULL,
    ref_kind text NOT NULL,
    external_id text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    segment_no smallint DEFAULT 0 NOT NULL,
    CONSTRAINT billing_line_provider_refs_segment_no_check CHECK ((segment_no >= 0))
)
```

## 35. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.billing_metrics (
    metric text NOT NULL,
    kind zeroship.metric_kind NOT NULL,
    unit text NOT NULL,
    archived boolean DEFAULT false NOT NULL,
    last_seen_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    owner_app uuid
)
```

## 36. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.billing_notifications (
    creator_id uuid NOT NULL,
    kind zeroship.billing_notification_kind NOT NULL,
    transition_id text NOT NULL,
    status zeroship.notification_status DEFAULT 'pending'::text NOT NULL,
    claimed_at timestamp with time zone DEFAULT now() NOT NULL,
    sent_at timestamp with time zone
)
```

## 37. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.billing_provider_refs (
    invoice_id text NOT NULL,
    provider text NOT NULL,
    ref_kind text NOT NULL,
    external_id text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 38. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.billing_reconciliation_findings (
    id text NOT NULL,
    kind zeroship.reconciliation_finding_kind NOT NULL,
    severity zeroship.reconciliation_finding_severity DEFAULT 'medium'::text NOT NULL,
    entity_id text NOT NULL,
    our_value jsonb,
    stripe_value jsonb,
    dedup_key text NOT NULL,
    detected_at timestamp with time zone DEFAULT now() NOT NULL,
    resolved_at timestamp with time zone
)
```

## 39. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.connect_checkout_failures (
    id text NOT NULL,
    creator_id uuid NOT NULL,
    provider_payment_intent_id text NOT NULL,
    stripe_account_id text NOT NULL,
    amount_cents bigint NOT NULL,
    currency character(3) DEFAULT 'usd'::bpchar NOT NULL,
    failure_code text,
    failure_message text,
    occurred_at timestamp with time zone NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT connect_checkout_failures_amount_cents_check CHECK ((amount_cents >= 0)),
    CONSTRAINT connect_checkout_failures_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text))
)
```

## 40. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.creator_account_history (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    creator_id uuid NOT NULL,
    stripe_account_id text NOT NULL,
    linked_at timestamp with time zone DEFAULT now() NOT NULL,
    unlinked_at timestamp with time zone
)
```

## 41. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.creator_accounts (
    creator_id uuid NOT NULL,
    stripe_account_id text NOT NULL,
    onboarded_at timestamp with time zone DEFAULT now() NOT NULL,
    unlinked_at timestamp with time zone,
    charges_enabled boolean DEFAULT false NOT NULL,
    payouts_enabled boolean DEFAULT false NOT NULL,
    details_submitted boolean DEFAULT false NOT NULL
)
```

## 42. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.creator_billing (
    creator_id uuid NOT NULL,
    default_pm_set boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 43. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.creator_billing_status (
    creator_id uuid NOT NULL,
    state zeroship.account_state DEFAULT 'active'::text NOT NULL,
    past_due_since timestamp with time zone,
    suspended_at timestamp with time zone,
    last_payment_failure_at timestamp with time zone,
    failed_invoice_id text,
    last_event_at timestamp with time zone,
    last_recovered_at timestamp with time zone,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 44. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.creator_billing_status_history (
    id text NOT NULL,
    creator_id uuid NOT NULL,
    from_state zeroship.account_state NOT NULL,
    to_state zeroship.account_state NOT NULL,
    reason text,
    at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 45. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.creator_fee_policy (
    creator_id uuid NOT NULL,
    kind text NOT NULL,
    amount_cents bigint,
    percent_bps integer,
    cap_cents bigint,
    floor_cents bigint,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT creator_fee_policy_cap_nonneg CHECK (((cap_cents IS NULL) OR (cap_cents >= 0))),
    CONSTRAINT creator_fee_policy_floor_le_cap CHECK (((floor_cents IS NULL) OR (cap_cents IS NULL) OR (floor_cents <= cap_cents))),
    CONSTRAINT creator_fee_policy_floor_nonneg CHECK (((floor_cents IS NULL) OR (floor_cents >= 0))),
    CONSTRAINT creator_fee_policy_kind_check CHECK ((kind = ANY (ARRAY['fixed'::text, 'percent'::text]))),
    CONSTRAINT creator_fee_policy_shape CHECK ((((kind = 'fixed'::text) AND (amount_cents IS NOT NULL) AND (amount_cents >= 0)) OR ((kind = 'percent'::text) AND (percent_bps IS NOT NULL) AND ((percent_bps >= 0) AND (percent_bps <= 10000)))))
)
```

## 46. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.credit_ledger (
    id text NOT NULL,
    creator_id uuid NOT NULL,
    kind zeroship.credit_entry_kind NOT NULL,
    amount_cents bigint NOT NULL,
    currency character(3) DEFAULT 'usd'::bpchar NOT NULL,
    applied_invoice_id text,
    consumed_from_grant_id text,
    expires_at timestamp with time zone,
    note text,
    idempotency_key text,
    request_fingerprint text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT credit_ledger_amount_cents_check CHECK ((amount_cents <> 0)),
    CONSTRAINT credit_ledger_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text)),
    CONSTRAINT credit_ledger_grant_ref CHECK (((((kind)::text = ANY (ARRAY['consumed'::text, 'void_reversal'::text, 'refund_clawback'::text])) AND (consumed_from_grant_id IS NOT NULL)) OR (((kind)::text <> ALL (ARRAY['consumed'::text, 'void_reversal'::text, 'refund_clawback'::text])) AND (consumed_from_grant_id IS NULL)))),
    CONSTRAINT credit_ledger_kind_sign CHECK (((((kind)::text = ANY (ARRAY['consumed'::text, 'refund_clawback'::text])) AND (amount_cents < 0)) OR (((kind)::text <> ALL (ARRAY['consumed'::text, 'refund_clawback'::text])) AND (amount_cents > 0))))
)
```

## 47. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.cron_state (
    key text NOT NULL,
    last_rotated_at timestamp with time zone DEFAULT now() NOT NULL,
    notes text
)
```

## 48. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.deleted_sandboxes (
    sandbox_id text NOT NULL,
    user_id text NOT NULL,
    deleted_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT deleted_sandboxes_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT deleted_sandboxes_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))
)
```

## 49. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.device_grants (
    device_code_hash text NOT NULL,
    user_code text NOT NULL,
    status text DEFAULT 'pending'::text NOT NULL,
    principal_id uuid,
    platform_access_token_enc bytea,
    provider text NOT NULL,
    scope text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    last_polled_at timestamp with time zone,
    auth_credential_version bigint DEFAULT 0 NOT NULL,
    client_id text,
    sid text,
    poll_interval_secs integer DEFAULT 5 NOT NULL,
    CONSTRAINT device_grants_poll_interval_secs_check CHECK ((poll_interval_secs > 0)),
    CONSTRAINT device_grants_status_check CHECK ((status = ANY (ARRAY['pending'::text, 'approved'::text, 'denied'::text])))
)
```

## 50. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.dpop_jti (
    jti text NOT NULL,
    inserted_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 51. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.email_suppressions (
    email public.citext NOT NULL,
    reason text NOT NULL,
    suppressed_at timestamp with time zone DEFAULT now() NOT NULL,
    provider_msg text
)
```

## 52. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.email_verifications (
    token_hash bytea NOT NULL,
    user_id uuid NOT NULL,
    email public.citext NOT NULL,
    issued_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    consumed_at timestamp with time zone
)
```

## 53. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.federated_identities (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    user_id uuid NOT NULL,
    provider text NOT NULL,
    subject text NOT NULL,
    email_at_link public.citext,
    raw_profile jsonb,
    linked_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 54. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.gateway_sessions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    user_id uuid NOT NULL,
    app_id uuid NOT NULL,
    email public.citext,
    name text,
    avatar_url text,
    email_verified boolean DEFAULT false NOT NULL,
    issued_at timestamp with time zone DEFAULT now() NOT NULL,
    idle_expires_at timestamp with time zone NOT NULL,
    abs_expires_at timestamp with time zone NOT NULL,
    revoked_at timestamp with time zone,
    granted_scopes text[] DEFAULT '{}'::text[] NOT NULL,
    auth_time timestamp with time zone,
    amr text[] DEFAULT '{}'::text[] NOT NULL,
    sid text
)
```

## 55. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.hosts (
    host_id text NOT NULL,
    boot_id text NOT NULL,
    hostname text NOT NULL,
    region text NOT NULL,
    backend text NOT NULL,
    started_at timestamp with time zone DEFAULT now() NOT NULL,
    last_heartbeat timestamp with time zone DEFAULT now() NOT NULL,
    status text DEFAULT 'alive'::text NOT NULL,
    drain_started_at timestamp with time zone,
    version text DEFAULT ''::text NOT NULL,
    metadata jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT hosts_backend_check CHECK ((backend = ANY (ARRAY['docker'::text, 'k8s'::text, 'nomad-ch'::text]))),
    CONSTRAINT hosts_boot_id_check CHECK ((boot_id ~ '^[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT hosts_host_id_check CHECK ((host_id ~ '^hst_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT hosts_region_check CHECK ((region ~ '^[a-z][a-z0-9-]{1,63}$'::text)),
    CONSTRAINT hosts_status_check CHECK ((status = ANY (ARRAY['alive'::text, 'draining'::text, 'dead'::text])))
)
```

## 56. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.identity_links (
    principal_id uuid NOT NULL,
    provider text NOT NULL,
    provider_subject text NOT NULL,
    email text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 57. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.idp_sessions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    user_id uuid NOT NULL,
    auth_method text NOT NULL,
    amr text[] NOT NULL,
    acr text,
    auth_time timestamp with time zone DEFAULT now() NOT NULL,
    credential_version bigint DEFAULT 0 NOT NULL,
    idle_expires_at timestamp with time zone NOT NULL,
    abs_expires_at timestamp with time zone NOT NULL,
    revoked_at timestamp with time zone
)
```

## 58. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.invoice_lines (
    invoice_id text NOT NULL,
    app_id uuid NOT NULL,
    included_units bigint NOT NULL,
    fx_pico_cents_per_unit bigint NOT NULL,
    base_fee_cents bigint DEFAULT 0 NOT NULL,
    amount_cents bigint NOT NULL,
    usage_snapshot jsonb NOT NULL,
    weights_snapshot jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    segment_no smallint DEFAULT 0 NOT NULL,
    plan_id text NOT NULL,
    CONSTRAINT invoice_lines_amount_cents_check CHECK ((amount_cents >= 0)),
    CONSTRAINT invoice_lines_base_fee_cents_check CHECK ((base_fee_cents >= 0)),
    CONSTRAINT invoice_lines_fx_pico_cents_per_unit_check CHECK ((fx_pico_cents_per_unit >= 1000)),
    CONSTRAINT invoice_lines_included_units_check CHECK ((included_units >= 0)),
    CONSTRAINT invoice_lines_segment_no_check CHECK ((segment_no >= 0))
)
```

## 59. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.invoice_payments (
    id text NOT NULL,
    invoice_id text NOT NULL,
    amount_cents bigint NOT NULL,
    currency character(3) DEFAULT 'usd'::bpchar NOT NULL,
    kind zeroship.invoice_payment_kind NOT NULL,
    provider_ref text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT invoice_payments_amount_cents_check CHECK ((amount_cents <> 0)),
    CONSTRAINT invoice_payments_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text))
)
```

## 60. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.invoices (
    id text NOT NULL,
    creator_id uuid NOT NULL,
    period zeroship.billing_period NOT NULL,
    status zeroship.invoice_status DEFAULT 'draft'::text NOT NULL,
    currency character(3) DEFAULT 'usd'::bpchar NOT NULL,
    subtotal_cents bigint DEFAULT 0 NOT NULL,
    credit_cents bigint DEFAULT 0 NOT NULL,
    tax_cents bigint DEFAULT 0 NOT NULL,
    total_cents bigint DEFAULT 0 NOT NULL,
    finalized_at timestamp with time zone,
    voided_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT invoice_total_balances CHECK ((total_cents = ((subtotal_cents - credit_cents) + tax_cents))),
    CONSTRAINT invoices_credit_cents_check CHECK ((credit_cents >= 0)),
    CONSTRAINT invoices_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text)),
    CONSTRAINT invoices_subtotal_cents_check CHECK ((subtotal_cents >= 0)),
    CONSTRAINT invoices_tax_cents_check CHECK ((tax_cents >= 0)),
    CONSTRAINT invoices_total_cents_check CHECK ((total_cents >= 0))
)
```

## 61. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.jwk_key_state (
    set_name text NOT NULL,
    kid text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 62. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.magic_completions (
    csrf_nonce text NOT NULL,
    code text NOT NULL,
    email public.citext NOT NULL,
    login_challenge text NOT NULL,
    attempts smallint DEFAULT 0 NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    consumed_pending_at timestamp with time zone,
    consumed_at timestamp with time zone
)
```

## 63. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.magic_links (
    token_hash bytea NOT NULL,
    email public.citext NOT NULL,
    csrf_nonce text NOT NULL,
    purpose text NOT NULL,
    request_ip inet,
    request_ua text,
    issued_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    consumed_pending_at timestamp with time zone,
    consumed_at timestamp with time zone,
    user_id uuid
)
```

## 64. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.metering_exports (
    creator_id uuid NOT NULL,
    period zeroship.billing_period NOT NULL,
    exported_units bigint DEFAULT 0 NOT NULL,
    consecutive_failures integer DEFAULT 0 NOT NULL,
    last_error text,
    last_attempt_at timestamp with time zone,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT metering_exports_consecutive_failures_check CHECK ((consecutive_failures >= 0)),
    CONSTRAINT metering_exports_exported_units_check CHECK ((exported_units >= 0))
)
```

## 65. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.metric_weights (
    metric text NOT NULL,
    units_per_op bigint NOT NULL,
    per_units bigint NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT metric_weights_per_units_check CHECK ((per_units > 0)),
    CONSTRAINT metric_weights_units_per_op_check CHECK ((units_per_op >= 0))
)
```

## 66. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.migrated_app_policies (
    app_id uuid NOT NULL,
    version bigint NOT NULL,
    raw_toml text NOT NULL,
    parsed_profile jsonb NOT NULL,
    effective_profile jsonb NOT NULL,
    ceiling_id text NOT NULL,
    ceiling_version bigint NOT NULL,
    submitted_by uuid NOT NULL,
    submitted_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT migrated_app_policies_ceiling_version_check CHECK ((ceiling_version > 0)),
    CONSTRAINT migrated_app_policies_version_check CHECK ((version > 0))
)
```

## 67. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.migrated_migration_audit (
    audit_id uuid DEFAULT gen_random_uuid() NOT NULL,
    app_id uuid NOT NULL,
    migration_id uuid NOT NULL,
    migration_versions jsonb DEFAULT '[]'::jsonb NOT NULL,
    action text NOT NULL,
    outcome text NOT NULL,
    principal_id uuid NOT NULL,
    effective_profile jsonb NOT NULL,
    sealed_profile jsonb,
    ceiling_id text NOT NULL,
    ceiling_version bigint NOT NULL,
    detail jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT migrated_migration_audit_action_check CHECK ((action = ANY (ARRAY['submit'::text, 'reject_pending'::text, 'approve'::text, 'apply'::text]))),
    CONSTRAINT migrated_migration_audit_ceiling_version_check CHECK ((ceiling_version > 0))
)
```

## 68. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.migrated_migrations (
    app_id uuid NOT NULL,
    migration_id uuid NOT NULL,
    status text NOT NULL,
    request_body jsonb NOT NULL,
    effective_profile jsonb NOT NULL,
    ceiling_id text NOT NULL,
    ceiling_version bigint NOT NULL,
    gated_versions jsonb DEFAULT '[]'::jsonb NOT NULL,
    submitted_by uuid NOT NULL,
    submitted_at timestamp with time zone DEFAULT now() NOT NULL,
    approved_by uuid,
    approved_at timestamp with time zone,
    applied_at timestamp with time zone,
    last_error text,
    CONSTRAINT migrated_migrations_ceiling_version_check CHECK ((ceiling_version > 0)),
    CONSTRAINT migrated_migrations_status_check CHECK ((status = ANY (ARRAY['submitted'::text, 'pending_approval'::text, 'approved'::text, 'applied'::text, 'failed'::text])))
)
```

## 69. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.net_policy_catalog (
    key text NOT NULL,
    value_json jsonb NOT NULL,
    updated_by text,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 70. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.oauth_authorization_codes (
    code_hash bytea NOT NULL,
    client_id text NOT NULL,
    redirect_uri text NOT NULL,
    pkce_challenge text NOT NULL,
    pkce_method text NOT NULL,
    requested_scopes text[] NOT NULL,
    granted_scopes text[] NOT NULL,
    nonce text,
    user_id uuid NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    consumed_at timestamp with time zone,
    auth_credential_version bigint DEFAULT 0 NOT NULL,
    sid text NOT NULL,
    CONSTRAINT oauth_authorization_codes_max_ttl CHECK ((expires_at <= (created_at + '00:01:00'::interval))),
    CONSTRAINT oauth_authorization_codes_pkce_method_check CHECK ((pkce_method = 'S256'::text))
)
```

## 71. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.oauth_clients (
    client_id text NOT NULL,
    client_name text NOT NULL,
    client_uri text,
    logo_uri text,
    redirect_uris text[] NOT NULL,
    scopes text[] NOT NULL,
    skip_consent boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    created_by uuid,
    client_secret_hash text,
    refresh_allowed boolean DEFAULT false NOT NULL,
    token_endpoint_auth_method text DEFAULT 'none'::text NOT NULL,
    brokered boolean DEFAULT false NOT NULL,
    backchannel_logout_uri text,
    CONSTRAINT oauth_clients_brokered_requires_secret_basic CHECK (((brokered = false) OR (token_endpoint_auth_method = 'client_secret_basic'::text))),
    CONSTRAINT oauth_clients_token_endpoint_auth_method_check CHECK ((token_endpoint_auth_method = ANY (ARRAY['none'::text, 'client_secret_basic'::text, 'client_secret_post'::text])))
)
```

## 72. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.oauth_grants (
    user_id uuid NOT NULL,
    client_id text NOT NULL,
    granted_scopes text[] NOT NULL,
    granted_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    last_used_at timestamp with time zone
)
```

## 73. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.oauth_refresh_tokens (
    token_hash bytea NOT NULL,
    hash_key_version smallint NOT NULL,
    refresh_family_id text NOT NULL,
    replaced_by_token_hash bytea,
    client_id text NOT NULL,
    user_id uuid NOT NULL,
    sub text NOT NULL,
    granted_scopes text[] NOT NULL,
    family_granted_scopes text[] NOT NULL,
    issued_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    family_absolute_expires_at timestamp with time zone NOT NULL,
    consumed_at timestamp with time zone,
    rotated_at timestamp with time zone,
    revoked_at timestamp with time zone,
    last_used_at timestamp with time zone,
    idem_response_enc bytea,
    idem_expires_at timestamp with time zone,
    CONSTRAINT oauth_refresh_tokens_idle_le_ceiling CHECK ((expires_at <= family_absolute_expires_at))
)
```

## 74. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.oidc_session_clients (
    idp_session_id uuid NOT NULL,
    user_id uuid NOT NULL,
    client_id text NOT NULL,
    sid text NOT NULL,
    sub text NOT NULL,
    first_seen_at timestamp with time zone DEFAULT now() NOT NULL,
    last_seen_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 75. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.payout_failures (
    id text NOT NULL,
    creator_id uuid NOT NULL,
    provider_payout_id text NOT NULL,
    stripe_account_id text NOT NULL,
    amount_cents bigint NOT NULL,
    currency character(3) DEFAULT 'usd'::bpchar NOT NULL,
    failure_code text,
    failure_message text,
    occurred_at timestamp with time zone NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT payout_failures_amount_cents_check CHECK ((amount_cents >= 0)),
    CONSTRAINT payout_failures_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text))
)
```

## 76. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.payouts (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    creator_id uuid NOT NULL,
    event_id text NOT NULL,
    event_type text NOT NULL,
    gross_amount bigint NOT NULL,
    platform_fee bigint NOT NULL,
    net_amount bigint NOT NULL,
    currency text NOT NULL,
    occurred_at timestamp with time zone NOT NULL,
    payload_hash bytea,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT control_payouts_currency_shape CHECK ((currency ~ '^[a-z]{3}$'::text)),
    CONSTRAINT control_payouts_fee_lte_gross CHECK ((platform_fee <= gross_amount)),
    CONSTRAINT control_payouts_fee_nonnegative CHECK ((platform_fee >= 0)),
    CONSTRAINT control_payouts_gross_nonnegative CHECK ((gross_amount >= 0)),
    CONSTRAINT control_payouts_net_matches_amounts CHECK ((net_amount = (gross_amount - platform_fee))),
    CONSTRAINT control_payouts_net_nonnegative CHECK ((net_amount >= 0))
)
```

## 77. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.pending_disputes (
    provider_dispute_id text NOT NULL,
    payment_intent text,
    charge text,
    amount_cents bigint NOT NULL,
    currency character(3) DEFAULT 'usd'::bpchar NOT NULL,
    reason text,
    evidence_due_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT pending_disputes_amount_cents_check CHECK ((amount_cents > 0)),
    CONSTRAINT pending_disputes_check CHECK (((payment_intent IS NOT NULL) OR (charge IS NOT NULL))),
    CONSTRAINT pending_disputes_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text))
)
```

## 78. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.permission_tokens (
    id uuid NOT NULL,
    owner_id uuid NOT NULL,
    kind text NOT NULL,
    client_id text,
    name text NOT NULL,
    policies jsonb NOT NULL,
    policy_hash text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone,
    revoked_at timestamp with time zone,
    last_used_at timestamp with time zone,
    CONSTRAINT permission_tokens_kind_check CHECK ((kind = ANY (ARRAY['pat'::text, 'oauth_grant'::text])))
)
```

## 79. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.plan_change_events (
    id text NOT NULL,
    app_id uuid NOT NULL,
    period zeroship.billing_period NOT NULL,
    from_plan_id text,
    to_plan_id text NOT NULL,
    effective_at timestamp with time zone DEFAULT now() NOT NULL,
    usage_at_change jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 80. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.plans (
    id text NOT NULL,
    name text NOT NULL,
    base_fee_cents bigint DEFAULT 0 NOT NULL,
    included_units bigint DEFAULT 0 NOT NULL,
    fx_pico_cents_per_unit bigint,
    spend_limit_default_cents bigint DEFAULT 0 NOT NULL,
    assignable_by_creator boolean DEFAULT false NOT NULL,
    runtime_limits_json jsonb NOT NULL,
    archived boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    net_policy_limits_json jsonb DEFAULT '{"max_sockets": 4, "egress_ceiling_bytes": 10485760}'::jsonb NOT NULL,
    CONSTRAINT plans_base_fee_cents_check CHECK ((base_fee_cents >= 0)),
    CONSTRAINT plans_fx_pico_cents_per_unit_check CHECK (((fx_pico_cents_per_unit IS NULL) OR (fx_pico_cents_per_unit >= 1000))),
    CONSTRAINT plans_included_units_check CHECK ((included_units >= 0)),
    CONSTRAINT plans_spend_limit_default_cents_check CHECK ((spend_limit_default_cents >= 0))
)
```

## 81. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.platform_admin_roles (
    user_id uuid NOT NULL,
    role text NOT NULL,
    granted_at timestamp with time zone DEFAULT now() NOT NULL,
    granted_by uuid,
    CONSTRAINT platform_admin_roles_role_check CHECK ((role = ANY (ARRAY['admin'::text, 'support'::text, 'billing'::text, 'readonly'::text])))
)
```

## 82. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.platform_policies (
    id text NOT NULL,
    cedar_source text NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_by uuid
)
```

## 83. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.pricing_config (
    id text DEFAULT 'global'::text NOT NULL,
    fx_pico_cents_per_unit bigint NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT pricing_config_fx_pico_cents_per_unit_check CHECK ((fx_pico_cents_per_unit >= 1000)),
    CONSTRAINT pricing_config_id_check CHECK ((id = 'global'::text))
)
```

## 84. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.principal_grants (
    principal_id uuid NOT NULL,
    grant_name text NOT NULL
)
```

## 85. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.rate_limits (
    bucket_key text NOT NULL,
    tokens real NOT NULL,
    updated_at timestamp with time zone NOT NULL
)
```

## 86. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.refund_provider_refs (
    refund_id text NOT NULL,
    provider text NOT NULL,
    ref_kind text NOT NULL,
    external_id text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 87. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.refunds (
    id text NOT NULL,
    invoice_id text NOT NULL,
    amount_cents bigint NOT NULL,
    subtotal_cents bigint NOT NULL,
    tax_cents bigint DEFAULT 0 NOT NULL,
    currency character(3) DEFAULT 'usd'::bpchar NOT NULL,
    destination zeroship.refund_destination NOT NULL,
    reason text,
    idempotency_key text NOT NULL,
    request_fingerprint text NOT NULL,
    status zeroship.refund_status DEFAULT 'pending'::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    issued_at timestamp with time zone,
    failed_at timestamp with time zone,
    CONSTRAINT refund_amount_split CHECK ((amount_cents = (subtotal_cents + tax_cents))),
    CONSTRAINT refunds_amount_cents_check CHECK ((amount_cents > 0)),
    CONSTRAINT refunds_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text)),
    CONSTRAINT refunds_subtotal_cents_check CHECK ((subtotal_cents >= 0)),
    CONSTRAINT refunds_tax_cents_check CHECK ((tax_cents >= 0))
)
```

## 88. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.sandbox_events (
    event_id text NOT NULL,
    sandbox_id text,
    user_id text NOT NULL,
    kind text NOT NULL,
    ts timestamp with time zone DEFAULT now() NOT NULL,
    data jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192)),
    CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))
)
PARTITION BY RANGE (ts)
```

## 89. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.sandbox_events_2026_05 (
    event_id text NOT NULL,
    sandbox_id text,
    user_id text NOT NULL,
    kind text NOT NULL,
    ts timestamp with time zone DEFAULT now() NOT NULL,
    data jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192)),
    CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))
)
```

## 90. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.sandbox_events_2026_06 (
    event_id text NOT NULL,
    sandbox_id text,
    user_id text NOT NULL,
    kind text NOT NULL,
    ts timestamp with time zone DEFAULT now() NOT NULL,
    data jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192)),
    CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))
)
```

## 91. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.sandbox_events_2026_07 (
    event_id text NOT NULL,
    sandbox_id text,
    user_id text NOT NULL,
    kind text NOT NULL,
    ts timestamp with time zone DEFAULT now() NOT NULL,
    data jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192)),
    CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))
)
```

## 92. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.sandbox_events_2026_08 (
    event_id text NOT NULL,
    sandbox_id text,
    user_id text NOT NULL,
    kind text NOT NULL,
    ts timestamp with time zone DEFAULT now() NOT NULL,
    data jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192)),
    CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))
)
```

## 93. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.sandbox_events_2026_09 (
    event_id text NOT NULL,
    sandbox_id text,
    user_id text NOT NULL,
    kind text NOT NULL,
    ts timestamp with time zone DEFAULT now() NOT NULL,
    data jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192)),
    CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))
)
```

## 94. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.sandbox_events_2026_10 (
    event_id text NOT NULL,
    sandbox_id text,
    user_id text NOT NULL,
    kind text NOT NULL,
    ts timestamp with time zone DEFAULT now() NOT NULL,
    data jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192)),
    CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))
)
```

## 95. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.sandbox_events_default (
    event_id text NOT NULL,
    sandbox_id text,
    user_id text NOT NULL,
    kind text NOT NULL,
    ts timestamp with time zone DEFAULT now() NOT NULL,
    data jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT sandbox_events_data_check CHECK ((pg_column_size(data) <= 8192)),
    CONSTRAINT sandbox_events_event_id_check CHECK ((event_id ~ '^evt_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandbox_events_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))
)
```

## 96. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.sandboxes (
    sandbox_id text NOT NULL,
    user_id text NOT NULL,
    project_id text NOT NULL,
    backend text NOT NULL,
    vm_index integer,
    agent_url text,
    host_id text NOT NULL,
    generation bigint DEFAULT 0 NOT NULL,
    status text DEFAULT 'starting'::text NOT NULL,
    key_fp text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    started_at timestamp with time zone,
    stopped_at timestamp with time zone,
    last_used_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    metadata jsonb DEFAULT '{}'::jsonb NOT NULL,
    snapshot_artifact_path text,
    snapshot_taken_at timestamp with time zone,
    snapshot_ch_version text,
    snapshot_sha256 bytea,
    snapshot_aead_dek_id text,
    snapshot_backing_versions jsonb,
    snapshot_vm_index smallint,
    lessee_updated_at timestamp with time zone,
    last_running_worker_id text,
    idle_snapshot_opted_in boolean DEFAULT false NOT NULL,
    idle_snapshot_count_long_poll boolean DEFAULT false NOT NULL,
    last_drain_failure_at timestamp with time zone,
    drain_failure_count integer DEFAULT 0 NOT NULL,
    CONSTRAINT sandboxes_backend_check CHECK ((backend = ANY (ARRAY['docker'::text, 'k8s'::text, 'nomad-ch'::text]))),
    CONSTRAINT sandboxes_generation_check CHECK ((generation >= 0)),
    CONSTRAINT sandboxes_key_fp_check CHECK ((key_fp ~ '^[0-9a-f]{32}$'::text)),
    CONSTRAINT sandboxes_project_id_check CHECK ((project_id ~ '^prj_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandboxes_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT sandboxes_snapshot_artifact_consistency CHECK (((status <> ALL (ARRAY['snapshotted'::text, 'snapshotted_suspect'::text])) OR ((snapshot_artifact_path IS NOT NULL) AND (snapshot_sha256 IS NOT NULL) AND (snapshot_ch_version IS NOT NULL)))),
    CONSTRAINT sandboxes_status_check CHECK ((status = ANY (ARRAY['starting'::text, 'running'::text, 'stopping'::text, 'stopped'::text, 'lost'::text, 'recreating'::text, 'orphan'::text, 'unreachable'::text, 'snapshotting'::text, 'snapshotted'::text, 'snapshotting_aborted'::text, 'snapshotted_suspect'::text, 'restoring'::text, 'restoring_cold'::text]))),
    CONSTRAINT sandboxes_user_id_check CHECK ((user_id ~ '^usr_[0-9A-Za-z]{20,40}$'::text))
)
```

## 97. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.shares (
    token_id text NOT NULL,
    sandbox_id text NOT NULL,
    port integer NOT NULL,
    scope text NOT NULL,
    secret_version integer NOT NULL,
    issued_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    revoked_at timestamp with time zone,
    use_count bigint DEFAULT 0 NOT NULL,
    last_used_at timestamp with time zone,
    iss text,
    deleted_at timestamp with time zone,
    CONSTRAINT shares_check CHECK ((expires_at > issued_at)),
    CONSTRAINT shares_check1 CHECK (((revoked_at IS NULL) OR (revoked_at >= issued_at))),
    CONSTRAINT shares_iss_check CHECK (((iss IS NULL) OR (iss ~ '^usr_[0-9A-Za-z]{20,40}$'::text))),
    CONSTRAINT shares_port_check CHECK (((port >= 1) AND (port <= 65535))),
    CONSTRAINT shares_scope_check CHECK ((scope = ANY (ARRAY['ro'::text, 'rw'::text]))),
    CONSTRAINT shares_secret_version_check CHECK ((secret_version >= 1)),
    CONSTRAINT shares_token_id_check CHECK ((token_id ~ '^tok_[A-Za-z0-9_-]{20,40}$'::text))
)
```

## 98. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.signing_keys (
    kid text NOT NULL,
    alg text NOT NULL,
    public_jwk jsonb NOT NULL,
    status text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    activated_at timestamp with time zone,
    retiring_at timestamp with time zone,
    retired_at timestamp with time zone,
    CONSTRAINT signing_keys_alg_check CHECK ((alg = 'EdDSA'::text)),
    CONSTRAINT signing_keys_status_check CHECK ((status = ANY (ARRAY['active'::text, 'next'::text, 'retiring'::text])))
)
```

## 99. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.spend_state_history (
    id text NOT NULL,
    app_id uuid NOT NULL,
    period zeroship.billing_period NOT NULL,
    from_state zeroship.spend_state NOT NULL,
    to_state zeroship.spend_state NOT NULL,
    spend_cents bigint NOT NULL,
    limit_cents bigint,
    at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT spend_state_history_limit_cents_check CHECK (((limit_cents IS NULL) OR (limit_cents >= 0))),
    CONSTRAINT spend_state_history_spend_cents_check CHECK ((spend_cents >= 0))
)
```

## 100. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.stripe_events_seen (
    event_id text NOT NULL,
    event_type text NOT NULL,
    seen_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 101. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.token_revocations (
    client_id text NOT NULL,
    sub text NOT NULL,
    revoked_after timestamp with time zone DEFAULT now() NOT NULL
)
```

## 102. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.totp_backup_codes (
    id bigint NOT NULL,
    user_id uuid NOT NULL,
    code_hash text NOT NULL,
    used_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 103. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.totp_credentials (
    user_id uuid NOT NULL,
    encrypted_secret bytea NOT NULL,
    confirmed_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 104. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.usage_aggregates (
    app_id uuid NOT NULL,
    period zeroship.billing_period NOT NULL,
    metric text NOT NULL,
    total bigint DEFAULT 0 NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT usage_aggregates_total_check CHECK ((total >= 0))
)
```

## 105. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.usage_reports_seen (
    worker_id text NOT NULL,
    sequence bigint NOT NULL,
    period zeroship.billing_period,
    seen_at timestamp with time zone DEFAULT now() NOT NULL
)
```

## 106. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.users (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    email public.citext NOT NULL,
    email_verified_at timestamp with time zone,
    name text NOT NULL,
    avatar_url text,
    password_hash text,
    credential_version bigint DEFAULT 0 NOT NULL,
    locked_until timestamp with time zone,
    disabled_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    last_login_at timestamp with time zone,
    failed_login_count integer DEFAULT 0 NOT NULL,
    deletion_requested_at timestamp with time zone,
    deletion_scheduled_for timestamp with time zone,
    anonymized_at timestamp with time zone
)
```

## 107. table().create currently routes through the app collection snapshot and injects runtime system fields, so it cannot reproduce an exact platform table

TODO: add an exact/platform table creation mode that does not inject app runtime system fields or the synthetic id primary key

```sql
CREATE TABLE zeroship.wake_jobs (
    wake_id text NOT NULL,
    sandbox_id text NOT NULL,
    state text NOT NULL,
    error_code text,
    error_message text,
    started_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    ready_at timestamp with time zone,
    agent_url text,
    lessee text NOT NULL,
    lessee_updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT wake_jobs_agent_url_chk CHECK (((agent_url IS NULL) OR (agent_url ~ '^https?://[a-zA-Z0-9._:/-]+$'::text))),
    CONSTRAINT wake_jobs_error_code_check CHECK (((error_code IS NULL) OR (error_code = ANY (ARRAY['slot_unavailable'::text, 'source_teardown_timeout'::text, 'restore_failed'::text, 'livez_timeout'::text, 'clock_resync_failed'::text, 'register_failed'::text, 'internal'::text, 'wake_worker_aborted'::text, 'staging_path_missing'::text, 'agent_version_mismatch'::text])))),
    CONSTRAINT wake_jobs_sandbox_id_check CHECK ((sandbox_id ~ '^sbx_[0-9A-Za-z]{20,40}$'::text)),
    CONSTRAINT wake_jobs_state_check CHECK ((state = ANY (ARRAY['pending'::text, 'reserving_slot'::text, 'restoring'::text, 'livez_polling'::text, 'clock_resyncing'::text, 'registering'::text, 'ok'::text, 'failed'::text])))
)
```

## 108. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER TABLE zeroship.totp_backup_codes ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME zeroship.totp_backup_codes_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
)
```

## 109. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER TABLE ONLY zeroship.sandbox_events ATTACH PARTITION zeroship.sandbox_events_2026_05 FOR VALUES FROM ('2026-05-01 00:00:00+00') TO ('2026-06-01 00:00:00+00')
```

## 110. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER TABLE ONLY zeroship.sandbox_events ATTACH PARTITION zeroship.sandbox_events_2026_06 FOR VALUES FROM ('2026-06-01 00:00:00+00') TO ('2026-07-01 00:00:00+00')
```

## 111. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER TABLE ONLY zeroship.sandbox_events ATTACH PARTITION zeroship.sandbox_events_2026_07 FOR VALUES FROM ('2026-07-01 00:00:00+00') TO ('2026-08-01 00:00:00+00')
```

## 112. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER TABLE ONLY zeroship.sandbox_events ATTACH PARTITION zeroship.sandbox_events_2026_08 FOR VALUES FROM ('2026-08-01 00:00:00+00') TO ('2026-09-01 00:00:00+00')
```

## 113. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER TABLE ONLY zeroship.sandbox_events ATTACH PARTITION zeroship.sandbox_events_2026_09 FOR VALUES FROM ('2026-09-01 00:00:00+00') TO ('2026-10-01 00:00:00+00')
```

## 114. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER TABLE ONLY zeroship.sandbox_events ATTACH PARTITION zeroship.sandbox_events_2026_10 FOR VALUES FROM ('2026-10-01 00:00:00+00') TO ('2026-11-01 00:00:00+00')
```

## 115. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER TABLE ONLY zeroship.sandbox_events ATTACH PARTITION zeroship.sandbox_events_default DEFAULT
```

## 116. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER TABLE ONLY zeroship.audit_events ALTER COLUMN id SET DEFAULT nextval('zeroship.audit_events_id_seq'::regclass)
```

## 117. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.app_audit
    ADD CONSTRAINT app_audit_pkey PRIMARY KEY (id)
```

## 118. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.app_env_expose
    ADD CONSTRAINT app_env_expose_pkey PRIMARY KEY (app_id, key_name)
```

## 119. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.app_members
    ADD CONSTRAINT app_members_pkey PRIMARY KEY (app_id, user_id)
```

## 120. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.app_net_grants
    ADD CONSTRAINT app_net_grants_pkey PRIMARY KEY (app_id, host, port)
```

## 121. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.app_oauth_clients
    ADD CONSTRAINT app_oauth_clients_pkey PRIMARY KEY (app_id)
```

## 122. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.app_scope_defs
    ADD CONSTRAINT app_scope_defs_pkey PRIMARY KEY (app_id, scope_id)
```

## 123. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.app_secrets
    ADD CONSTRAINT app_secrets_pkey PRIMARY KEY (app_id, key_name)
```

## 124. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.app_session_anchors
    ADD CONSTRAINT app_session_anchors_pkey PRIMARY KEY (id)
```

## 125. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.app_spend_limit
    ADD CONSTRAINT app_spend_limit_pkey PRIMARY KEY (app_id)
```

## 126. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.app_spend_state
    ADD CONSTRAINT app_spend_state_pkey PRIMARY KEY (app_id)
```

## 127. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.app_usage
    ADD CONSTRAINT app_usage_pkey PRIMARY KEY (app_id, resource)
```

## 128. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.app_user_identities
    ADD CONSTRAINT app_user_identities_pkey PRIMARY KEY (app_client_id, global_user_id)
```

## 129. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.app_vars
    ADD CONSTRAINT app_vars_pkey PRIMARY KEY (app_id, key_name)
```

## 130. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.apps
    ADD CONSTRAINT apps_pkey PRIMARY KEY (id)
```

## 131. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.audit_events
    ADD CONSTRAINT audit_events_pkey PRIMARY KEY (id)
```

## 132. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.authz_decisions
    ADD CONSTRAINT authz_decisions_pkey PRIMARY KEY (id)
```

## 133. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.billing_customer_refs
    ADD CONSTRAINT billing_customer_refs_pkey PRIMARY KEY (creator_id, provider)
```

## 134. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.billing_disputes
    ADD CONSTRAINT billing_disputes_pkey PRIMARY KEY (id)
```

## 135. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.billing_line_provider_refs
    ADD CONSTRAINT billing_line_provider_refs_pkey PRIMARY KEY (invoice_id, app_id, segment_no, provider, ref_kind)
```

## 136. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.billing_metrics
    ADD CONSTRAINT billing_metrics_pkey PRIMARY KEY (metric)
```

## 137. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.billing_notifications
    ADD CONSTRAINT billing_notifications_pkey PRIMARY KEY (creator_id, kind, transition_id)
```

## 138. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.billing_provider_refs
    ADD CONSTRAINT billing_provider_refs_pkey PRIMARY KEY (invoice_id, provider, ref_kind)
```

## 139. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.billing_reconciliation_findings
    ADD CONSTRAINT billing_reconciliation_findings_pkey PRIMARY KEY (id)
```

## 140. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.connect_checkout_failures
    ADD CONSTRAINT connect_checkout_failures_pkey PRIMARY KEY (id)
```

## 141. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.creator_account_history
    ADD CONSTRAINT creator_account_history_pkey PRIMARY KEY (id)
```

## 142. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.creator_accounts
    ADD CONSTRAINT creator_accounts_pkey PRIMARY KEY (creator_id)
```

## 143. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.creator_billing
    ADD CONSTRAINT creator_billing_pkey PRIMARY KEY (creator_id)
```

## 144. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.creator_billing_status_history
    ADD CONSTRAINT creator_billing_status_history_pkey PRIMARY KEY (id)
```

## 145. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.creator_billing_status
    ADD CONSTRAINT creator_billing_status_pkey PRIMARY KEY (creator_id)
```

## 146. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.creator_fee_policy
    ADD CONSTRAINT creator_fee_policy_pkey PRIMARY KEY (creator_id)
```

## 147. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.credit_ledger
    ADD CONSTRAINT credit_ledger_pkey PRIMARY KEY (id)
```

## 148. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.cron_state
    ADD CONSTRAINT cron_state_pkey PRIMARY KEY (key)
```

## 149. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.deleted_sandboxes
    ADD CONSTRAINT deleted_sandboxes_pkey PRIMARY KEY (sandbox_id)
```

## 150. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.device_grants
    ADD CONSTRAINT device_grants_pkey PRIMARY KEY (device_code_hash)
```

## 151. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.dpop_jti
    ADD CONSTRAINT dpop_jti_pkey PRIMARY KEY (jti)
```

## 152. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.email_suppressions
    ADD CONSTRAINT email_suppressions_pkey PRIMARY KEY (email)
```

## 153. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.email_verifications
    ADD CONSTRAINT email_verifications_pkey PRIMARY KEY (token_hash)
```

## 154. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.federated_identities
    ADD CONSTRAINT federated_identities_pkey PRIMARY KEY (id)
```

## 155. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.gateway_sessions
    ADD CONSTRAINT gateway_sessions_pkey PRIMARY KEY (id)
```

## 156. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.hosts
    ADD CONSTRAINT hosts_pkey PRIMARY KEY (host_id)
```

## 157. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.identity_links
    ADD CONSTRAINT identity_links_pkey PRIMARY KEY (provider, provider_subject)
```

## 158. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.idp_sessions
    ADD CONSTRAINT idp_sessions_pkey PRIMARY KEY (id)
```

## 159. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.invoice_lines
    ADD CONSTRAINT invoice_lines_pkey PRIMARY KEY (invoice_id, app_id, segment_no)
```

## 160. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.invoice_payments
    ADD CONSTRAINT invoice_payments_pkey PRIMARY KEY (id)
```

## 161. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.invoices
    ADD CONSTRAINT invoices_pkey PRIMARY KEY (id)
```

## 162. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.jwk_key_state
    ADD CONSTRAINT jwk_key_state_pkey PRIMARY KEY (set_name, kid)
```

## 163. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.magic_completions
    ADD CONSTRAINT magic_completions_pkey PRIMARY KEY (csrf_nonce)
```

## 164. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.magic_links
    ADD CONSTRAINT magic_links_pkey PRIMARY KEY (token_hash)
```

## 165. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.metering_exports
    ADD CONSTRAINT metering_exports_pkey PRIMARY KEY (creator_id, period)
```

## 166. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.metric_weights
    ADD CONSTRAINT metric_weights_pkey PRIMARY KEY (metric)
```

## 167. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.migrated_app_policies
    ADD CONSTRAINT migrated_app_policies_pkey PRIMARY KEY (app_id, version)
```

## 168. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.migrated_migration_audit
    ADD CONSTRAINT migrated_migration_audit_pkey PRIMARY KEY (audit_id)
```

## 169. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.migrated_migrations
    ADD CONSTRAINT migrated_migrations_pkey PRIMARY KEY (app_id, migration_id)
```

## 170. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.net_policy_catalog
    ADD CONSTRAINT net_policy_catalog_pkey PRIMARY KEY (key)
```

## 171. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.oauth_authorization_codes
    ADD CONSTRAINT oauth_authorization_codes_pkey PRIMARY KEY (code_hash)
```

## 172. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.oauth_clients
    ADD CONSTRAINT oauth_clients_pkey PRIMARY KEY (client_id)
```

## 173. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.oauth_grants
    ADD CONSTRAINT oauth_grants_pkey PRIMARY KEY (user_id, client_id)
```

## 174. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.oauth_refresh_tokens
    ADD CONSTRAINT oauth_refresh_tokens_pkey PRIMARY KEY (token_hash)
```

## 175. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.oidc_session_clients
    ADD CONSTRAINT oidc_session_clients_pkey PRIMARY KEY (idp_session_id, client_id)
```

## 176. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.payout_failures
    ADD CONSTRAINT payout_failures_pkey PRIMARY KEY (id)
```

## 177. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.payouts
    ADD CONSTRAINT payouts_pkey PRIMARY KEY (id)
```

## 178. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.pending_disputes
    ADD CONSTRAINT pending_disputes_pkey PRIMARY KEY (provider_dispute_id)
```

## 179. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.permission_tokens
    ADD CONSTRAINT permission_tokens_pkey PRIMARY KEY (id)
```

## 180. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.plan_change_events
    ADD CONSTRAINT plan_change_events_pkey PRIMARY KEY (id)
```

## 181. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.plans
    ADD CONSTRAINT plans_pkey PRIMARY KEY (id)
```

## 182. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.platform_admin_roles
    ADD CONSTRAINT platform_admin_roles_pkey PRIMARY KEY (user_id)
```

## 183. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.platform_policies
    ADD CONSTRAINT platform_policies_pkey PRIMARY KEY (id)
```

## 184. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.pricing_config
    ADD CONSTRAINT pricing_config_pkey PRIMARY KEY (id)
```

## 185. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.principal_grants
    ADD CONSTRAINT principal_grants_pkey PRIMARY KEY (principal_id, grant_name)
```

## 186. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.rate_limits
    ADD CONSTRAINT rate_limits_pkey PRIMARY KEY (bucket_key)
```

## 187. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.refund_provider_refs
    ADD CONSTRAINT refund_provider_refs_pkey PRIMARY KEY (refund_id, provider, ref_kind)
```

## 188. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.refunds
    ADD CONSTRAINT refunds_pkey PRIMARY KEY (id)
```

## 189. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.sandbox_events
    ADD CONSTRAINT sandbox_events_pkey PRIMARY KEY (ts, event_id)
```

## 190. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.sandbox_events_2026_05
    ADD CONSTRAINT sandbox_events_2026_05_pkey PRIMARY KEY (ts, event_id)
```

## 191. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.sandbox_events_2026_06
    ADD CONSTRAINT sandbox_events_2026_06_pkey PRIMARY KEY (ts, event_id)
```

## 192. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.sandbox_events_2026_07
    ADD CONSTRAINT sandbox_events_2026_07_pkey PRIMARY KEY (ts, event_id)
```

## 193. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.sandbox_events_2026_08
    ADD CONSTRAINT sandbox_events_2026_08_pkey PRIMARY KEY (ts, event_id)
```

## 194. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.sandbox_events_2026_09
    ADD CONSTRAINT sandbox_events_2026_09_pkey PRIMARY KEY (ts, event_id)
```

## 195. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.sandbox_events_2026_10
    ADD CONSTRAINT sandbox_events_2026_10_pkey PRIMARY KEY (ts, event_id)
```

## 196. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.sandbox_events_default
    ADD CONSTRAINT sandbox_events_default_pkey PRIMARY KEY (ts, event_id)
```

## 197. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.sandboxes
    ADD CONSTRAINT sandboxes_pkey PRIMARY KEY (sandbox_id)
```

## 198. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.shares
    ADD CONSTRAINT shares_pkey PRIMARY KEY (token_id)
```

## 199. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.signing_keys
    ADD CONSTRAINT signing_keys_pkey PRIMARY KEY (kid)
```

## 200. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.spend_state_history
    ADD CONSTRAINT spend_state_history_pkey PRIMARY KEY (id)
```

## 201. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.stripe_events_seen
    ADD CONSTRAINT stripe_events_seen_pkey PRIMARY KEY (event_id)
```

## 202. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.token_revocations
    ADD CONSTRAINT token_revocations_pkey PRIMARY KEY (client_id, sub)
```

## 203. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.totp_backup_codes
    ADD CONSTRAINT totp_backup_codes_pkey PRIMARY KEY (id)
```

## 204. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.totp_credentials
    ADD CONSTRAINT totp_credentials_pkey PRIMARY KEY (user_id)
```

## 205. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.usage_aggregates
    ADD CONSTRAINT usage_aggregates_pkey PRIMARY KEY (app_id, period, metric)
```

## 206. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.usage_reports_seen
    ADD CONSTRAINT usage_reports_seen_pkey PRIMARY KEY (worker_id, sequence)
```

## 207. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id)
```

## 208. the table handle has no standalone primary-key constraint operation for existing exact platform tables

TODO: add table(name).primaryKey(name).add({ columns }) for existing tables, including composite primary keys

```sql
ALTER TABLE ONLY zeroship.wake_jobs
    ADD CONSTRAINT wake_jobs_pkey PRIMARY KEY (wake_id)
```

## 209. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_oauth_clients
    ADD CONSTRAINT app_oauth_clients_client_id_key UNIQUE (client_id)
```

## 210. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_usage_history
    ADD CONSTRAINT app_usage_history_app_id_period_key UNIQUE (app_id, period)
```

## 211. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.apps
    ADD CONSTRAINT apps_name_key UNIQUE (name)
```

## 212. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.billing_customer_refs
    ADD CONSTRAINT billing_customer_refs_external_id_key UNIQUE (external_id)
```

## 213. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.billing_disputes
    ADD CONSTRAINT billing_disputes_provider_dispute_id_key UNIQUE (provider_dispute_id)
```

## 214. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.billing_line_provider_refs
    ADD CONSTRAINT billing_line_provider_refs_provider_ref_kind_external_id_key UNIQUE (provider, ref_kind, external_id)
```

## 215. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.billing_provider_refs
    ADD CONSTRAINT billing_provider_refs_provider_ref_kind_external_id_key UNIQUE (provider, ref_kind, external_id)
```

## 216. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.billing_reconciliation_findings
    ADD CONSTRAINT billing_reconciliation_findings_dedup_key_key UNIQUE (dedup_key)
```

## 217. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.connect_checkout_failures
    ADD CONSTRAINT connect_checkout_failures_provider_payment_intent_id_key UNIQUE (provider_payment_intent_id)
```

## 218. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.device_grants
    ADD CONSTRAINT device_grants_user_code_key UNIQUE (user_code)
```

## 219. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.federated_identities
    ADD CONSTRAINT federated_identities_provider_subject_key UNIQUE (provider, subject)
```

## 220. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.payout_failures
    ADD CONSTRAINT payout_failures_provider_payout_id_key UNIQUE (provider_payout_id)
```

## 221. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.payouts
    ADD CONSTRAINT payouts_event_id_key UNIQUE (event_id)
```

## 222. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.refund_provider_refs
    ADD CONSTRAINT refund_provider_refs_provider_ref_kind_external_id_key UNIQUE (provider, ref_kind, external_id)
```

## 223. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.refunds
    ADD CONSTRAINT refunds_idempotency_key_key UNIQUE (idempotency_key)
```

## 224. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.users
    ADD CONSTRAINT users_email_key UNIQUE (email)
```

## 225. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX app_members_user_idx ON zeroship.app_members USING btree (user_id)
```

## 226. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX app_net_grants_app_id_idx ON zeroship.app_net_grants USING btree (app_id)
```

## 227. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX app_session_anchors_user_idx ON zeroship.app_session_anchors USING btree (app_id, global_user_id) WHERE (revoked_at IS NULL)
```

## 228. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX app_user_identities_pairwise_sub_idx ON zeroship.app_user_identities USING btree (pairwise_sub)
```

## 229. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE UNIQUE INDEX app_user_identities_relay_active_idx ON zeroship.app_user_identities USING btree (relay_email) WHERE ((relay_email IS NOT NULL) AND (revoked_at IS NULL))
```

## 230. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX apps_plan_id_idx ON zeroship.apps USING btree (plan_id)
```

## 231. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX auth_audit_event_idx ON zeroship.audit_events USING btree (event_type, occurred_at)
```

## 232. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX auth_audit_user_idx ON zeroship.audit_events USING btree (actor_user_id, occurred_at)
```

## 233. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX auth_dpop_jti_inserted_idx ON zeroship.dpop_jti USING btree (inserted_at)
```

## 234. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX auth_gateway_sessions_app_idx ON zeroship.gateway_sessions USING btree (app_id, user_id)
```

## 235. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX auth_gateway_sessions_app_sid_idx ON zeroship.gateway_sessions USING btree (app_id, sid) WHERE ((sid IS NOT NULL) AND (revoked_at IS NULL))
```

## 236. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX auth_gateway_sessions_idle_idx ON zeroship.gateway_sessions USING btree (idle_expires_at) WHERE (revoked_at IS NULL)
```

## 237. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX auth_magic_completions_expires_idx ON zeroship.magic_completions USING btree (expires_at) WHERE (consumed_at IS NULL)
```

## 238. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX auth_magic_email_idx ON zeroship.magic_links USING btree (email)
```

## 239. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX auth_magic_user_id_idx ON zeroship.magic_links USING btree (user_id)
```

## 240. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX auth_rate_limits_updated_at_idx ON zeroship.rate_limits USING btree (updated_at)
```

## 241. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX auth_token_revocations_revoked_after_idx ON zeroship.token_revocations USING btree (revoked_after)
```

## 242. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX auth_totp_backup_codes_user_idx ON zeroship.totp_backup_codes USING btree (user_id)
```

## 243. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX auth_users_deletion_due_idx ON zeroship.users USING btree (deletion_scheduled_for) WHERE ((deletion_scheduled_for IS NOT NULL) AND (anonymized_at IS NULL))
```

## 244. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX authz_decisions_occurred_idx ON zeroship.authz_decisions USING btree (occurred_at DESC)
```

## 245. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX authz_decisions_user_idx ON zeroship.authz_decisions USING btree (actor_user_id) WHERE (actor_user_id IS NOT NULL)
```

## 246. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX billing_disputes_invoice_idx ON zeroship.billing_disputes USING btree (invoice_id)
```

## 247. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX billing_metrics_owner_app_idx ON zeroship.billing_metrics USING btree (owner_app) WHERE (owner_app IS NOT NULL)
```

## 248. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX billing_reconciliation_findings_kind_idx ON zeroship.billing_reconciliation_findings USING btree (kind, detected_at)
```

## 249. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX billing_reconciliation_findings_open_idx ON zeroship.billing_reconciliation_findings USING btree (detected_at) WHERE (resolved_at IS NULL)
```

## 250. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX connect_checkout_failures_creator_idx ON zeroship.connect_checkout_failures USING btree (creator_id, created_at DESC)
```

## 251. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX credit_ledger_creator_created_idx ON zeroship.credit_ledger USING btree (creator_id, created_at)
```

## 252. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE UNIQUE INDEX credit_ledger_idempotency_key_idx ON zeroship.credit_ledger USING btree (idempotency_key) WHERE (idempotency_key IS NOT NULL)
```

## 253. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE UNIQUE INDEX credit_ledger_refund_clawback_note_idx ON zeroship.credit_ledger USING btree (note) WHERE ((kind)::text = 'refund_clawback'::text)
```

## 254. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE UNIQUE INDEX credit_ledger_refund_to_credit_note_idx ON zeroship.credit_ledger USING btree (note) WHERE ((kind)::text = 'refund_to_credit'::text)
```

## 255. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX device_grants_client_id_idx ON zeroship.device_grants USING btree (client_id) WHERE (client_id IS NOT NULL)
```

## 256. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX device_grants_expires_at_idx ON zeroship.device_grants USING btree (expires_at)
```

## 257. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX device_grants_provider_pending_user_code_idx ON zeroship.device_grants USING btree (provider, user_code) WHERE (status = 'pending'::text)
```

## 258. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX identity_links_principal_id_idx ON zeroship.identity_links USING btree (principal_id)
```

## 259. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_app_audit_app_at ON zeroship.app_audit USING btree (app_id, occurred_at DESC)
```

## 260. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_app_audit_creator_at ON zeroship.app_audit USING btree (creator_id, occurred_at DESC)
```

## 261. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_billing_notifications_pending ON zeroship.billing_notifications USING btree (claimed_at) WHERE ((status)::text = 'pending'::text)
```

## 262. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_creator_account_history_creator ON zeroship.creator_account_history USING btree (creator_id, linked_at DESC)
```

## 263. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE UNIQUE INDEX idx_creator_account_history_one_open ON zeroship.creator_account_history USING btree (creator_id) WHERE (unlinked_at IS NULL)
```

## 264. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_creator_billing_status_history_creator_at ON zeroship.creator_billing_status_history USING btree (creator_id, at DESC)
```

## 265. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_creator_billing_status_past_due ON zeroship.creator_billing_status USING btree (past_due_since) WHERE ((state)::text = 'past_due'::text)
```

## 266. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_deleted_sandboxes_deleted_at ON zeroship.deleted_sandboxes USING btree (deleted_at)
```

## 267. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_hosts_region_status ON zeroship.hosts USING btree (region, status)
```

## 268. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_hosts_status_heartbeat ON zeroship.hosts USING btree (status, last_heartbeat)
```

## 269. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_payouts_creator_time ON zeroship.payouts USING btree (creator_id, occurred_at DESC)
```

## 270. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_sandbox_events_metering ON ONLY zeroship.sandbox_events USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))
```

## 271. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_sandbox_events_sandbox_ts ON ONLY zeroship.sandbox_events USING btree (sandbox_id, ts)
```

## 272. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_sandbox_events_ts_brin ON ONLY zeroship.sandbox_events USING brin (ts) WITH (pages_per_range='32')
```

## 273. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_sandbox_events_user_id_ts ON ONLY zeroship.sandbox_events USING btree (user_id, ts)
```

## 274. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE UNIQUE INDEX idx_sandboxes_active_user_project ON zeroship.sandboxes USING btree (user_id, project_id) WHERE ((deleted_at IS NULL) AND (status = ANY (ARRAY['starting'::text, 'running'::text, 'recreating'::text])))
```

## 275. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_sandboxes_created_at ON zeroship.sandboxes USING btree (created_at)
```

## 276. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_sandboxes_host_id_status ON zeroship.sandboxes USING btree (host_id, status) WHERE (deleted_at IS NULL)
```

## 277. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_sandboxes_status_last_used ON zeroship.sandboxes USING btree (status, last_used_at) WHERE (deleted_at IS NULL)
```

## 278. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_sandboxes_user_id ON zeroship.sandboxes USING btree (user_id) WHERE (deleted_at IS NULL)
```

## 279. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_shares_expires_at ON zeroship.shares USING btree (expires_at) WHERE ((deleted_at IS NULL) AND (revoked_at IS NULL))
```

## 280. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_shares_iss_issued_at ON zeroship.shares USING btree (iss, issued_at) WHERE ((deleted_at IS NULL) AND (iss IS NOT NULL))
```

## 281. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_shares_sandbox_id_port ON zeroship.shares USING btree (sandbox_id, port) WHERE (deleted_at IS NULL)
```

## 282. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_spend_state_history_app_at ON zeroship.spend_state_history USING btree (app_id, at DESC)
```

## 283. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX idx_spend_state_history_period ON zeroship.spend_state_history USING btree (period)
```

## 284. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE UNIQUE INDEX invoice_payments_charge_provider_ref_key ON zeroship.invoice_payments USING btree (invoice_id, provider_ref) WHERE ((kind)::text = 'charge'::text)
```

## 285. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE UNIQUE INDEX invoice_payments_dispute_provider_ref_key ON zeroship.invoice_payments USING btree (invoice_id, provider_ref, kind) WHERE ((kind)::text = ANY (ARRAY['dispute_debit'::text, 'dispute_reversal'::text]))
```

## 286. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX invoice_payments_invoice_idx ON zeroship.invoice_payments USING btree (invoice_id)
```

## 287. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE UNIQUE INDEX invoices_active_period_claim ON zeroship.invoices USING btree (creator_id, period) WHERE ((status)::text <> 'void'::text)
```

## 288. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX migrated_app_policies_app_submitted_idx ON zeroship.migrated_app_policies USING btree (app_id, submitted_at DESC)
```

## 289. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX migrated_migration_audit_app_idx ON zeroship.migrated_migration_audit USING btree (app_id, migration_id, created_at)
```

## 290. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX migrated_migrations_app_status_idx ON zeroship.migrated_migrations USING btree (app_id, status, submitted_at DESC)
```

## 291. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX oauth_authorization_codes_expires_at_idx ON zeroship.oauth_authorization_codes USING btree (expires_at)
```

## 292. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX oauth_grants_client_idx ON zeroship.oauth_grants USING btree (client_id)
```

## 293. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX oauth_grants_user_granted_idx ON zeroship.oauth_grants USING btree (user_id, granted_at DESC)
```

## 294. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX oauth_refresh_tokens_expires_at_idx ON zeroship.oauth_refresh_tokens USING btree (expires_at)
```

## 295. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX oauth_refresh_tokens_family_idx ON zeroship.oauth_refresh_tokens USING btree (refresh_family_id, client_id, user_id)
```

## 296. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX oauth_refresh_tokens_idem_reap_idx ON zeroship.oauth_refresh_tokens USING btree (idem_expires_at) WHERE (idem_response_enc IS NOT NULL)
```

## 297. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE UNIQUE INDEX oauth_refresh_tokens_one_active_per_family ON zeroship.oauth_refresh_tokens USING btree (refresh_family_id) WHERE ((rotated_at IS NULL) AND (revoked_at IS NULL))
```

## 298. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX oauth_refresh_tokens_user_idx ON zeroship.oauth_refresh_tokens USING btree (user_id)
```

## 299. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX oidc_session_clients_client_idx ON zeroship.oidc_session_clients USING btree (client_id)
```

## 300. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX oidc_session_clients_user_idx ON zeroship.oidc_session_clients USING btree (user_id, idp_session_id)
```

## 301. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX payout_failures_creator_idx ON zeroship.payout_failures USING btree (creator_id, created_at DESC)
```

## 302. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX pending_disputes_charge_idx ON zeroship.pending_disputes USING btree (charge) WHERE (charge IS NOT NULL)
```

## 303. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX pending_disputes_payment_intent_idx ON zeroship.pending_disputes USING btree (payment_intent) WHERE (payment_intent IS NOT NULL)
```

## 304. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX permission_tokens_owner_active_idx ON zeroship.permission_tokens USING btree (owner_id) WHERE (revoked_at IS NULL)
```

## 305. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX permission_tokens_policies_gin_idx ON zeroship.permission_tokens USING gin (policies)
```

## 306. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX plan_change_events_app_period_idx ON zeroship.plan_change_events USING btree (app_id, period)
```

## 307. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX refunds_invoice_idx ON zeroship.refunds USING btree (invoice_id)
```

## 308. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_05_sandbox_id_ts_idx ON zeroship.sandbox_events_2026_05 USING btree (sandbox_id, ts)
```

## 309. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_05_ts_idx ON zeroship.sandbox_events_2026_05 USING brin (ts) WITH (pages_per_range='32')
```

## 310. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_05_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_05 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))
```

## 311. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_05_user_id_ts_idx ON zeroship.sandbox_events_2026_05 USING btree (user_id, ts)
```

## 312. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_06_sandbox_id_ts_idx ON zeroship.sandbox_events_2026_06 USING btree (sandbox_id, ts)
```

## 313. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_06_ts_idx ON zeroship.sandbox_events_2026_06 USING brin (ts) WITH (pages_per_range='32')
```

## 314. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_06_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_06 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))
```

## 315. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_06_user_id_ts_idx ON zeroship.sandbox_events_2026_06 USING btree (user_id, ts)
```

## 316. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_07_sandbox_id_ts_idx ON zeroship.sandbox_events_2026_07 USING btree (sandbox_id, ts)
```

## 317. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_07_ts_idx ON zeroship.sandbox_events_2026_07 USING brin (ts) WITH (pages_per_range='32')
```

## 318. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_07_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_07 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))
```

## 319. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_07_user_id_ts_idx ON zeroship.sandbox_events_2026_07 USING btree (user_id, ts)
```

## 320. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_08_sandbox_id_ts_idx ON zeroship.sandbox_events_2026_08 USING btree (sandbox_id, ts)
```

## 321. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_08_ts_idx ON zeroship.sandbox_events_2026_08 USING brin (ts) WITH (pages_per_range='32')
```

## 322. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_08_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_08 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))
```

## 323. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_08_user_id_ts_idx ON zeroship.sandbox_events_2026_08 USING btree (user_id, ts)
```

## 324. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_09_sandbox_id_ts_idx ON zeroship.sandbox_events_2026_09 USING btree (sandbox_id, ts)
```

## 325. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_09_ts_idx ON zeroship.sandbox_events_2026_09 USING brin (ts) WITH (pages_per_range='32')
```

## 326. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_09_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_09 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))
```

## 327. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_09_user_id_ts_idx ON zeroship.sandbox_events_2026_09 USING btree (user_id, ts)
```

## 328. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_10_sandbox_id_ts_idx ON zeroship.sandbox_events_2026_10 USING btree (sandbox_id, ts)
```

## 329. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_10_ts_idx ON zeroship.sandbox_events_2026_10 USING brin (ts) WITH (pages_per_range='32')
```

## 330. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_10_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_2026_10 USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))
```

## 331. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_2026_10_user_id_ts_idx ON zeroship.sandbox_events_2026_10 USING btree (user_id, ts)
```

## 332. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_default_sandbox_id_ts_idx ON zeroship.sandbox_events_default USING btree (sandbox_id, ts)
```

## 333. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_default_ts_idx ON zeroship.sandbox_events_default USING brin (ts) WITH (pages_per_range='32')
```

## 334. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_default_ts_sandbox_id_user_id_data_idx ON zeroship.sandbox_events_default USING btree (ts) INCLUDE (sandbox_id, user_id, data) WHERE (kind = ANY (ARRAY['compute_seconds'::text, 'share.used'::text, 'preview_egress'::text]))
```

## 335. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandbox_events_default_user_id_ts_idx ON zeroship.sandbox_events_default USING btree (user_id, ts)
```

## 336. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandboxes_idle_snapshot_idx ON zeroship.sandboxes USING btree (last_used_at) WHERE ((status = 'running'::text) AND idle_snapshot_opted_in)
```

## 337. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX sandboxes_status_lessee_idx ON zeroship.sandboxes USING btree (status, lessee_updated_at) WHERE (status = ANY (ARRAY['snapshotting'::text, 'restoring'::text, 'restoring_cold'::text]))
```

## 338. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX signing_keys_status_idx ON zeroship.signing_keys USING btree (status)
```

## 339. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX usage_aggregates_period_idx ON zeroship.usage_aggregates USING btree (period, app_id)
```

## 340. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX wake_jobs_lessee_idx ON zeroship.wake_jobs USING btree (lessee_updated_at) WHERE (state <> ALL (ARRAY['ok'::text, 'failed'::text]))
```

## 341. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX wake_jobs_sandbox_idx ON zeroship.wake_jobs USING btree (sandbox_id)
```

## 342. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE UNIQUE INDEX wake_jobs_sandbox_pending_uniq ON zeroship.wake_jobs USING btree (sandbox_id) WHERE (state <> ALL (ARRAY['ok'::text, 'failed'::text]))
```

## 343. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX wake_jobs_state_idx ON zeroship.wake_jobs USING btree (state) WHERE (state <> ALL (ARRAY['ok'::text, 'failed'::text]))
```

## 344. this index uses options the current index DSL cannot render exactly

TODO: add structural index support for sort direction, partial predicates, INCLUDE, ONLY/partitioned indexes, BRIN options, and predicate arrays as needed

```sql
CREATE INDEX wake_jobs_updated_at_idx ON zeroship.wake_jobs USING btree (updated_at)
```

## 345. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_env_expose
    ADD CONSTRAINT app_env_expose_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 346. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_members
    ADD CONSTRAINT app_members_added_by_fkey FOREIGN KEY (added_by) REFERENCES zeroship.users(id)
```

## 347. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_members
    ADD CONSTRAINT app_members_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 348. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_members
    ADD CONSTRAINT app_members_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 349. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_net_grants
    ADD CONSTRAINT app_net_grants_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 350. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_oauth_clients
    ADD CONSTRAINT app_oauth_clients_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 351. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_oauth_clients
    ADD CONSTRAINT app_oauth_clients_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE
```

## 352. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_scope_defs
    ADD CONSTRAINT app_scope_defs_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 353. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_secrets
    ADD CONSTRAINT app_secrets_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 354. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_session_anchors
    ADD CONSTRAINT app_session_anchors_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 355. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_session_anchors
    ADD CONSTRAINT app_session_anchors_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE
```

## 356. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_session_anchors
    ADD CONSTRAINT app_session_anchors_global_user_id_fkey FOREIGN KEY (global_user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 357. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_spend_limit
    ADD CONSTRAINT app_spend_limit_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 358. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_spend_state
    ADD CONSTRAINT app_spend_state_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 359. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_usage
    ADD CONSTRAINT app_usage_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 360. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_usage_history
    ADD CONSTRAINT app_usage_history_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 361. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_user_identities
    ADD CONSTRAINT app_user_identities_app_client_id_fkey FOREIGN KEY (app_client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE
```

## 362. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_user_identities
    ADD CONSTRAINT app_user_identities_global_user_id_fkey FOREIGN KEY (global_user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 363. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.app_vars
    ADD CONSTRAINT app_vars_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 364. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.apps
    ADD CONSTRAINT apps_plan_fk FOREIGN KEY (plan_id) REFERENCES zeroship.plans(id) ON DELETE RESTRICT
```

## 365. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.billing_customer_refs
    ADD CONSTRAINT billing_customer_refs_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE
```

## 366. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.billing_disputes
    ADD CONSTRAINT billing_disputes_invoice_id_fkey FOREIGN KEY (invoice_id) REFERENCES zeroship.invoices(id) ON DELETE RESTRICT
```

## 367. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.billing_line_provider_refs
    ADD CONSTRAINT billing_line_provider_refs_line_fk FOREIGN KEY (invoice_id, app_id, segment_no) REFERENCES zeroship.invoice_lines(invoice_id, app_id, segment_no) ON DELETE CASCADE
```

## 368. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.billing_metrics
    ADD CONSTRAINT billing_metrics_owner_app_fkey FOREIGN KEY (owner_app) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 369. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.billing_notifications
    ADD CONSTRAINT billing_notifications_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE
```

## 370. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.billing_provider_refs
    ADD CONSTRAINT billing_provider_refs_invoice_id_fkey FOREIGN KEY (invoice_id) REFERENCES zeroship.invoices(id) ON DELETE CASCADE
```

## 371. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.connect_checkout_failures
    ADD CONSTRAINT connect_checkout_failures_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_accounts(creator_id) ON DELETE CASCADE
```

## 372. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.creator_accounts
    ADD CONSTRAINT creator_accounts_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.users(id)
```

## 373. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.creator_billing
    ADD CONSTRAINT creator_billing_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 374. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.creator_billing_status
    ADD CONSTRAINT creator_billing_status_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE
```

## 375. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.creator_billing_status_history
    ADD CONSTRAINT creator_billing_status_history_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE
```

## 376. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.creator_fee_policy
    ADD CONSTRAINT creator_fee_policy_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 377. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.credit_ledger
    ADD CONSTRAINT credit_ledger_applied_invoice_id_fkey FOREIGN KEY (applied_invoice_id) REFERENCES zeroship.invoices(id) ON DELETE RESTRICT
```

## 378. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.credit_ledger
    ADD CONSTRAINT credit_ledger_consumed_from_grant_id_fkey FOREIGN KEY (consumed_from_grant_id) REFERENCES zeroship.credit_ledger(id) ON DELETE RESTRICT
```

## 379. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.credit_ledger
    ADD CONSTRAINT credit_ledger_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE
```

## 380. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.device_grants
    ADD CONSTRAINT device_grants_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE
```

## 381. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.device_grants
    ADD CONSTRAINT device_grants_principal_id_fkey FOREIGN KEY (principal_id) REFERENCES zeroship.users(id)
```

## 382. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.email_verifications
    ADD CONSTRAINT email_verifications_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 383. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.federated_identities
    ADD CONSTRAINT federated_identities_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 384. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.gateway_sessions
    ADD CONSTRAINT gateway_sessions_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 385. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.identity_links
    ADD CONSTRAINT identity_links_principal_id_fkey FOREIGN KEY (principal_id) REFERENCES zeroship.users(id)
```

## 386. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.idp_sessions
    ADD CONSTRAINT idp_sessions_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 387. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.invoice_lines
    ADD CONSTRAINT invoice_lines_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE RESTRICT
```

## 388. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.invoice_lines
    ADD CONSTRAINT invoice_lines_invoice_id_fkey FOREIGN KEY (invoice_id) REFERENCES zeroship.invoices(id) ON DELETE RESTRICT
```

## 389. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.invoice_lines
    ADD CONSTRAINT invoice_lines_plan_id_fkey FOREIGN KEY (plan_id) REFERENCES zeroship.plans(id) ON DELETE RESTRICT
```

## 390. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.invoice_payments
    ADD CONSTRAINT invoice_payments_invoice_id_fkey FOREIGN KEY (invoice_id) REFERENCES zeroship.invoices(id) ON DELETE RESTRICT
```

## 391. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.invoices
    ADD CONSTRAINT invoices_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE
```

## 392. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.magic_links
    ADD CONSTRAINT magic_links_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 393. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.metering_exports
    ADD CONSTRAINT metering_exports_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_billing(creator_id) ON DELETE CASCADE
```

## 394. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.metric_weights
    ADD CONSTRAINT metric_weights_metric_fkey FOREIGN KEY (metric) REFERENCES zeroship.billing_metrics(metric) ON DELETE RESTRICT
```

## 395. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.migrated_app_policies
    ADD CONSTRAINT migrated_app_policies_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 396. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.migrated_app_policies
    ADD CONSTRAINT migrated_app_policies_submitted_by_fkey FOREIGN KEY (submitted_by) REFERENCES zeroship.users(id) ON DELETE RESTRICT
```

## 397. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.migrated_migration_audit
    ADD CONSTRAINT migrated_migration_audit_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 398. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.migrated_migration_audit
    ADD CONSTRAINT migrated_migration_audit_principal_id_fkey FOREIGN KEY (principal_id) REFERENCES zeroship.users(id) ON DELETE RESTRICT
```

## 399. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.migrated_migrations
    ADD CONSTRAINT migrated_migrations_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 400. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.migrated_migrations
    ADD CONSTRAINT migrated_migrations_approved_by_fkey FOREIGN KEY (approved_by) REFERENCES zeroship.users(id) ON DELETE RESTRICT
```

## 401. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.migrated_migrations
    ADD CONSTRAINT migrated_migrations_submitted_by_fkey FOREIGN KEY (submitted_by) REFERENCES zeroship.users(id) ON DELETE RESTRICT
```

## 402. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.oauth_authorization_codes
    ADD CONSTRAINT oauth_authorization_codes_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE
```

## 403. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.oauth_authorization_codes
    ADD CONSTRAINT oauth_authorization_codes_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 404. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.oauth_clients
    ADD CONSTRAINT oauth_clients_created_by_fkey FOREIGN KEY (created_by) REFERENCES zeroship.users(id)
```

## 405. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.oauth_grants
    ADD CONSTRAINT oauth_grants_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE
```

## 406. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.oauth_grants
    ADD CONSTRAINT oauth_grants_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 407. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.oauth_refresh_tokens
    ADD CONSTRAINT oauth_refresh_tokens_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE
```

## 408. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.oauth_refresh_tokens
    ADD CONSTRAINT oauth_refresh_tokens_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 409. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.oidc_session_clients
    ADD CONSTRAINT oidc_session_clients_client_id_fkey FOREIGN KEY (client_id) REFERENCES zeroship.oauth_clients(client_id) ON DELETE CASCADE
```

## 410. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.oidc_session_clients
    ADD CONSTRAINT oidc_session_clients_idp_session_id_fkey FOREIGN KEY (idp_session_id) REFERENCES zeroship.idp_sessions(id) ON DELETE CASCADE
```

## 411. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.oidc_session_clients
    ADD CONSTRAINT oidc_session_clients_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 412. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.payout_failures
    ADD CONSTRAINT payout_failures_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_accounts(creator_id) ON DELETE CASCADE
```

## 413. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.payouts
    ADD CONSTRAINT payouts_creator_id_fkey FOREIGN KEY (creator_id) REFERENCES zeroship.creator_accounts(creator_id) ON DELETE RESTRICT
```

## 414. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.permission_tokens
    ADD CONSTRAINT permission_tokens_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 415. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.plan_change_events
    ADD CONSTRAINT plan_change_events_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 416. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.plan_change_events
    ADD CONSTRAINT plan_change_events_from_plan_id_fkey FOREIGN KEY (from_plan_id) REFERENCES zeroship.plans(id) ON DELETE RESTRICT
```

## 417. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.plan_change_events
    ADD CONSTRAINT plan_change_events_to_plan_id_fkey FOREIGN KEY (to_plan_id) REFERENCES zeroship.plans(id) ON DELETE RESTRICT
```

## 418. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.platform_admin_roles
    ADD CONSTRAINT platform_admin_roles_granted_by_fkey FOREIGN KEY (granted_by) REFERENCES zeroship.users(id)
```

## 419. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.platform_admin_roles
    ADD CONSTRAINT platform_admin_roles_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 420. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.platform_policies
    ADD CONSTRAINT platform_policies_updated_by_fkey FOREIGN KEY (updated_by) REFERENCES zeroship.users(id)
```

## 421. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.principal_grants
    ADD CONSTRAINT principal_grants_principal_id_fkey FOREIGN KEY (principal_id) REFERENCES zeroship.users(id)
```

## 422. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.refund_provider_refs
    ADD CONSTRAINT refund_provider_refs_refund_id_fkey FOREIGN KEY (refund_id) REFERENCES zeroship.refunds(id) ON DELETE CASCADE
```

## 423. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.refunds
    ADD CONSTRAINT refunds_invoice_id_fkey FOREIGN KEY (invoice_id) REFERENCES zeroship.invoices(id) ON DELETE RESTRICT
```

## 424. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.sandboxes
    ADD CONSTRAINT sandboxes_host_id_fkey FOREIGN KEY (host_id) REFERENCES zeroship.hosts(host_id) ON DELETE RESTRICT
```

## 425. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.shares
    ADD CONSTRAINT shares_sandbox_id_fkey FOREIGN KEY (sandbox_id) REFERENCES zeroship.sandboxes(sandbox_id) ON DELETE CASCADE
```

## 426. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.spend_state_history
    ADD CONSTRAINT spend_state_history_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 427. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.totp_backup_codes
    ADD CONSTRAINT totp_backup_codes_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 428. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.totp_credentials
    ADD CONSTRAINT totp_credentials_user_id_fkey FOREIGN KEY (user_id) REFERENCES zeroship.users(id) ON DELETE CASCADE
```

## 429. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.usage_aggregates
    ADD CONSTRAINT usage_aggregates_app_id_fkey FOREIGN KEY (app_id) REFERENCES zeroship.apps(id) ON DELETE CASCADE
```

## 430. this constraint shape is outside the current structural renderer

TODO: add structural support for this table constraint shape

```sql
ALTER TABLE ONLY zeroship.usage_aggregates
    ADD CONSTRAINT usage_aggregates_metric_fkey FOREIGN KEY (metric) REFERENCES zeroship.billing_metrics(metric) DEFERRABLE INITIALLY DEFERRED
```

## 431. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.sandbox_events_pkey ATTACH PARTITION zeroship.sandbox_events_2026_05_pkey
```

## 432. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_sandbox_ts ATTACH PARTITION zeroship.sandbox_events_2026_05_sandbox_id_ts_idx
```

## 433. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_ts_brin ATTACH PARTITION zeroship.sandbox_events_2026_05_ts_idx
```

## 434. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_metering ATTACH PARTITION zeroship.sandbox_events_2026_05_ts_sandbox_id_user_id_data_idx
```

## 435. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_user_id_ts ATTACH PARTITION zeroship.sandbox_events_2026_05_user_id_ts_idx
```

## 436. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.sandbox_events_pkey ATTACH PARTITION zeroship.sandbox_events_2026_06_pkey
```

## 437. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_sandbox_ts ATTACH PARTITION zeroship.sandbox_events_2026_06_sandbox_id_ts_idx
```

## 438. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_ts_brin ATTACH PARTITION zeroship.sandbox_events_2026_06_ts_idx
```

## 439. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_metering ATTACH PARTITION zeroship.sandbox_events_2026_06_ts_sandbox_id_user_id_data_idx
```

## 440. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_user_id_ts ATTACH PARTITION zeroship.sandbox_events_2026_06_user_id_ts_idx
```

## 441. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.sandbox_events_pkey ATTACH PARTITION zeroship.sandbox_events_2026_07_pkey
```

## 442. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_sandbox_ts ATTACH PARTITION zeroship.sandbox_events_2026_07_sandbox_id_ts_idx
```

## 443. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_ts_brin ATTACH PARTITION zeroship.sandbox_events_2026_07_ts_idx
```

## 444. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_metering ATTACH PARTITION zeroship.sandbox_events_2026_07_ts_sandbox_id_user_id_data_idx
```

## 445. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_user_id_ts ATTACH PARTITION zeroship.sandbox_events_2026_07_user_id_ts_idx
```

## 446. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.sandbox_events_pkey ATTACH PARTITION zeroship.sandbox_events_2026_08_pkey
```

## 447. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_sandbox_ts ATTACH PARTITION zeroship.sandbox_events_2026_08_sandbox_id_ts_idx
```

## 448. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_ts_brin ATTACH PARTITION zeroship.sandbox_events_2026_08_ts_idx
```

## 449. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_metering ATTACH PARTITION zeroship.sandbox_events_2026_08_ts_sandbox_id_user_id_data_idx
```

## 450. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_user_id_ts ATTACH PARTITION zeroship.sandbox_events_2026_08_user_id_ts_idx
```

## 451. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.sandbox_events_pkey ATTACH PARTITION zeroship.sandbox_events_2026_09_pkey
```

## 452. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_sandbox_ts ATTACH PARTITION zeroship.sandbox_events_2026_09_sandbox_id_ts_idx
```

## 453. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_ts_brin ATTACH PARTITION zeroship.sandbox_events_2026_09_ts_idx
```

## 454. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_metering ATTACH PARTITION zeroship.sandbox_events_2026_09_ts_sandbox_id_user_id_data_idx
```

## 455. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_user_id_ts ATTACH PARTITION zeroship.sandbox_events_2026_09_user_id_ts_idx
```

## 456. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.sandbox_events_pkey ATTACH PARTITION zeroship.sandbox_events_2026_10_pkey
```

## 457. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_sandbox_ts ATTACH PARTITION zeroship.sandbox_events_2026_10_sandbox_id_ts_idx
```

## 458. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_ts_brin ATTACH PARTITION zeroship.sandbox_events_2026_10_ts_idx
```

## 459. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_metering ATTACH PARTITION zeroship.sandbox_events_2026_10_ts_sandbox_id_user_id_data_idx
```

## 460. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_user_id_ts ATTACH PARTITION zeroship.sandbox_events_2026_10_user_id_ts_idx
```

## 461. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.sandbox_events_pkey ATTACH PARTITION zeroship.sandbox_events_default_pkey
```

## 462. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_sandbox_ts ATTACH PARTITION zeroship.sandbox_events_default_sandbox_id_ts_idx
```

## 463. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_ts_brin ATTACH PARTITION zeroship.sandbox_events_default_ts_idx
```

## 464. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_metering ATTACH PARTITION zeroship.sandbox_events_default_ts_sandbox_id_user_id_data_idx
```

## 465. this platform DDL object has no exact structural operation in the current v2 surface

TODO: add a structural operation for this exact platform DDL fragment

```sql
ALTER INDEX zeroship.idx_sandbox_events_user_id_ts ATTACH PARTITION zeroship.sandbox_events_default_user_id_ts_idx
```

## 466. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER app_audit_block_delete BEFORE DELETE ON zeroship.app_audit FOR EACH ROW EXECUTE FUNCTION zeroship.app_audit_block_tamper()
```

## 467. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER app_audit_block_truncate BEFORE TRUNCATE ON zeroship.app_audit FOR EACH STATEMENT EXECUTE FUNCTION zeroship.app_audit_block_tamper()
```

## 468. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER app_audit_block_update BEFORE UPDATE ON zeroship.app_audit FOR EACH ROW EXECUTE FUNCTION zeroship.app_audit_block_tamper()
```

## 469. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER app_oauth_clients_sector_identifier_immutable BEFORE UPDATE OF sector_identifier ON zeroship.app_oauth_clients FOR EACH ROW EXECUTE FUNCTION zeroship.app_oauth_clients_reject_sector_change()
```

## 470. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER audit_events_block_delete BEFORE DELETE ON zeroship.audit_events FOR EACH ROW EXECUTE FUNCTION zeroship.audit_events_block_tamper()
```

## 471. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER audit_events_block_truncate BEFORE TRUNCATE ON zeroship.audit_events FOR EACH STATEMENT EXECUTE FUNCTION zeroship.audit_events_block_tamper()
```

## 472. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER audit_events_block_update BEFORE UPDATE ON zeroship.audit_events FOR EACH ROW EXECUTE FUNCTION zeroship.audit_events_block_tamper()
```

## 473. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER authz_decisions_block_delete BEFORE DELETE ON zeroship.authz_decisions FOR EACH ROW EXECUTE FUNCTION zeroship.authz_decisions_block_tamper()
```

## 474. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER authz_decisions_block_truncate BEFORE TRUNCATE ON zeroship.authz_decisions FOR EACH STATEMENT EXECUTE FUNCTION zeroship.authz_decisions_block_tamper()
```

## 475. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER authz_decisions_block_update BEFORE UPDATE ON zeroship.authz_decisions FOR EACH ROW EXECUTE FUNCTION zeroship.authz_decisions_block_tamper()
```

## 476. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER billing_disputes_controlled_update_trg BEFORE DELETE OR UPDATE ON zeroship.billing_disputes FOR EACH ROW EXECUTE FUNCTION zeroship.billing_disputes_controlled_update()
```

## 477. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER billing_reconciliation_findings_controlled_update_trg BEFORE DELETE OR UPDATE ON zeroship.billing_reconciliation_findings FOR EACH ROW EXECUTE FUNCTION zeroship.billing_reconciliation_findings_controlled_update()
```

## 478. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER connect_checkout_failures_immutable_trg BEFORE DELETE OR UPDATE ON zeroship.connect_checkout_failures FOR EACH ROW EXECUTE FUNCTION zeroship.connect_checkout_failures_immutable()
```

## 479. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER credit_ledger_immutable_trg BEFORE DELETE OR UPDATE ON zeroship.credit_ledger FOR EACH ROW EXECUTE FUNCTION zeroship.credit_ledger_immutable()
```

## 480. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER invoice_lines_immutable_trg BEFORE INSERT OR DELETE OR UPDATE ON zeroship.invoice_lines FOR EACH ROW EXECUTE FUNCTION zeroship.invoice_lines_immutable()
```

## 481. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER invoice_payments_immutable_trg BEFORE DELETE OR UPDATE ON zeroship.invoice_payments FOR EACH ROW EXECUTE FUNCTION zeroship.invoice_payments_immutable()
```

## 482. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER invoices_immutable_trg BEFORE DELETE OR UPDATE ON zeroship.invoices FOR EACH ROW EXECUTE FUNCTION zeroship.invoices_immutable()
```

## 483. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER migrated_migration_audit_append_only BEFORE DELETE OR UPDATE ON zeroship.migrated_migration_audit FOR EACH ROW EXECUTE FUNCTION zeroship.reject_migrated_migration_audit_mutation()
```

## 484. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER payout_failures_immutable_trg BEFORE DELETE OR UPDATE ON zeroship.payout_failures FOR EACH ROW EXECUTE FUNCTION zeroship.payout_failures_immutable()
```

## 485. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER plan_change_events_immutable_trg BEFORE DELETE OR UPDATE ON zeroship.plan_change_events FOR EACH ROW EXECUTE FUNCTION zeroship.plan_change_events_immutable()
```

## 486. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER refunds_immutable_trg BEFORE DELETE OR UPDATE ON zeroship.refunds FOR EACH ROW EXECUTE FUNCTION zeroship.refunds_immutable()
```

## 487. createTrigger targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createTrigger to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE TRIGGER refunds_no_over_refund_trg BEFORE INSERT ON zeroship.refunds FOR EACH ROW EXECUTE FUNCTION zeroship.refunds_no_over_refund()
```

## 488. table and column comments target raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow comment() to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
COMMENT ON COLUMN zeroship.app_oauth_clients.sector_identifier IS 'Immutable after insert: refresh-token revocation markers persist the derived pairwise subject, so changing the sector would de-align stored family-kill markers from live access-token subjects.'
```

## 489. table and column comments target raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow comment() to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
COMMENT ON TABLE zeroship.migrated_app_policies IS 'Creator migration policy versions. App isolation is enforced in the migrated service by authorization plus app_id-scoped queries; no table RLS is installed because the service role can hold BYPASSRLS.'
```

## 490. table and column comments target raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow comment() to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
COMMENT ON TABLE zeroship.migrated_migration_audit IS 'Append-only audit history for creator migration submit, approval, pending rejection, and apply outcomes.'
```

## 491. table and column comments target raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow comment() to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
COMMENT ON TABLE zeroship.migrated_migrations IS 'Creator migration workflow rows. App isolation is enforced by migrated service authorization plus app_id-scoped primary-key lookups.'
```

## 492. table and column comments target raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow comment() to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
COMMENT ON COLUMN zeroship.oauth_refresh_tokens.sub IS 'Pairwise subject snapshot persisted for refresh-family kill markers. app_oauth_clients.sector_identifier is immutable after insert so this snapshot cannot diverge from newly minted access-token subjects.'
```

## 493. enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE zeroship.app_secrets ENABLE ROW LEVEL SECURITY
```

## 494. enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE zeroship.app_session_anchors ENABLE ROW LEVEL SECURITY
```

## 495. enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE zeroship.app_spend_limit ENABLE ROW LEVEL SECURITY
```

## 496. enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE zeroship.app_spend_state ENABLE ROW LEVEL SECURITY
```

## 497. enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE zeroship.app_user_identities ENABLE ROW LEVEL SECURITY
```

## 498. enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE zeroship.gateway_sessions ENABLE ROW LEVEL SECURITY
```

## 499. enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE zeroship.plan_change_events ENABLE ROW LEVEL SECURITY
```

## 500. enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE zeroship.spend_state_history ENABLE ROW LEVEL SECURITY
```

## 501. enableRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow enableRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE zeroship.usage_aggregates ENABLE ROW LEVEL SECURITY
```

## 502. forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE ONLY zeroship.app_secrets FORCE ROW LEVEL SECURITY
```

## 503. forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE ONLY zeroship.app_session_anchors FORCE ROW LEVEL SECURITY
```

## 504. forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE ONLY zeroship.app_spend_limit FORCE ROW LEVEL SECURITY
```

## 505. forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE ONLY zeroship.app_spend_state FORCE ROW LEVEL SECURITY
```

## 506. forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE ONLY zeroship.app_user_identities FORCE ROW LEVEL SECURITY
```

## 507. forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE ONLY zeroship.gateway_sessions FORCE ROW LEVEL SECURITY
```

## 508. forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE ONLY zeroship.plan_change_events FORCE ROW LEVEL SECURITY
```

## 509. forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE ONLY zeroship.spend_state_history FORCE ROW LEVEL SECURITY
```

## 510. forceRowLevelSecurity targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow forceRowLevelSecurity to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
ALTER TABLE ONLY zeroship.usage_aggregates FORCE ROW LEVEL SECURITY
```

## 511. createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE POLICY tenant_isolation ON zeroship.app_secrets USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid)) WITH CHECK ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))
```

## 512. createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE POLICY tenant_isolation ON zeroship.app_session_anchors USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid)) WITH CHECK ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))
```

## 513. createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE POLICY tenant_isolation ON zeroship.app_spend_limit USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid)) WITH CHECK ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))
```

## 514. createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE POLICY tenant_isolation ON zeroship.app_spend_state USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))
```

## 515. createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE POLICY tenant_isolation ON zeroship.app_user_identities USING ((app_client_id = current_setting('zeroship.tenant_client'::text, true))) WITH CHECK ((app_client_id = current_setting('zeroship.tenant_client'::text, true)))
```

## 516. createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE POLICY tenant_isolation ON zeroship.gateway_sessions USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid)) WITH CHECK ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))
```

## 517. createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE POLICY tenant_isolation ON zeroship.plan_change_events USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))
```

## 518. createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE POLICY tenant_isolation ON zeroship.spend_state_history USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))
```

## 519. createPolicy targets raw-created platform tables that are not registered in the v2 ownership registry

TODO: allow createPolicy to target trusted platform tables created outside table().create(), or add exact platform table registration

```sql
CREATE POLICY tenant_isolation ON zeroship.usage_aggregates USING ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid)) WITH CHECK ((app_id = (current_setting('zeroship.tenant_app'::text, true))::uuid))
```
