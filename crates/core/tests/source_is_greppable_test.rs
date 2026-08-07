//! Every tracked source file must be greppable.
//!
//! This is not about the compiler. It is about every grep-based sweep in this
//! repository, and about the ones a reviewer runs by hand. A file grep refuses
//! to read is a silent hole in all of them: a sweep for dead citations,
//! leftover process markers, or a dangerous call reports clean, and that clean
//! report is indistinguishable from a clean tree.
//!
//! Two byte-level properties cost a file its greppability, and BOTH are checked
//! here because the first one alone is not enough - proven, not assumed.
//!
//! # 1. A NUL byte
//!
//! TWO greps are in play and they differ, so both were measured. The
//! interactive shell resolves ugrep 7.5.0; `nix develop` - which is what the
//! test suite and CI run under - resolves GNU grep 3.12. Same two files, one
//! clean and one with a single trailing NUL:
//!
//!                      LINE MODE                 -c
//!     ugrep 7.5.0      empty, exit 1             empty, exit 1
//!     GNU grep 3.12    empty, exit 0             "1",   exit 0
//!
//! The clean file matches in every cell. Read the NUL column carefully, because
//! the naive summary - "the local grep is blind, the build grep is fine" - is
//! wrong and was the first conclusion reached here.
//!
//! GNU grep counts correctly under `-c` and is BLIND IN LINE MODE, which is the
//! mode a reviewer actually uses (`grep -rn pattern .`). It omits the matching
//! line and exits 0. That is stealthier than ugrep, which at least exits 1: a
//! successful exit with no output is exactly what a genuinely absent pattern
//! looks like, so the hazard is worse in the environment the tests run under,
//! not absent from it.
//!
//! # 2. Invalid UTF-8, with no NUL anywhere
//!
//! This gate was NUL-only when it was written, and that was too narrow. A file
//! carrying a lone `0xE2` - the lead byte of an em-dash whose two continuation
//! bytes are missing - is binary to ugrep just as surely, and there is no NUL
//! in it to find. Measured 2026-08-07 against a real file in this
//! environment:
//!
//!     grep    "<term>" f    ->  no output, exit 1
//!     grep -c "<term>" f    ->  no output, exit 1     (not even "0")
//!     grep -a -c "<term>" f ->  1                     (present all along)
//!     iconv -f UTF-8 -t UTF-8 f -> exit 1             (the actual tell)
//!
//! The NUL-only form of this test read that file, found no NUL, and passed.
//! Confirmed by planting exactly such a file in the tree: 2407 of 2407 files
//! read, "0 carry a NUL byte", green.
//!
//! What makes this worth guarding is how ordinary the cause is. It is not
//! exotic corruption. It is any "shorten this file" edit done with byte
//! semantics - `head -c`, `cut -c`, a byte `substr` - landing mid-character.
//! That is precisely how the file above acquired it.
//!
//! # On the wording of this comment
//!
//! Three earlier versions were wrong in the way this section exists to prevent.
//! The first said ugrep "reports no matches", which is the outcome inferred
//! rather than the output seen - the tool printed nothing and it was written
//! down as having told us zero. The second asserted GNU grep prints "Binary
//! file ... matches"; 3.12 prints no such message on either path, and that
//! claim came from memory rather than a run. The third - this gate's own
//! former title - promised that a pass meant every sweep was trustworthy,
//! while checking only half of what that requires.
//!
//! Today no tracked source file here fails either check, verified across the
//! whole index. This exists so that stays true, because both failure modes are
//! invisible by construction and nobody would go looking.
//!
//! Genuine binaries (images, fonts, compiled artifacts) are not our concern and
//! are not scanned; only the extensions a sweep would plausibly grep.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

/// Extensions a text sweep would grep. An unreadable file among these is the hazard.
const SOURCE_EXTS: &[&str] = &[
    "rs", "ts", "tsx", "js", "mjs", "cjs", "jsx", "toml", "sh", "md", "json", "yml", "yaml", "sql",
    "css", "html",
];

/// The file list comes from `git ls-files`, not a directory walk.
///
/// A walk gets the scope wrong in both directions and the first attempt here
/// did: it swept up WPT's deliberately-UTF-16 encoding fixtures, `.direnv` nix
/// inputs and `refs/` reference checkouts - none of them ours, all of them
/// legitimately unreadable as text. Untracked is exactly the line that matters,
/// because an unreadable file only misleads a sweep over files we own and edit.
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
fn every_tracked_source_file_is_greppable() {
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
    // test-target floor carries, against 2406 measured on 2026-08-07. Raise it
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

    let mut nul_offenders = Vec::new();
    let mut non_utf8_offenders = Vec::new();
    let mut read = 0usize;
    for path in &files {
        // The skip exists for a path `git ls-files` names but the working tree
        // does not hold - a submodule gitlink, a sparse checkout. It must not
        // become a way for the whole scan to succeed at reading nothing.
        let Ok(bytes) = fs::read(root.join(path)) else { continue };
        read += 1;
        // Reported separately because the remedies differ: a NUL usually means
        // the file is genuinely binary and misfiled, while invalid UTF-8 in an
        // otherwise-text file means an edit severed a character and the fix is
        // to repair that character.
        if bytes.contains(&0) {
            nul_offenders.push(path.clone());
        } else if std::str::from_utf8(&bytes).is_err() {
            non_utf8_offenders.push(path.clone());
        }
    }

    // Enumerating and READING are different failures and the count above only
    // guards the first. If the root stopped resolving, every read would fail,
    // every file would be skipped, both offender lists would be empty, and this
    // would report a clean tree having opened nothing. Planting a bad file
    // proves the test can say no; it proves nothing about why it says yes.
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
            "source_is_greppable_test: read {read} of {} enumerated source files, \
             {} carry a NUL byte, {} are not valid UTF-8\n",
            files.len(),
            nul_offenders.len(),
            non_utf8_offenders.len(),
        )
        .as_bytes(),
    );

    assert!(
        nul_offenders.is_empty(),
        "these source files contain a NUL byte: {nul_offenders:?}\n\
         ugrep reports NO MATCHES in such a file, silently and with exit 1, and \
         GNU grep omits the line while exiting 0, so every grep-based sweep over \
         it comes back clean whatever it contains. Strip the NUL, or if the file \
         is genuinely binary give it a binary extension so sweeps stop pretending \
         to read it."
    );

    assert!(
        non_utf8_offenders.is_empty(),
        "these source files are not valid UTF-8: {non_utf8_offenders:?}\n\
         grep treats them as binary and finds nothing in them, exactly as it does \
         for a NUL, so every sweep over them is vacuous. The usual cause is an \
         edit that truncated the file by BYTES and cut a multibyte character in \
         half. Find the severed character and repair it - `iconv -f UTF-8 -t \
         UTF-8 <file>` exits non-zero while it is still broken - and redo the \
         edit with character semantics."
    );
}
