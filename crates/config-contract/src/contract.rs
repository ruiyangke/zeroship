//! Cross-registry validation for canonical names and generated read sites.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use zeroship_core::config::{ConfigSpec, ReadSite, SourceKind};

/// A compiled configuration-contract violation.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ContractError {
    /// No declarations were extracted, so every set comparison would be vacuous.
    #[error("configuration contract extracted zero ConfigSpec declarations")]
    EmptySpecs,
    /// No read sites were linked, so a set-but-unread check could pass vacuously.
    #[error("configuration contract extracted zero ReadSite registrations")]
    EmptyReadSites,
    /// Two identities collapse to one global environment spelling.
    #[error("environment projection {projection} collides for {first} and {second}")]
    EnvCollision {
        /// Colliding projection.
        projection: String,
        /// First canonical identity.
        first: String,
        /// Second canonical identity.
        second: String,
    },
    /// Two identities collapse to one flag within one binary.
    #[error("flag projection --{projection} collides in {consumer} for {first} and {second}")]
    FlagCollision {
        /// Exact binary target.
        consumer: String,
        /// Colliding projection without leading dashes.
        projection: String,
        /// First canonical identity.
        first: String,
        /// Second canonical identity.
        second: String,
    },
    /// A declared source/consumer tuple has no linked reader.
    #[error("declared source has no read site: {canonical} for {consumer} via {kind:?}")]
    DeclaredButUnread {
        /// Canonical identity.
        canonical: String,
        /// Exact binary target.
        consumer: String,
        /// Declared source.
        kind: SourceKind,
    },
    /// A linked reader has no matching declaration for its exact consumer.
    #[error("read site has no declaration: {canonical} for {consumer} via {kind:?}")]
    UndeclaredReadSite {
        /// Canonical identity.
        canonical: String,
        /// Exact binary target.
        consumer: String,
        /// Read source.
        kind: SourceKind,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Tuple {
    canonical: String,
    consumer: String,
    source: SourceKind,
}

/// Validate projections and bidirectional declaration/read-site equality.
///
/// # Errors
///
/// Returns explicit anti-vacuity, collision, set-but-unread, and undeclared
/// reader errors. Every diagnostic contains names only, never values.
pub fn validate_contract(
    specs: &[ConfigSpec],
    read_sites: &[ReadSite],
) -> Result<(), Vec<ContractError>> {
    let mut errors = Vec::new();
    if specs.is_empty() {
        errors.push(ContractError::EmptySpecs);
    }
    if read_sites.is_empty() {
        errors.push(ContractError::EmptyReadSites);
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let mut env_names: BTreeMap<String, String> = BTreeMap::new();
    let mut flag_names: BTreeMap<(String, String), String> = BTreeMap::new();
    for spec in specs {
        let canonical = spec.canonical().as_str().to_owned();
        if let Some(env) = spec.env_name() {
            if let Some(first) = env_names.insert(env.clone(), canonical.clone()) {
                if first != canonical {
                    errors.push(ContractError::EnvCollision {
                        projection: env,
                        first,
                        second: canonical.clone(),
                    });
                }
            }
        }
        for consumer in spec.consumers() {
            if let Some(flag) = spec.flag_name(*consumer) {
                let key = (consumer.target().to_owned(), flag.clone());
                if let Some(first) = flag_names.insert(key, canonical.clone()) {
                    if first != canonical {
                        errors.push(ContractError::FlagCollision {
                            consumer: consumer.target().to_owned(),
                            projection: flag,
                            first,
                            second: canonical.clone(),
                        });
                    }
                }
            }
        }
    }

    let declared = specs
        .iter()
        .flat_map(|spec| {
            spec.consumers().iter().flat_map(move |consumer| {
                spec.sources().iter().map(move |source| Tuple {
                    canonical: spec.canonical().as_str().to_owned(),
                    consumer: consumer.target().to_owned(),
                    source: *source,
                })
            })
        })
        .collect::<BTreeSet<_>>();
    let read = read_sites
        .iter()
        .map(|site| Tuple {
            canonical: site.canonical().as_str().to_owned(),
            consumer: site.consumer().target().to_owned(),
            source: site.source(),
        })
        .collect::<BTreeSet<_>>();

    for missing in declared.difference(&read) {
        errors.push(ContractError::DeclaredButUnread {
            canonical: missing.canonical.clone(),
            consumer: missing.consumer.clone(),
            kind: missing.source,
        });
    }
    for extra in read.difference(&declared) {
        errors.push(ContractError::UndeclaredReadSite {
            canonical: extra.canonical.clone(),
            consumer: extra.consumer.clone(),
            kind: extra.source,
        });
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}
