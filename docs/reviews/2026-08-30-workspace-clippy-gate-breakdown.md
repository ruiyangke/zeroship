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
