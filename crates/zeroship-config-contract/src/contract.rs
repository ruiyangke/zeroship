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
    /// Two consumers of one canonical identity disagree about what it IS.
    ///
    /// The `default` is deliberately not one of the compared properties; see
    /// [`validate_contract`].
    #[error(
        "consumers of {canonical} disagree about {property}: {first} declares \
         {first_value}, {second} declares {second_value}"
    )]
    SharedIdentityDisagreement {
        /// Canonical identity declared more than once.
        canonical: String,
        /// The property that differs, such as `class` or `type`.
        property: &'static str,
        /// First consumer, in sorted order.
        first: String,
        /// First consumer's value.
        first_value: String,
        /// Second consumer.
        second: String,
        /// Second consumer's value.
        second_value: String,
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

/// Compare the properties on which consumers of one identity must agree.
///
/// The DEFAULT is not among them, and its absence is the point:
/// `observability.log_filter` defaults to a directive naming the declaring
/// crate, so requiring agreement would reject a correct configuration with no
/// conforming alternative.
///
/// Everything compared here is something an OPERATOR would be misled by: one
/// `ZEROSHIP_*` spelling and one overlay path must mean one class, one type and
/// one supply set. The default is invisible from outside the process.
fn shared_properties(spec: &ConfigSpec) -> [(&'static str, String); 4] {
    [
        ("class", format!("{:?}", spec.class())),
        ("type", spec.rust_type().to_owned()),
        (
            "environment source",
            spec.env_name().unwrap_or_else(|| "none".to_owned()),
        ),
        ("supply set", format!("{:?}", spec.sources())),
    ]
}

/// Validate projections, shared-identity agreement, and bidirectional
/// declaration/read-site equality.
///
/// # Errors
///
/// Returns explicit anti-vacuity, collision, shared-identity, set-but-unread,
/// and undeclared reader errors. Every diagnostic contains names only, never
/// values.
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
        if let Some(env) = spec.env_name()
            && let Some(first) = env_names.insert(env.clone(), canonical.clone())
            && first != canonical
        {
            errors.push(ContractError::EnvCollision {
                projection: env,
                first,
                second: canonical.clone(),
            });
        }
        for consumer in spec.consumers() {
            if let Some(flag) = spec.flag_name(*consumer) {
                let key = (consumer.target().to_owned(), flag.clone());
                if let Some(first) = flag_names.insert(key, canonical.clone())
                    && first != canonical
                {
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

    // Group by canonical identity. The declaration attribute already makes the
    // NAME, class and type of a shared identity unforgeable by reading them
    // from one table, so this is defence in depth for a hand-written
    // `ConfigSpec` and for whatever the registry grows next.
    let mut by_identity: BTreeMap<&str, Vec<&ConfigSpec>> = BTreeMap::new();
    for spec in specs {
        by_identity
            .entry(spec.canonical().as_str())
            .or_default()
            .push(spec);
    }
    for (canonical, group) in &by_identity {
        let Some((first, rest)) = group.split_first() else {
            continue;
        };
        let first_properties = shared_properties(first);
        let first_consumer = first
            .consumers()
            .first()
            .map_or("<none>", |consumer| consumer.target())
            .to_owned();
        for spec in rest {
            let second_consumer = spec
                .consumers()
                .first()
                .map_or("<none>", |consumer| consumer.target())
                .to_owned();
            for (expected, actual) in first_properties.iter().zip(shared_properties(spec)) {
                if expected.1 != actual.1 {
                    errors.push(ContractError::SharedIdentityDisagreement {
                        canonical: (*canonical).to_owned(),
                        property: expected.0,
                        first: first_consumer.clone(),
                        first_value: expected.1.clone(),
                        second: second_consumer.clone(),
                        second_value: actual.1,
                    });
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
