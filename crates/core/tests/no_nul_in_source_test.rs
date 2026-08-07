//! No tracked source file may contain a NUL byte.
//!
//! This is not about the compiler - it is about every grep-based sweep in this
//! repository, and about the ones a reviewer runs by hand.
//!
//! The `grep` on this machine is ugrep 7.5.0. Given a file containing a NUL
//! byte it PRINTS NOTHING. Measured, both arms, on files made for the run:
//!
//!     grep -c needle clean.txt   stdout "1"    exit 0
//!     grep -c needle nul.txt     stdout EMPTY  exit 1, stderr EMPTY
//!
//! Note what the second line is not. It does not print `0`, and it does not
//! warn. An earlier version of this comment said ugrep "reports no matches",
//! which is the outcome inferred rather than the output seen - the tool told us
//! nothing and it got written down as having told us zero. Nothing distinguishes
//! that from a file genuinely lacking the pattern except the exit code, and
//! nobody reads the exit code of a grep they expect to succeed.
//!
//! The version matters and is why it is named. GNU grep does not behave this
//! way: it prints "Binary file ... matches", which is loud enough to notice. The
//! hazard this test guards is specific to the grep in this environment.
//!
//! That makes a NUL byte in a source file a silent hole in every audit. A sweep
//! for dead citations, leftover process markers, or a dangerous call reports
//! clean, and the clean report is indistinguishable from a clean tree. A peer
//! project lost hours to exactly this: two of its files carried a NUL, and they
//! were precisely the two holding the tags a task had been open on.
//!
//! Today no tracked source file here contains one - verified when this test was
//! written, across the whole index. This exists so that stays true, because the
//! failure mode is invisible by construction and nobody would go looking.
//!
//! A NUL in a genuine binary (images, fonts, compiled artifacts) is normal and
//! ignored; only the extensions a sweep would plausibly grep are checked.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

/// Extensions a text sweep would grep. A NUL in any of these is the hazard.
const SOURCE_EXTS: &[&str] = &[
    "rs", "ts", "tsx", "js", "mjs", "cjs", "jsx", "toml", "sh", "md", "json", "yml", "yaml", "sql",
    "css", "html",
];

/// The file list comes from `git ls-files`, not a directory walk.
///
/// A walk gets the scope wrong in both directions and the first attempt here
/// did: it swept up WPT's deliberately-UTF-16 encoding fixtures, `.direnv` nix
/// inputs and `refs/` reference checkouts - none of them ours, all of them
/// legitimately full of NUL bytes. Untracked is exactly the line that matters,
/// because a NUL only misleads a sweep over files we actually own and edit.
fn tracked_source_files(root: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .expect("git ls-files");
    assert!(out.status.success(), "git ls-files failed");
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty())
        .filter(|p| {
            Path::new(p)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| SOURCE_EXTS.contains(&e))
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn no_tracked_source_file_contains_a_nul_byte() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/core sits two levels below the repo root")
        .to_path_buf();

    let files = tracked_source_files(&root);

    // The enumeration is the thing most likely to break silently: a wrong root
    // or an over-broad skip list yields a short list, and a short list that is
    // fully read passes every downstream check.
    //
    // This floor is a TOLERANCE, not a degenerate-case guard, and the read check
    // below does not redeem it. That check is proportional to whatever the
    // enumeration produced, so it cannot see enumeration loss at all: enumerate
    // 501 files and read all 501 and it reports 100 percent. Measured, not
    // reasoned - truncating the list to 501 of 2397 left this test green, and
    // the only tell was the clock (0.01s against 0.22s).
    //
    // The tolerance is irreducible. A proportional bound needs a denominator,
    // and every count available here comes from the same `git ls-files`, so a
    // truncated listing is indistinguishable from a smaller repository. Only the
    // SIZE is a choice, and it is set to the same ~7 percent headroom the CI
    // test-target floor carries, against 2397 measured on 2026-08-07. Raise it
    // when the tree grows; a fixed floor gets looser every time a file is added,
    // which is the wrong direction for a guard against coverage loss.
    const MIN_ENUMERATED: usize = 2230;
    assert!(
        files.len() >= MIN_ENUMERATED,
        "git ls-files returned {} source files under {}, fewer than the {} this \
         gate expects to scan - the enumeration is not reaching the tree it \
         claims to cover, so a pass would mean nothing. If the repository really \
         did shrink, lower MIN_ENUMERATED deliberately; do not treat the gap as \
         slack.",
        files.len(),
        root.display(),
        MIN_ENUMERATED,
    );

    let mut offenders = Vec::new();
    let mut read = 0usize;
    for path in &files {
        // The skip exists for a path `git ls-files` names but the working tree
        // does not hold - a submodule gitlink, a sparse checkout. It must not
        // become a way for the whole scan to succeed at reading nothing.
        let Ok(bytes) = fs::read(root.join(path)) else { continue };
        read += 1;
        if bytes.contains(&0) {
            offenders.push(path.clone());
        }
    }

    // Enumerating and READING are different failures and the count above only
    // guards the first. If the root stopped resolving, every read would fail,
    // every file would be skipped, `offenders` would be empty, and this would
    // report a clean tree having opened nothing. Planting a NUL proves the test
    // can say no; it proves nothing about why it says yes.
    assert!(
        read * 10 >= files.len() * 9,
        "only {read} of {} enumerated files were actually opened and read - a scan \
         that reads nothing must not report a clean tree",
        files.len(),
    );

    // Report the coverage on the SUCCESS path, not only inside a panic message.
    // Every number this test knows lived in its assertions, which means it was
    // only ever visible on the one run where nobody needed it. A gate that says
    // how much it covered lets a reader notice the count sagging without anyone
    // first breaking the plumbing on purpose to find out.
    //
    // The direct handle write is load-bearing and `println!` would not do. The
    // test harness captures output by swapping the thread-local target that the
    // `print!`/`eprint!` machinery writes through, and it only replays that
    // buffer for a FAILING test. A write straight to the `Stderr` handle never
    // enters the buffer. Measured: in one passing test with no `--nocapture`,
    // `println!` and `eprintln!` vanished while `stderr().write_all` and
    // `stdout().write_all` both appeared.
    let _ = std::io::stderr().write_all(
        format!(
            "no_nul_in_source_test: read {read} of {} enumerated source files, \
             {} carry a NUL byte\n",
            files.len(),
            offenders.len(),
        )
        .as_bytes(),
    );

    assert!(
        offenders.is_empty(),
        "these source files contain a NUL byte: {offenders:?}\n\
         ugrep reports NO MATCHES in such a file, silently and with exit 1, so \
         every grep-based sweep over it comes back clean whatever it contains. \
         Strip the NUL, or if the file is genuinely binary give it a binary \
         extension so sweeps stop pretending to read it."
    );
}
