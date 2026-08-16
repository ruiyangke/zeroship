# Auth suite diagnosis scratch notes

Baseline command, started from clean `e62b60dc7`:

```text
bash tests/run_auth_suite.sh
```

Outer log: `/tmp/s16-auth-suite-red.log`.

## Verbatim auth red

```text
[ERROR zeroship_auth::oidc::backchannel_logout] BCL logout_token signing failed error=internal: signing key lVndtD3yV9EaLOyKwb_CrLI3vGoPSQP2ojnOIOjCnW0 is no longer trusted for issuance client_id=oac_bcl_emit_ccb284e66fb5486b901addcbdc2f3120

thread 'logout_emission_posts_signed_logout_token_with_sid' (2105150) panicked at crates/auth/tests/oidc_backchannel_logout_test.rs:98:5:
assertion `left == right` failed
  left: 0
 right: 1

failures:
    logout_emission_posts_signed_logout_token_with_sid

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.93s
```

This test never enters Gateway identity projection. Its fixture constructs an
`Issuer` at `crates/auth/tests/oidc_backchannel_logout_test.rs:49` but does not
publish that key. Production publishes it at `crates/auth/src/main.rs:248`.
`Issuer::issue_logout_token` reserves the token expiry through the active key
registry, so the unpublished fixture key is correctly refused.

## Verbatim Gateway red

Each of the eight failures logged this production guard error (the first used
`oac_bcl_refresh`; the remaining seven used `oac_myapp`):

```text
[ERROR zeroship_gateway::auth_token] app_user_identities upsert failed on cookie mint error=database: app_user_identities pairwise binding changed app_client_id=oac_myapp
thread 'token_exchange_swaps_email_for_relay_alias' (2128900) panicked at crates/gateway/tests/auth_token_anchors_test.rs:1477:5:
assertion `left == right` failed: code exchange must succeed
  left: 500
 right: 200
```

The binary's verbatim failure list and summary were:

```text
failures:
    backchannel_logout_revokes_refreshed_session_with_sid_logout_token
    mint_racing_concurrent_reset_fails_closed_no_fresh_cookie
    session_mint_invalid_grant_deletes_anchor_and_requires_login
    session_mint_persists_rotated_refresh_token_for_next_rotation
    session_mint_recovers_after_reload_one_refresh
    session_minted_cookie_verifies_locally_bound_to_route_client
    session_steady_state_reads_gateway_session_without_op
    token_exchange_swaps_email_for_relay_alias

test result: FAILED. 15 passed; 8 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.38s
```

The complete runner ended with:

```text
FAIL: only 451 auth tests passed, fewer than the 505 this gate expects.
AUTH SUITE: FAILED
```

The runner's 451 aggregate excludes every passing test in a failed test
binary, including the 15 anchor passes. There were exactly nine failed tests.
The separate auth failure was not an HTTP 500 and did not enter the Gateway
identity upsert, so the original claim that all nine share that path is false.
