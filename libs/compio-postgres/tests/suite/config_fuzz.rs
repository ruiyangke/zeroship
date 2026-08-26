//! Randomised connection strings, to generalise the hand-written parser cases.
//!
//! The DSN parser is the largest hand-written parser in this crate and the one
//! with the most moving parts: two syntaxes (keyword and URL), single-quoted
//! values with backslash escapes, percent-decoding, a comma-separated host
//! list, and repeated keys that override. Seven real defects were found in it
//! on 2026-08-23 alone, every one by comparing against libpq on an input a
//! person thought of. This file feeds it inputs nobody thought of.
//!
//! THE PROPERTY IS TOTALITY, NOT CORRECTNESS. A fuzzer has no oracle for what a
//! random string should mean, so asserting a parse RESULT would only restate
//! the implementation. What it can assert is that the parser is a total
//! function over `&str`: every input yields `Ok` or `Err`, and neither panics,
//! hangs, nor overflows. That matters because a DSN is frequently caller
//! supplied -- an application reading one from its own config or environment
//! hands this arbitrary bytes -- and a panic there takes the caller down.
//!
//! Two properties beyond "does not panic":
//!
//! 1. Anything that parses must survive `Debug`. That is the REDACTION path
//!    (`Config`'s Debug hides the password), so a panic in it is reachable from
//!    any log line, and it runs only on inputs that parsed.
//! 2. Parsing is deterministic: the same input twice gives the same verdict.
//!    A parser carrying hidden state across calls would show up here and
//!    nowhere else in the suite.
//!
//! DETERMINISTIC BY CONSTRUCTION, in the style of `tests/frame_fuzz.rs`: a
//! seeded xorshift written inline, no dependency added to any manifest, and a
//! failure prints the exact input and seed that produced it.

use compio_postgres::Config;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // A zero state is a fixed point for xorshift, so never allow one.
        Self(if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// Fragments chosen because they are where the parser makes a DECISION:
/// separators, quoting, escapes, percent-encoding, the URL authority, and the
/// keys whose values are themselves parsed.
const FRAGMENTS: &[&str] = &[
    "=",
    " ",
    "'",
    "\\",
    "\"",
    "%",
    "%%",
    "%zz",
    "%41",
    "/",
    "//",
    ":",
    "@",
    "?",
    "&",
    ",",
    "\t",
    "\n",
    "\r",
    "\x0b",
    "\x0c",
    "\0",
    "postgres://",
    "postgresql://",
    "host",
    "hostaddr",
    "port",
    "user",
    "password",
    "dbname",
    "options",
    "sslmode",
    "keepalives",
    "connect_timeout",
    "target_session_attrs",
    "application_name",
    "client_encoding",
    "replication",
    "load_balance_hosts",
    "a",
    "1",
    "-1",
    "0",
    "65536",
    "999999999999999999999",
    "localhost",
    "127.0.0.1",
    "[::1]",
    "verify-full",
    "require",
    "UTF8",
    "x=y",
];

fn generate(rng: &mut Rng) -> String {
    let pieces = 1 + rng.below(12);
    let mut out = String::new();
    for _ in 0..pieces {
        out.push_str(FRAGMENTS[rng.below(FRAGMENTS.len())]);
    }
    out
}

/// The parser is total: no input panics, and its verdict is stable.
#[test]
fn random_connection_strings_never_panic_the_parser() {
    // Fixed seeds, so a failure is reproducible and CI cannot go pink on a
    // Tuesday. Widening the seed set is how this file grows.
    for seed in [1u64, 0x5eed, 0xdead_beef, 0x0123_4567_89ab_cdef] {
        let mut rng = Rng::new(seed);
        for case in 0..2_000 {
            let input = generate(&mut rng);

            // `catch_unwind` is not used: a panic here SHOULD fail the test,
            // and the harness already prints which one. The input is printed
            // first so the failing case is visible above the panic.
            let first = std::panic::catch_unwind(|| input.parse::<Config>().is_ok())
                .unwrap_or_else(|_| {
                    panic!("seed {seed:#x} case {case}: parser panicked on {input:?}")
                });

            // Property 2: stable verdict across repeated parses.
            let second = std::panic::catch_unwind(|| input.parse::<Config>().is_ok())
                .unwrap_or_else(|_| {
                    panic!("seed {seed:#x} case {case}: second parse panicked on {input:?}")
                });
            assert_eq!(
                first, second,
                "seed {seed:#x} case {case}: parsing {input:?} was not deterministic"
            );

            // Property 1: whatever parsed must survive the redacting Debug.
            if first {
                let config = input.parse::<Config>().expect("just parsed Ok");
                let rendered =
                    std::panic::catch_unwind(|| format!("{config:?}")).unwrap_or_else(|_| {
                        panic!("seed {seed:#x} case {case}: Debug panicked on {input:?}")
                    });
                assert!(
                    !rendered.is_empty(),
                    "seed {seed:#x} case {case}: Debug rendered nothing for {input:?}"
                );
            }
        }
    }
}

/// The generator must actually reach both verdicts.
///
/// Without this the test above is satisfied by a generator that only ever
/// produces garbage the parser rejects on the first byte -- it would never
/// exercise the value parsers, the URL authority, or `Debug` at all, and would
/// still pass. This asserts the corpus straddles the accept/reject boundary.
#[test]
fn the_generator_produces_both_accepted_and_rejected_inputs() {
    let mut rng = Rng::new(0x5eed);
    let (mut accepted, mut rejected) = (0usize, 0usize);
    for _ in 0..2_000 {
        if generate(&mut rng).parse::<Config>().is_ok() {
            accepted += 1;
        } else {
            rejected += 1;
        }
    }
    assert!(
        accepted > 0 && rejected > 0,
        "the corpus must straddle the boundary to be worth anything: \
         {accepted} accepted, {rejected} rejected"
    );
}
