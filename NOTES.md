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

The baseline suite was still running when this evidence was committed. The
Gateway red block and final aggregate belong in the next notes commit.
