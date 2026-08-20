# zeroship-testkit

The test harness's own logic, in Rust rather than shell. `compio-postgres`
replaces `psql`.

## Why

`tests/` is 110 shell files and 41,426 lines. The part of it that reasons about
databases reached PostgreSQL by shelling out to `psql` and reading stdout back
as a string: 132 `INSERT INTO`, 39 `CREATE DATABASE`, 34 `DROP DATABASE`, 18
`SELECT COUNT`, 5 `ALTER DATABASE`. Through that interface a connection failure
and an empty result set are the same two bytes of nothing, which is why
`zs_suite_db_exists` had to be three-valued with a paragraph explaining the
third. The driver distinguishes them for free.

This crate is the first three files of that migration. It is the substrate the
rest ports onto, so the shape matters more than the coverage.

## What is here

| Module | Was | What it decides |
| --- | --- | --- |
| `overlay` | `tests/lib/test_config.sh` | which server the suites dial, and refusing when the caller asked for a different one |
| `fingerprint` | `zs_schema_fingerprint` + `zs_fingerprint_of_ref` | the 12-hex name of the branch-keyed suite database, from a working tree and from a git tree, which MUST agree |
| `suite_db` | `tests/lib/suite_db.sh` | create-if-absent, never drop, tolerate losing the create race |
| `sweep` | `tests/lib/sweep_db.sh` | which databases are dead, and which are a peer agent's live run |
| `admin` | the `run_psql` seam | `exists` and `create`, over compio-postgres |
| `lock` | `flock 9>` | the per-machine provisioning lock |

## How to run it

One binary with subcommands. The consumers are `.sh` files, so a `#[test]`
cannot be reached from them and a harness nobody can invoke is worse than the
shell it replaced.

```bash
cargo build -p zeroship-testkit
target/debug/zs-testkit fingerprint dir --root .
target/debug/zs-testkit suite-db resolve --prefix zeroship_auth_test --root .
target/debug/zs-testkit sweep family-of --name zeroship_auth_test_a1eec1e1c30f
```

Nobody types those in practice. `tests/lib/{test_config,suite_db,sweep_db}.sh`
still export the same shell function names and the same variables, and their
bodies are now one call to this binary each -- so `tests/run_auth_suite.sh`,
`tests/run_billing_suite.sh` and `tests/sweep_test_databases.sh` did not change.
`tests/lib/testkit.sh` finds (and, when stale, builds) the binary.

## The dependency this crate must never take

`zeroship-core`. It depends on `cyper` -> `hyper` -> **tokio**, and measured on
2026-08-20 with `cargo tree -i tokio -e normal -p zeroship-control`, that is the
only edge by which tokio enters this workspace at all. So the overlay reader
here is forty hand-rolled lines rather than a call into
`zeroship_core::config::test_overlay`, and there is no HTTP client: `cargo tree
-i tokio -e normal -p zeroship-testkit` reports "did not match any packages",
with and without dev-dependencies, while the same command on
`zeroship-gateway` prints the full inversion tree. That pair is the check --
the second half is what proves the instrument discriminates rather than merely
runs.

This is the constraint that will decide how much more of `tests/` can follow:
50 of the 111 shell files call `curl`, and the in-tree HTTP client is the one
that drags tokio.

## Two rules this crate is built around

**No environment reads.** Not one, in library or binary: `std::env::var` and its
three siblings are workspace-denied, and nothing here reaches for `set_var`
either. Everything arrives as an argument or on stdin, including the values the
shell held in exported variables -- the shim reads its own environment and
passes what it found. That is what keeps the refusal for an ambient `TEST_DB`
honest: the thing doing the refusing is not itself steerable by the variable it
refuses.

**`WITH (FORCE)` is not a detail.** It terminates every other backend on a
database before dropping it. That is correct in `tests/lib/scratch_db.sh`, which
drops a database its own run created and would otherwise leak it on its own
stragglers. It is catastrophic in the sweeper, where a live connection is a peer
agent fifteen minutes into a suite. So `suite_db` and `sweep` contain no drop at
all -- the `DbAdmin` trait has exactly `exists` and `create` -- and
`tests/lib_suite_db_selftest.sh` asserts both the trait's shape and the absence
of the statement, with a positive control that proves the pattern can still find
one.

## Tests

```bash
cargo test -p zeroship-testkit          # 32 unit + 4 live (needs the overlay)
tests/lib_test_config_selftest.sh       # 16, hermetic
tests/lib_suite_db_selftest.sh          # 36, hermetic (was 43 with the psql seam)
tests/lib_sweep_db_selftest.sh          # 29 cases, reads /proc and git
```

`lib_sweep_db_selftest.sh` reports 28 passed / 1 failed, and did so on `main`
before this crate existed. The failing case is a negative control that picks its
"different migration set" by commit distance (`rev-list --skip=40`) rather than
by constructing one, so it goes red during any quiet period on migrations. It is
bound to repository history rather than to the variable it means to vary. Left
alone here deliberately: matching `main` exactly, failure and all, is what shows
the port changed no behaviour.

The live tests need `deploy/ops/zeroship.test.toml`; without it they announce a
skip that `tests/lib/skip_census.sh` counts, rather than passing silently.

The real gate is `tests/run_auth_suite.sh`, which provisions through this crate
and must reach its floor.
