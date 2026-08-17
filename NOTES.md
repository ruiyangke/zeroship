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

## Measured subject mismatch and origins

Focused command after adding temporary diagnostics:

```text
GATEWAY_ANCHORS_DB_URL=postgres://postgres:zeroship@127.0.0.1:5440/zeroship_auth_test AUTH_DB_URL=postgres://postgres:zeroship@127.0.0.1:5440/zeroship_auth_test cargo test -p zeroship-gateway --test auth_token_anchors_test token_exchange_swaps_email_for_relay_alias -- --exact --test-threads 1 --nocapture
```

Verbatim values from `/tmp/s16-pairwise-instrumented-red.log`:

```text
pairwise instrumentation: stored_pairwise_sub=pws_seed_8ed3d71548e04205832647f488c5a77e origin=auth_token_anchors_test::seed_relay_alias_for client_id=oac_myapp global_user_id=8ed3d715-48e0-4205-8326-47f488c5a77e
[ERROR zeroship_gateway::identities] app_user_identities immutable pairwise binding mismatch app_client_id=oac_myapp global_user_id=8ed3d715-48e0-4205-8326-47f488c5a77e stored_pairwise_sub=Some("pws_seed_8ed3d71548e04205832647f488c5a77e") recomputed_pairwise_sub=pws_6LttJUCDnqZy1AhlkD9k recomputed_origin="gateway caller supplied pairwise projection"
```

The stored value is fabricated by
`crates/gateway/tests/auth_token_anchors_test.rs:1250`, where the relay fixture
formats `pws_seed_` plus the UUID. Every one of the eight failing Gateway tests
calls this helper. The requested value is the real HMAC projection computed by
`pairwise_sub` at `crates/gateway/src/auth_token.rs:132-138`, invoked for the
cookie mint at line 541 and passed to the immutable upsert at line 659. The
guard at `crates/gateway/src/identities.rs:72` correctly refuses the rebind.

Queued finding 9 is stale. Auth still derives the access-token subject at
`crates/auth/src/oidc/issuer.rs:421` through `pairwise_subject` at lines
849-850. Current Gateway checks that subject's pairwise shape and copies it at
`crates/gateway/src/router/auth.rs:625-629`, then emits it unchanged at lines
681-692. The old `project_pairwise` helper no longer exists. Commit
`290c85e0a` removed the double projection before this baseline. The cited Auth
line range and both Gateway ranges have rotted; the core non-UUID behavior is
still present at `crates/core/src/auth/mod.rs:302-318` but is not reached here.

## Focused real-OP setup red

The first focused run of the strengthened `oidc_rp_e2e` reused the completed
suite database. It stopped before the new projection assertion because the
suite had already retired that test's fixed signing key:

```text
thread 'gateway_bearer_rejects_real_op_id_token_but_accepts_access_token' (2167098) panicked at crates/gateway/tests/oidc_rp_e2e.rs:658:10:
publish active OP key: Config("signing key RdsIdO3CsMDzCjNZvzh9oqMmTgMASg3jgoAi8dXZLIQ has non-activatable status \"retiring\"")
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 2 filtered out; finished in 0.24s
```

This is cross-command contamination caused by intentionally reusing the suite
database after the baseline finished. It is not one of the nine suite failures
and says nothing about the projection assertion. The rerun must use a separate
freshly migrated local database.

## Full-suite signing fixture interaction

After the pairwise fixture fix and the first logout fixture fix, the full suite
made all 23 Gateway anchor tests pass and made the logout emission test pass.
It then failed all 17 `oidc_login_consent_test` cases because the logout test
had published its private `[77u8; 32]` signing key, retiring the suite's
canonical `[42u8; 32]` key:

```text
publish active OP key: Config("signing key RdsIdO3CsMDzCjNZvzh9oqMmTgMASg3jgoAi8dXZLIQ has non-activatable status \"retiring\"")
test result: FAILED. 0 passed; 17 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.68s
FAIL: only 488 auth tests passed, fewer than the 505 this gate expects.
AUTH SUITE: FAILED
```

This is an intra-run fixture conflict. `oidc_backchannel_logout_test.rs:40`
uses seed 77 and now publishes it at lines 49-53. The following consent suite
uses seed 42 at `oidc_login_consent_test.rs:913` and publishes it at lines
66-69. Publishing seed 77 correctly retires the previously active seed 42,
so the later fixture correctly refuses to reactivate a retiring key.

## The consent failure predates this branch: cargo fail-fast hid it

Switching the logout fixture to seed 42 did NOT remove the consent failure.
The suite still ends:

```text
test result: FAILED. 0 passed; 17 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.26s
error: test failed, to rerun pass `-p zeroship-auth --test oidc_login_consent_test`
...
FAIL: only 488 auth tests passed, fewer than the 505 this gate expects.
```

with the same message the seed-77 attempt produced:

```text
publish active OP key: Config("signing key RdsIdO3CsMDzCjNZvzh9oqMmTgMASg3jgoAi8dXZLIQ has non-activatable status \"retiring\"")
```

so the earlier attribution of this failure to the logout fixture was wrong.
`cargo test` stops at the first failing test target. On main the auth crate
aborted at `oidc_backchannel_logout_test`, which sorts before
`oidc_brokered_login_test`, `oidc_foundation_test` and
`oidc_login_consent_test` - so those three binaries, and everything after them
in the crate, NEVER RAN on main. Main's "nine failures" is nine failures
*before truncation*, not the whole picture. Fixing the logout fixture moved the
truncation point later and uncovered the next latent breakage.

## Duplicate signing-key seeds across test binaries

`Issuer::publish_active_key` retires every other active row
(`crates/auth/src/oidc/issuer.rs:397-408`) and refuses to reactivate a
`retiring` row (`issuer.rs:388-391`). The kid is a pure thumbprint of the
public key (`issuer.rs:273`), so two test binaries built from the same seed
publish the SAME kid into the one shared suite database. Ten binaries in
`crates/auth/tests/` publish, and TWO seeds are shared - 42 by three of them and
43 by two - not "three seeds used twice", which this paragraph said until it was
re-measured against the block directly below it that contradicts it:

```text
seed 42  oidc_authorization_code_test:503, oidc_login_consent_test:913, oidc_userinfo_test:338
seed 43  oidc_brokered_login_test:406, oidc_refresh_token_test:1174
```

Measured on main with
`git grep -n SigningKey::from_bytes main -- crates/auth/tests` cross-referenced
against `git grep -ln publish_active_key main -- crates/auth/tests`. The second
list is what bounds the population: `consent_ui_test.rs:339` also builds an
issuer from seed 42 but never publishes it, so it is not one of the ten and not
part of the collision. `signing_key_retention_test` publishes from a random
secret, not a fixed seed. The remaining eight publishers hold seeds 21, 23, 51,
57, 77 and 91, each used once.

Run order is alphabetical, so `oidc_authorization_code_test` publishes kid(42),
`oidc_brokered_login_test` publishes kid(43) and retires kid(42),
`oidc_foundation_test` publishes kid(21) and retires kid(43), and then
`oidc_login_consent_test` tries to reactivate the now-`retiring` kid(42) and
fails closed. `oidc_refresh_token_test` and `oidc_userinfo_test` sit behind the
same wall, unreached only because the run is truncated first.

The production behaviour is correct: a retiring signer must not reactivate.
The fixtures are wrong to share one OP identity across binaries that each act
as their own OP.

## A third fabricated pairwise subject, on the Auth side

Giving every binary its own key took `oidc_login_consent_test` from 0/17 to
16/17 and exposed the last one:

```text
[ERROR zeroship_auth::oidc::authorization_code] token: pairwise identity binding changed client_id=oac_3VEiRlBnyOZxu1HOW3JVfW user_id=8175a0ae-40fd-4909-9fc0-90983b6e34f3
thread 'end_to_end_native_authorize_login_consent_token_flow' panicked at crates/auth/tests/oidc_login_consent_test.rs:1082:5:
assertion `left == right` failed: token status
  left: 500
 right: 200
```

Same defect as the Gateway relay fixture, in Auth's half of the same
invariant. `crates/auth/src/oidc/authorization_code.rs:1431-1464` recomputes
the pairwise subject and refuses to mint when the stored row disagrees, and
`seed_user_client` was inserting `pws_test_<uuid>`. The two neighbouring
fixtures that seed the same table already do it correctly - both call
`Issuer::pairwise_subject` (`oidc_userinfo_test.rs:421`,
`oidc_refresh_token_test.rs:1248`) - so the fix is to match them, not to
invent a fourth spelling.

Three fixtures, one production invariant, three different invented `pws_`
formats: `pws_seed_`, `pws_test_`, and an unpublished signing key. Each failed
closed exactly as designed.

## Green

```text
==> skip census: 1 test(s) announced they did nothing (1 allowlisted, 0 not)
    test smtp_plaintext_sink_delivers_relay_forward ... ZEROSHIP-TEST-SKIPPED: skip (need AUTH_DB_URL + AUTH_TEST_SMTP_SINK=host:port)
AUTH SUITE: 636 tests passed, 0 unexpected skips, 1 allowlisted (floor 505)
```

Zero `test result: FAILED` lines in the whole run. 451 to 636 is far more than
the nine failures repaired: THIRTY Auth binaries that main's fail-fast never
reached now run, contributing 161 of the 185.

Thirty, not the six this paragraph claimed until it was re-measured. The method:
take the completed run's log, reduce it to one `<binary> <passed>` line per
target in the order cargo ran them, cut at the last `zeroship-auth` target, and
sum everything after `oidc_backchannel_logout_test` - the binary that failed on
main and therefore ended the whole `cargo test -p zeroship-auth` invocation:

```text
awk '/Running (unittests|tests\/)/ { bin=$2; if (bin=="unittests") bin=$3; next }
     /^test result:/ { if (bin!="") { print bin, $4; bin="" } }' suite.log \
  | awk 'NR<=54' \
  | awk 'f{n++; s+=$2} /oidc_backchannel_logout_test/{f=1} END{print n, s}'
30 161
```

The conclusion the six was offered for is unchanged and if anything understated:
truncation, not the nine repaired failures, is where the delta came from. But
the count was wrong by 5x, and a wrong number in an audit document outlives the
argument it was supporting.

No production code changed on this branch. Every edit is a test fixture, this
file, or the audit document. The regression coverage is the suite itself plus
the two projected-identity assertions added earlier
(`crates/gateway/tests/oidc_rp_e2e.rs`,
`crates/gateway/tests/auth_token_anchors_test.rs:1500`), which check the
projected `pws_` rather than an HTTP status.

Remaining gates, all at their stated baselines:

```text
cargo test -p zeroship-gateway       491 across 11 suites (414+19+23+11+10+1+2+3+7+1+0)
cargo test -p zeroship-auth --lib    220 passed
cargo test -p zeroship-control --lib 224 passed
tests/commit_msg_gate.sh --range main..HEAD   checked 20 commits, 0 rejected
```

`oidc_rp_e2e` needs care: under a plain `cargo test -p zeroship-gateway` it
reports "3 passed ... in 0.00s", which is three SELF-SKIPS, so the new
projected-identity assertion never executes there. Given AUTH_DB_URL and
CONTROL_TEST_DB against a freshly migrated database it runs for real:

```text
test gateway_bearer_rejects_real_op_id_token_but_accepts_access_token ...
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.81s
```

3.81s against 0.00s is the whole difference between the assertion holding and
the assertion being absent.

**The reason first given here for no gate running it was wrong, and is
retracted.** This section said `run_auth_suite.sh` excluded the binary by name
"because it also wants CONTROL_TEST_DB, which the script does not provision".
That repeated the runner's own comment without checking it against the code.
`db_url()` at `crates/gateway/tests/oidc_rp_e2e.rs:47` is
`test_env!("AUTH_DB_URL").or_else(|| test_env!("CONTROL_TEST_DB"))`: EITHER
variable satisfies it, and `tests/run_auth_suite.sh:69` exports `AUTH_DB_URL`.
The target could have been in the gate the whole time.

The measurement that settles it needs the control, because setting both
variables at once cannot say which one did the work. With `CONTROL_TEST_DB`
explicitly unset and `AUTH_DB_URL` alone: `3 passed ... finished in 6.73s`.
With neither: `3 passed ... finished in 0.00s`. One variable changed, opposite
outcomes.

`tests/run_auth_suite.sh` now runs `zeroship-gateway:oidc_rp_e2e` in its
gated-binary loop, with no `SKIP_ALLOWLIST` entry - the file announces through
`zeroship_test_support::skip`, so losing the database makes the census count it
and the gate go red rather than report a silent pass.
