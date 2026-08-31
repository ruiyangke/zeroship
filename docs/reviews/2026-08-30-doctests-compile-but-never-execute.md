# Every doc example in compio-postgres is `no_run`, so their assertions never run

Measured 2026-08-30 at `ebd1f71d0`.

    cargo test -p compio-postgres --doc --features with-chrono-0_4,with-time-0_3
    running 5 tests
    test result: ok. 5 passed; 0 failed; ...; finished in 0.00s

**0.00s is the tell.** Five doctests "passed" without executing anything.

Fence census over `libs/compio-postgres/src/`:

    no_run     5      compiled, NOT executed
    text       5      not compiled
    not_rust   8      not compiled
    (plain)    0      <- there are none

There is not a single plain ```` ``` ```` Rust block in the crate's docs. The 18
bare fences in the grep are the closers.

## Why this matters

The crate's front-page example (`lib.rs:18`, the `no_run` block) ends with

    let value: &str = rows[0].get(0);
    assert_eq!(value, "hello world");

A reader sees `assert_eq!` in the crate's headline example and reasonably infers
it is checked on every `cargo test`. It is not. It is type-checked and
borrow-checked; the comparison never happens.

## This is not a defect, and should not be "fixed"

Every one of these examples opens a real connection. Making them execute would
require a live PostgreSQL for `cargo test --doc`, which would turn a
documentation build into an integration test and break any offline `cargo test`.
`no_run` is the correct choice here.

What is worth doing is what was done: say so at the assertion, so nobody reads
it as a guarantee, and nobody "strengthens" the docs by adding more assertions
believing they are verified.

## The general shape

This is the same family as the other findings recorded this week - a construct
that READS as protection while providing none:

- a `grep -c` piped to `head`, whose exit status is `head`'s, so an ASCII guard
  fired unconditionally;
- a test filter matching zero tests, which prints `ok`;
- a `#[cfg(test)]` module whose tests no verified feature resolution compiles;
- and now an `assert_eq!` in a `no_run` fence.

In each case the artifact exists, looks right, and checks nothing. Ask what the
thing is KEYED to, not whether it is present.
