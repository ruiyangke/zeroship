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

/// How a codec feature is handed to the differential oracle.
const ORACLE_PREFIX: &str = "tokio-postgres/";

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

/// Every codec this crate exposes must also reach the differential oracle.
///
/// `tokio-postgres` is a dev-dependency for one reason: running it beside this
/// driver against the same server is the strongest check a port has. That only
/// works for a type BOTH sides can decode. A `with-*` feature forwarded to the
/// vendored fork but not to `tokio-postgres` leaves our codec with no oracle
/// at all, which is how `with-bit-vec-0_8` came to be the one codec no
/// differential could compare - `tokio-postgres` offers the feature, we simply
/// never handed it over.
///
/// Two ways of reaching the oracle count, because both are in use:
/// forwarding `tokio-postgres/<feature>` from this crate's own feature, and
/// naming the feature directly in the `[dev-dependencies]` entry, which is how
/// chrono and time are wired.
#[test]
fn every_codec_feature_reaches_the_differential_oracle() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("read the compio-postgres manifest");
    let dev_dependencies = manifest
        .split_once("[dev-dependencies]")
        .map_or("", |(_, tail)| tail);

    let mut blind = Vec::new();
    let mut ruled_on = 0usize;
    for feature in forwarded_codec_features() {
        ruled_on += 1;
        let forwarded_to_oracle = manifest.contains(&format!("{ORACLE_PREFIX}{feature}"));
        // The dev-dependency entry lists bare feature names, so match the
        // quoted form rather than a bare substring: `"with-time-0_3"` must not
        // be satisfied by some longer feature that merely contains it.
        let named_in_dev_dependency = dev_dependencies.contains(&format!("\"{feature}\""));
        if !forwarded_to_oracle && !named_in_dev_dependency {
            blind.push(feature);
        }
    }

    assert!(
        ruled_on >= 8,
        "only {ruled_on} codec features were examined, so the parser is what \
         this test measured"
    );
    assert!(
        blind.is_empty(),
        "{} codec feature(s) reach our fork but not the oracle, so no \
         differential can compare them: {blind:?}. Add \
         `tokio-postgres/<feature>` to this crate's feature, or name it in the \
         tokio-postgres dev-dependency.",
        blind.len()
    );
}
