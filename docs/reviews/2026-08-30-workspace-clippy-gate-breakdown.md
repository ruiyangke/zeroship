# What is actually keeping the workspace clippy gate red

Measured 2026-08-30 at `7861a7f5d`. Not compio-postgres work - recorded because
the number gets quoted as one undifferentiated wall of errors, and it is not.

    cargo clippy -p zeroship-migrate-core -p zeroship-migrate-mysql \
                 -p zeroship-migrate-postgres -p zeroship-migrate-sqlite \
                 --all-features --all-targets

exits 101 with **284 errors**. Treat 284 as a FLOOR, not a total: a deny-level
error aborts cargo's scheduling, so crates downstream of the first failure are
never linted and contribute nothing to the count.

| error text                                          | count | nature |
| --------------------------------------------------- | ----: | ------ |
| `doc list item without indentation`                   |   137 | whitespace inside `//!` and `///` blocks |
| `` the `Err`-variant returned from this function `` |   101 | `result_large_err` |
| `` ...returned from this closure is very large ``    |    15 | `result_large_err` |
| `too many arguments` (8..13 vs 7)                     |    25 | signature shape |
| remainder (casts, option, needless)                   |    ~6 | mixed |

**48% of it is doc-comment whitespace.** That half is mechanical, carries no
behavioural risk, and is separable: a change touching only `//!`/`///` lines can
be proved safe with `git diff --numstat` plus a check that every changed line
begins with a doc-comment marker.

**The other half is a design change, not a cleanup.** `result_large_err` wants
the error type boxed, which rewrites `Result<_, E>` signatures across four
crates and every caller. Doing it by sprinkling `#[allow]` would leave the gate
green while the oversized `Err` variants stay, which is worse than the current
honest red.

**These crates are OURS.** `crates/zeroship-migrate-*` are tracked in this repo
(`git ls-files` lists their `Cargo.toml`); they are NOT the vendored engine,
which lives at `third_party/zero-migrate/` and has no such path. The "do not
touch zero-migrate" rule does not cover them, and confusing the two is easy from
the crate names alone.

**Why this matters for the postgres gate.** `cargo clippy -p compio-postgres
--all-features --all-targets` exits 0 today. The workspace gate's red is
entirely upstream of that crate, so a postgres change cannot be judged by the
workspace gate - run the per-crate one, and read a workspace red as "the migrate
crates are still red", not as a regression.

## The doc gate is red the same way, and it refuses rather than lying

Measured 2026-08-30 at `00e31cf10`. `./tests/run_doc_gate.sh` exits 1:

    error: could not document `zeroship-migrate-adapter`
    error: could not compile `zeroship-migrate-adapter` (lib) due to 22 previous errors
    error: could not document `zeroship-config-contract`

**`compio-postgres` is not implicated.** `cargo doc -p compio-postgres --no-deps
--all-features` exits 0 with zero errors and zero unresolved links. And the
twenty commits of this session touched exactly four directories -
`docs/reviews`, `docs/runbooks`, `libs/compio-postgres/src`,
`libs/compio-postgres/tests/suite` - with nothing outside them, so they cannot
have broken another crate. Check that with

    git log <base>..HEAD --name-only --pretty=format: | sort -u \
      | grep -vE '^(libs/compio-postgres/|docs/)'

rather than asserting it.

### What this gate does RIGHT, and why it is worth copying

Its two arms report a census against a floor:

    arm=doc_default_features examined=37 floor=38
    arm=doc_all_features     examined=28 floor=38

Both came in UNDER the floor - ten crates short under `--all-features` - so the
gate refused. It did not report "no doc errors found". That distinction is the
whole point: a deny-level compile error aborts cargo's scheduling, so every
crate downstream of the failure is never documented and contributes no errors.
Without the floor, a run that examined 28 of 38 crates and found nothing wrong
in those 28 prints exactly what a clean workspace prints.

The clippy gate above has the same guard for the same reason, which is why its
284 is a FLOOR and not a total. When adding a gate here, make it count what it
ruled on and compare that against cargo's own inventory - "found no problems"
and "never looked" are indistinguishable otherwise.
