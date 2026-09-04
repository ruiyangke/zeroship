//! The single source of truth for capping a **derived** SQL identifier to the
//! Postgres `NAMEDATALEN` budget.
//!
//! # Why derived names get capped and authored names get refused
//!
//! `query::validate_collection` / `query::validate_field_name` REFUSE a name
//! over [`PG_MAX_IDENT_BYTES`]. That is right for names the creator chose: they
//! can shorten them, and a refusal tells them so.
//!
//! It is wrong for names the platform DERIVES from those — `<coll>_<col>_idx`,
//! `<coll>__<col>_masked_idx`, `<field>_fkey`, ... . A
//! collection at exactly the 63-byte ceiling passes validation, yet every name
//! derived from it overflows. Refusing there would fail the creator for
//! something they did not do, so the derived name is capped instead.
//!
//! # Why capping is not optional
//!
//! Postgres does not error on an over-long identifier. It truncates to
//! `NAMEDATALEN - 1` bytes and emits a NOTICE. Two derived names that share
//! their first 63 bytes therefore become ONE identifier — and because almost
//! every `CREATE INDEX` this crate emits carries `IF NOT EXISTS`, the second
//! one is a SILENT no-op: both statements report success, the truncation and
//! the skip are both NOTICEs, and only one index exists.
//!
//! For a `CREATE UNIQUE INDEX` that means the uniqueness the schema declares
//! does not exist in the database, with nothing in the catalog to notice it.
//!
//! # One function here, and a CONDITIONAL agreement with the engine
//!
//! Every derivation site in this crate routes through [`cap_ident_name`]. A
//! second copy is the actual failure mode here: this crate previously carried
//! FOUR inlined copies of the cap, each cutting at 60 bytes with an 8-char
//! base32 tail, while the migration engine's
//! `zeroship_migrate_core::plan::author::cap_ident_name` cuts at 63 with a
//! 10-hex tail — so for a natural name in the 61..=63-byte window the engine
//! emitted it verbatim while the runtime hashed it, and the two disagreed about
//! what the index is called.
//!
//! [`cap_ident_name`] now follows the engine's scheme. It is NOT the same
//! function and cannot be: the engine takes a `VendorSet` and reads its budget
//! from `render::backends::generated_ident_max_bytes(vendors)`, the tightest
//! identifier limit any REGISTERED backend declares, while this crate is a leaf
//! with no registry and bakes the literal [`PG_MAX_IDENT_BYTES`]. The two do not
//! even share an arity, so "byte-for-byte identical" — which this block claimed
//! until 2026-09-04 — is not a statement that can be true.
//!
//! The real obligation is behavioural and conditional: for every input,
//! `cap_ident_name(n)` must equal
//! `plan::author::cap_ident_name(zeroship_migrate::shipping_vendors(), n)`, and
//! that holds only while the tightest shipping backend's identifier limit is 63.
//! Register a backend declaring a tighter one and the engine's cap moves on its
//! own while this one does not.
//!
//! `engine_parity` (below) is the guard for both halves: it drives the two
//! functions over a corpus spanning the boundary, and separately pins the
//! literal 63 against the shipping registry, so either half moving alone fails
//! the build.
//!
//! # "The engine's cap" is ambiguous unless the function is named
//!
//! The engine carries more than one capping scheme, deliberately.
//! `zeroship_migrate_core::schema::query::index_name` — the engine's own copy of
//! this crate's [`crate::query::index_name`] — still cuts at 60 bytes with an
//! 8-char base32 tail and never routes through `plan::author::cap_ident_name`.
//! That divergence is documented and absorbed on the engine side rather than
//! removed (`render/declarative.rs`'s `AcceptedIndexAlias` records the other
//! spelling of a derived index name), so it is a known state, not a second bug
//! this module is hiding. Only `plan::author::cap_ident_name` is what this file
//! agrees with.
//!
//! `ident_cap_has_exactly_one_implementation` (below) fails the build if any
//! other file in this crate starts hashing identifiers on its own.

/// The Postgres identifier length limit (`NAMEDATALEN - 1`), in bytes.
///
/// An identifier longer than this is silently truncated *by the server*, which
/// is why nothing downstream may emit a longer one. Postgres can be compiled
/// with a larger `NAMEDATALEN`, but 63 is the default and the only value we
/// target; capping below the server's real limit is always safe.
pub const PG_MAX_IDENT_BYTES: usize = 63;

/// Number of hex characters of the full-name digest appended on overflow.
///
/// 10 hex chars = 40 bits. Distinct inputs that share a truncated prefix would
/// have to collide in 40 bits of SHA-256 to collapse onto one identifier.
const HASH_HEX_CHARS: usize = 10;

/// Cap a DERIVED identifier to at most [`PG_MAX_IDENT_BYTES`] bytes,
/// deterministically and collision-safely.
///
/// * A name that already fits is returned **unchanged** — capping must not
///   churn the identifier of every index that was never at risk.
/// * A name that overflows becomes `<readable prefix>_<10 hex of SHA-256 of the
///   FULL natural name>`. The hash covers the whole input, not the truncated
///   remainder, so two inputs that diverge only after byte 63 still produce two
///   different identifiers.
/// * Same input, same output, always: `CREATE ... IF NOT EXISTS` and the
///   matching `DROP` must agree across processes and releases.
///
/// The prefix is truncated on a UTF-8 char boundary. Derived identifiers are
/// ASCII in practice (both validators enforce an ASCII allowlist), but slicing
/// by raw byte offset would be a panic waiting for the first caller that is
/// not.
///
/// Behaviourally equal to the migration engine's
/// `zeroship_migrate_core::plan::author::cap_ident_name` **under the shipping
/// vendor set** — not the same function: the engine takes a `VendorSet` and
/// reads its budget off the registry, this crate bakes [`PG_MAX_IDENT_BYTES`].
/// `engine_parity::the_two_caps_agree_over_the_corpus` is what holds that
/// equality, and
/// `engine_parity::the_baked_budget_is_the_tightest_limit_the_shipping_backends_declare`
/// is what holds the condition it rests on. Break either and the engine and the
/// runtime disagree about what an index is called.
#[must_use]
pub fn cap_ident_name(natural: &str) -> String {
    use sha2::{Digest, Sha256};

    if natural.len() <= PG_MAX_IDENT_BYTES {
        return natural.to_string();
    }

    // Lowercase hex, hand-rolled: the crate does not depend on `hex` and a new
    // dependency for ten characters is not worth it. The table lookup is
    // masked to 0..=15, so it is total — no panic path to document.
    const HEX: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
    ];
    let digest = Sha256::digest(natural.as_bytes());
    let mut suffix = String::with_capacity(HASH_HEX_CHARS);
    for byte in digest.iter().take(HASH_HEX_CHARS / 2) {
        suffix.push(HEX[usize::from(byte >> 4)]);
        suffix.push(HEX[usize::from(byte & 0x0f)]);
    }

    // Reserve `_<suffix>` on the tail; the rest is a readable prefix of the
    // natural name.
    let budget = PG_MAX_IDENT_BYTES - (1 + suffix.len());
    let mut prefix = String::with_capacity(budget);
    for ch in natural.chars() {
        if prefix.len() + ch.len_utf8() > budget {
            break;
        }
        prefix.push(ch);
    }
    format!("{prefix}_{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name inside the budget must come back byte-for-byte. A cap that
    /// rewrote short names would rename every index that was never at risk.
    ///
    /// What this does NOT catch: it says nothing about the overflow branch, and
    /// nothing about which names the derivation sites feed in.
    #[test]
    fn short_names_are_returned_unchanged() {
        for n in ["users_email_idx", "a", "", &"x".repeat(62), &"y".repeat(63)] {
            assert_eq!(cap_ident_name(n), n, "name within budget must not be rewritten");
        }
    }

    /// One byte over the budget is where the hash must start.
    #[test]
    fn the_boundary_is_inclusive_at_63_and_caps_at_64() {
        assert_eq!(cap_ident_name(&"z".repeat(63)).len(), 63);
        let capped = cap_ident_name(&"z".repeat(64));
        assert_ne!(capped, "z".repeat(64));
        assert_eq!(capped.len(), PG_MAX_IDENT_BYTES);
    }

    /// `CREATE ... IF NOT EXISTS` and its `DROP` run in different processes and
    /// different releases; they must derive the same name.
    ///
    /// What this does NOT catch: determinism within one build only — it cannot
    /// detect a future change to the digest or the budget, which would be
    /// self-consistently deterministic and still wrong.
    #[test]
    fn capping_is_deterministic() {
        let long = format!("{}__{}_idx", "c".repeat(63), "alpha_masked");
        assert_eq!(cap_ident_name(&long), cap_ident_name(&long));
    }

    /// The point of the whole module: inputs that are identical for the first
    /// 63 bytes and diverge only afterwards must NOT collapse onto one
    /// identifier. Truncation alone would; the hash covers the full name.
    #[test]
    fn inputs_diverging_after_the_truncation_point_stay_distinct() {
        let coll = "c".repeat(63);
        let a = cap_ident_name(&format!("{coll}__alpha_masked_idx"));
        let b = cap_ident_name(&format!("{coll}__beta_masked_idx"));
        assert_ne!(a, b, "distinct derived names collapsed: {a} / {b}");
        assert!(a.len() <= PG_MAX_IDENT_BYTES && b.len() <= PG_MAX_IDENT_BYTES);
    }

    /// Every output must fit, whatever the input length or shape.
    #[test]
    fn every_output_fits_the_budget() {
        for len in 0..200 {
            let out = cap_ident_name(&"n".repeat(len));
            assert!(out.len() <= PG_MAX_IDENT_BYTES, "len {len} produced {} bytes", out.len());
        }
    }

    /// A multi-byte input must not be sliced mid-character. Both identifier
    /// validators reject non-ASCII today, so this guards the function against a
    /// future caller rather than a current one.
    #[test]
    fn multibyte_input_is_truncated_on_a_char_boundary() {
        // 3 bytes per char, 40 chars = 120 bytes.
        let out = cap_ident_name(&"日".repeat(40));
        assert!(out.len() <= PG_MAX_IDENT_BYTES);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    /// Structural guard for the "one function" rule: the identifier cap is
    /// implemented here and nowhere else in the crate. Four inlined copies is
    /// what this file replaced; a fifth would drift the same way.
    ///
    /// What this does NOT catch: a second implementation that hashes with
    /// something other than `sha2`, or one that lives in a different crate.
    #[test]
    fn ident_cap_has_exactly_one_implementation() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut hashing_files: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&src).expect("src/ is readable") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let body = std::fs::read_to_string(&path).expect("source is utf-8");
            if body.contains("Sha256") {
                hashing_files
                    .push(path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string());
            }
        }
        assert_eq!(
            hashing_files,
            vec!["ident.rs".to_string()],
            "identifier hashing must live only in ident.rs; found it in {hashing_files:?}"
        );
    }
}

/// Binds [`cap_ident_name`] to the migration engine's
/// `plan::author::cap_ident_name` across the crate boundary.
///
/// # What was unbound
///
/// The module header claimed the two were "byte-for-byte" identical. Nothing
/// named the engine's copy: this file's only other structural test,
/// `ident_cap_has_exactly_one_implementation`, rules on THIS crate alone. The
/// claim was also unfalsifiable as worded — the two do not share an arity, so no
/// reading of "byte-for-byte" could ever have been checked.
///
/// # What a divergence would cost, and why nothing would report it
///
/// Both crates derive on-disk index and constraint names for the SAME live
/// databases. A name this crate derives is emitted through
/// `CREATE [UNIQUE] INDEX [CONCURRENTLY] IF NOT EXISTS` (`query.rs`, every index
/// emitter) and `ALTER TABLE ... DROP CONSTRAINT IF EXISTS` (`query::fk_name`
/// feeds `query.rs:1615`). Both clauses report success on a name that is not
/// what the other side spelled: the CREATE makes a second index nobody asked
/// for, and the DROP removes nothing while reporting that it did.
///
/// The header's own severity note is about the ADJACENT failure — two derived
/// names colliding onto one identifier, where `IF NOT EXISTS` skips the second
/// `CREATE UNIQUE INDEX` and the declared uniqueness is simply absent. That half
/// is measured: `IF NOT EXISTS` is present on every index emitter in `query.rs`.
/// A cross-crate divergence is the quieter of the two only on the DROP path; on
/// the CREATE path it duplicates rather than skips.
///
/// # Which arms are independent, and which ride a derivation
///
/// The readable-prefix budget is DERIVED from [`PG_MAX_IDENT_BYTES`] and
/// [`HASH_HEX_CHARS`] on this side and from `max` and the tail width on the
/// engine's, so a single mutation moves several outputs at once and fails more
/// arms than it was aimed at. Measured, one mutation at a time, each against an
/// unmutated control (5 new arms + the 6 pre-existing `tests` arms):
///
/// * `PG_MAX_IDENT_BYTES` 63 -> 62 fails ALL FIVE new arms and NONE of the six
///   pre-existing ones. The budget is the widest-blast clause here; nothing in
///   this suite isolates it, and nothing needs to.
/// * The `PostgreSQL` descriptor's `IdentifierLimit::Bytes(63)` -> `Bytes(60)`
///   (`zeroship-migrate-postgres/src/descriptor.rs`) also fails all five and
///   none of the six. This is the standing risk the header names — the ENGINE's
///   budget moving on its own — and
///   [`the_baked_budget_is_the_tightest_limit_the_shipping_backends_declare`] is
///   the arm that names the cause, because it is the only one that reads the
///   registry rather than the engine's output.
/// * [`HASH_HEX_CHARS`] 10 -> 8 fails exactly two:
///   [`the_two_caps_agree_over_the_corpus`] and
///   [`both_hash_tails_are_ten_lowercase_hex_characters`]. The budget arms stay
///   green because the output still fills 63 bytes — which is also why
///   `tests::every_output_fits_the_budget` above cannot see it, and why all six
///   pre-existing arms pass under it.
/// * The ENGINE's tail width (`&digest[..5]` -> `&digest[..4]` in
///   `plan/author.rs`) fails the same two arms from the other side and no
///   pre-existing arm. `the_two_caps_agree_over_the_corpus` and the tail arm are
///   the only things in this crate that evaluate the engine at all.
/// * This crate's `<=` -> `<` fails three:
///   [`both_sides_switch_to_the_hashed_form_at_the_same_input_length`],
///   [`the_two_caps_agree_over_the_corpus`] and
///   [`both_sides_measure_the_cap_in_bytes_not_characters`], plus the
///   pre-existing `tests::short_names_are_returned_unchanged`. It does NOT reach
///   the budget arm: the boundary is a comparison, not a number.
///
/// So: [`the_baked_budget_is_the_tightest_limit_the_shipping_backends_declare`]
/// is independently bound (it is the only arm that fails on a registry change
/// this crate never sees, and the only one a `<=` flip leaves green), and
/// [`both_hash_tails_are_ten_lowercase_hex_characters`] is independently bound
/// against a tail-width change on either side. The other three ride the budget
/// derivation and fail together whenever it moves.
#[cfg(test)]
mod engine_parity {
    use super::*;

    /// The identifier budget both sides must be capping to.
    ///
    /// Stated as a literal, once, so each side is compared against a NUMBER
    /// rather than against the other side. Comparing the two functions to each
    /// other alone would go green on a coordinated change; this constant is what
    /// makes a one-sided change fail, and
    /// [`the_baked_budget_is_the_tightest_limit_the_shipping_backends_declare`]
    /// is what keeps the literal itself honest.
    const IDENTIFIER_BUDGET_BYTES: usize = 63;

    /// Length of the hex tail both sides append on overflow.
    const HASH_TAIL_HEX_CHARS: usize = 10;

    /// The readable prefix both sides keep when they hash: the budget less
    /// `_<tail>`.
    const PREFIX_BUDGET_BYTES: usize = IDENTIFIER_BUDGET_BYTES - (1 + HASH_TAIL_HEX_CHARS);

    /// A 3-byte character. The cap is measured in BYTES on both sides and the
    /// readable prefix is truncated on a CHAR boundary, so a multi-byte name is
    /// the only input that separates those two readings.
    const WIDE: &str = "\u{65e5}";

    /// The migration engine's cap, driven by the real shipping vendor set.
    ///
    /// `plan::author::cap_ident_name` reads its budget off the registry, so it
    /// cannot be evaluated without a composed set.
    /// `zeroship_migrate::shipping_vendors()` is the one the platform ships, and
    /// it is the set the migrate-server applies creator migrations under
    /// (`zeroship-migrate-server/src/apply.rs`).
    fn engine_cap(natural: &str) -> String {
        zeroship_migrate_core::plan::author::cap_ident_name(
            zeroship_migrate::shipping_vendors(),
            natural,
        )
    }

    /// Inputs the two caps must agree on.
    ///
    /// Spans the boundary from both directions (at the budget, one byte over,
    /// well over), the readable-prefix budget, non-ASCII in both the "at the
    /// cap" and "over the cap" positions, and the four name shapes this crate
    /// actually derives from a collection sitting on the declaration ceiling.
    fn corpus() -> Vec<String> {
        let budget = IDENTIFIER_BUDGET_BYTES;
        let mut names = vec![
            // Degenerate inputs. Both functions are TOTAL, so a divergence on an
            // input no caller produces is as real as one at the boundary.
            String::new(),
            "a".to_string(),
            "users_email_idx".to_string(),
        ];
        // The readable-prefix window: a prefix-budget change shows up here
        // without the overflow threshold moving.
        for len in [PREFIX_BUDGET_BYTES - 1, PREFIX_BUDGET_BYTES, PREFIX_BUDGET_BYTES + 1] {
            names.push("p".repeat(len));
        }
        // At the cap, one byte over, and well over.
        for len in [budget - 1, budget, budget + 1, budget + 2, 2 * budget, 200] {
            names.push("x".repeat(len));
        }
        // Non-ASCII, sized by BYTES: 21 chars is exactly the budget, 22 chars is
        // over it while still being far short of it by characters.
        for chars in [PREFIX_BUDGET_BYTES / 3, budget / 3, budget / 3 + 1, 40] {
            names.push(WIDE.repeat(chars));
        }
        // Prefixes that land ON and one byte SHORT of a character boundary: the
        // second must lose a byte rather than split the character.
        names.push(format!("{}{}", "x".repeat(PREFIX_BUDGET_BYTES), WIDE.repeat(8)));
        names.push(format!("{}{}", "x".repeat(PREFIX_BUDGET_BYTES - 1), WIDE.repeat(8)));
        // A prefix ending on `_`. NEITHER side strips it, so the hashed form
        // carries `__`; the engine's OTHER scheme
        // (`schema::query::index_name`) does strip it, and this arm is what
        // notices if this file ever picks that scheme up.
        names.push(format!("{}_{}", "a".repeat(PREFIX_BUDGET_BYTES - 1), "b".repeat(20)));
        // The shapes this crate derives, from a collection name on the ceiling
        // `query::validate_collection` accepts.
        let collection = "c".repeat(budget);
        names.push(format!("{collection}__alpha_masked_idx"));
        names.push(format!("{collection}__email_mask_idx"));
        names.push(format!("{collection}_email_key"));
        names.push(format!("{collection}_authorId_fkey"));
        names
    }

    /// The binding this module exists for: one input, one name, across the crate
    /// boundary.
    #[test]
    fn the_two_caps_agree_over_the_corpus() {
        let mut divergences = Vec::new();
        for natural in corpus() {
            let ours = cap_ident_name(&natural);
            let theirs = engine_cap(&natural);
            if ours != theirs {
                divergences.push(format!(
                    "{} byte(s) / {} char(s): runtime={ours:?}, engine={theirs:?}",
                    natural.len(),
                    natural.chars().count(),
                ));
            }
        }
        assert!(
            divergences.is_empty(),
            "{} derived identifier(s) diverged across the migration-engine boundary:\n{}",
            divergences.len(),
            divergences.join("\n"),
        );
    }

    /// The condition the corpus agreement rests on, held against its authority.
    ///
    /// The engine's budget is `min(identifier limit)` over the REGISTERED
    /// backends; this crate bakes [`PG_MAX_IDENT_BYTES`]. Register a backend with
    /// a tighter limit and the engine starts hashing names this crate still
    /// emits verbatim, with nothing else in the tree relating the two numbers.
    /// This arm reads the registry directly rather than calling the engine's
    /// `generated_ident_max_bytes`, which is `pub(crate)`.
    #[test]
    fn the_baked_budget_is_the_tightest_limit_the_shipping_backends_declare() {
        use zeroship_migrate::IdentifierLimit;

        let tightest = zeroship_migrate::shipping_vendors()
            .as_slice()
            .iter()
            .map(|vendor| match vendor.descriptor.limits.identifier {
                IdentifierLimit::Bytes(n) | IdentifierLimit::Characters(n) => n,
                IdentifierLimit::Unbounded => usize::MAX,
            })
            .min()
            .expect("the shipping vendor set is never empty");

        assert_eq!(
            tightest, IDENTIFIER_BUDGET_BYTES,
            "a registered backend declares a tighter identifier budget than this \
             crate's cap assumes",
        );
        assert_eq!(
            PG_MAX_IDENT_BYTES, IDENTIFIER_BUDGET_BYTES,
            "this crate's cap no longer bakes the budget the parity corpus states",
        );
    }

    /// The threshold is a COMPARISON, not a number: both sides must switch to
    /// the hashed form on the same input, not merely produce the same width.
    #[test]
    fn both_sides_switch_to_the_hashed_form_at_the_same_input_length() {
        for len in 0..=(2 * IDENTIFIER_BUDGET_BYTES) {
            let natural = "x".repeat(len);
            let ours_verbatim = cap_ident_name(&natural) == natural;
            let theirs_verbatim = engine_cap(&natural) == natural;
            assert_eq!(
                ours_verbatim, theirs_verbatim,
                "at {len} bytes: runtime kept verbatim={ours_verbatim}, \
                 engine kept verbatim={theirs_verbatim}",
            );
            assert_eq!(
                ours_verbatim,
                len <= IDENTIFIER_BUDGET_BYTES,
                "at {len} bytes both sides agreed on the wrong verdict",
            );
        }
    }

    /// The tail, isolated from the budget.
    ///
    /// A [`HASH_HEX_CHARS`] change keeps every output at 63 bytes, so the budget
    /// arms cannot see it. This one reads the tail's width and alphabet off each
    /// side's actual output.
    #[test]
    fn both_hash_tails_are_ten_lowercase_hex_characters() {
        let natural = "q".repeat(200);
        for (side, out) in [("runtime", cap_ident_name(&natural)), ("engine", engine_cap(&natural))]
        {
            assert_eq!(out.len(), IDENTIFIER_BUDGET_BYTES, "{side} output is not the full budget");
            let (head, tail) = out.split_at(out.len() - HASH_TAIL_HEX_CHARS);
            assert!(
                tail.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{side} tail {tail:?} is not lowercase hex",
            );
            assert!(head.ends_with('_'), "{side} output {out:?} has no `_` before its tail");
            assert_eq!(
                head.len() - 1,
                PREFIX_BUDGET_BYTES,
                "{side} readable prefix is not the budget less `_<tail>`",
            );
        }
        assert_eq!(HASH_HEX_CHARS, HASH_TAIL_HEX_CHARS, "this crate's tail width moved alone");
    }

    /// Bytes, not characters, on BOTH sides.
    ///
    /// 22 wide characters is 66 bytes: over the budget by bytes, far under it by
    /// characters. A side that switched to `chars().count()` would keep it
    /// verbatim. 21 wide characters is exactly 63 bytes, which both readings keep
    /// verbatim, so it is the control rather than the case.
    #[test]
    fn both_sides_measure_the_cap_in_bytes_not_characters() {
        let over = WIDE.repeat(IDENTIFIER_BUDGET_BYTES / 3 + 1);
        assert_eq!(over.len(), IDENTIFIER_BUDGET_BYTES + 3);
        assert!(over.chars().count() < IDENTIFIER_BUDGET_BYTES);
        assert_ne!(cap_ident_name(&over), over, "the runtime read the cap in characters");
        assert_ne!(engine_cap(&over), over, "the engine read the cap in characters");

        let control = WIDE.repeat(IDENTIFIER_BUDGET_BYTES / 3);
        assert_eq!(control.len(), IDENTIFIER_BUDGET_BYTES);
        assert_eq!(cap_ident_name(&control), control, "the runtime capped a name that fits");
        assert_eq!(engine_cap(&control), control, "the engine capped a name that fits");
    }
}
