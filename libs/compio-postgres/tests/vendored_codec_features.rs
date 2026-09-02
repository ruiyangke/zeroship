//! The vendored `postgres-types` fork must not carry a codec this crate cannot
//! reach.
//!
//! Every `with-*` feature the fork declares gates a codec module. A module
//! whose feature has no passthrough here is unreachable from the test suite --
//! no `cargo test -p compio-postgres --features ...` can compile it -- and its
//! dependency is absent from the workspace lockfile, so `--all-features` on the
//! fork cannot even resolve offline.
//!
//! That is not hypothetical. The fork carried a second copy of seven codecs for
//! older crate versions, and `cidr_02::to_sql` went on emitting the INET flag
//! for a CIDR value long after the identical bug had been repaired in
//! `cidr_03`. Nothing could have caught it: no test could name the type.
//!
//! So this asserts the two feature sets are EQUAL, not that one contains the
//! other. A passthrough here with no feature in the fork is equally broken --
//! it names a feature cargo will reject.

use std::collections::BTreeSet;

/// How a passthrough names a fork feature inside this crate's own manifest.
const PASSTHROUGH_PREFIX: &str = "postgres-types/";

/// `with-*` feature names declared in the fork's own `[features]` table.
fn fork_codec_features() -> BTreeSet<String> {
    let manifest = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/vendor/postgres-types/Cargo.toml"
    ))
    .expect("read the vendored postgres-types manifest");

    let mut features = BTreeSet::new();
    let mut in_features = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            // Section headers end the table; `[features]` starts it. Without
            // this, a `[dependencies.with-...]` key would be read as a feature.
            in_features = line == "[features]";
            continue;
        }
        if !in_features {
            continue;
        }
        if let Some(name) = line.split(" = ").next()
            && name.starts_with("with-")
        {
            features.insert(name.to_string());
        }
    }
    features
}

/// `with-*` features this crate forwards to the fork, taken from the
/// `postgres-types/<feature>` strings in its own feature table.
fn forwarded_codec_features() -> BTreeSet<String> {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("read the compio-postgres manifest");

    manifest
        .match_indices(PASSTHROUGH_PREFIX)
        .map(|(index, _)| {
            manifest[index + PASSTHROUGH_PREFIX.len()..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect::<String>()
        })
        .filter(|name| name.starts_with("with-"))
        .collect()
}

#[test]
fn every_vendored_codec_feature_is_reachable_from_this_crate() {
    let fork = fork_codec_features();
    let forwarded = forwarded_codec_features();

    // Both sets empty compares EQUAL, so a parser that silently matched
    // nothing would report success. This floor is anti-vacuity only: it sits
    // below the 11 codecs carried today so that dropping one does not trip it,
    // and far above what a broken parser returns.
    assert!(
        fork.len() >= 8,
        "parsed only {} `with-*` features from the fork manifest, so the \
         parser -- not the manifest -- is what this test measured: {fork:?}",
        fork.len()
    );

    let unreachable: Vec<_> = fork.difference(&forwarded).collect();
    assert!(
        unreachable.is_empty(),
        "the vendored fork declares {} codec feature(s) this crate does not \
         forward, so no test can compile them: {unreachable:?}. Either add a \
         passthrough to compio-postgres's [features], or delete the module and \
         its optional dependency from the fork.",
        unreachable.len()
    );

    // Second-line defence, and deliberately unproven by mutation: cargo's own
    // resolver rejects a passthrough naming a feature the fork does not
    // declare, so a tree that reaches this line cannot be in that state. It is
    // kept because it makes the invariant a set EQUALITY rather than a
    // one-way containment, which is what the assertion above reads as.
    let dangling: Vec<_> = forwarded.difference(&fork).collect();
    assert!(
        dangling.is_empty(),
        "this crate forwards {} feature(s) the vendored fork does not declare, \
         which cargo rejects at resolve time: {dangling:?}",
        dangling.len()
    );
}
