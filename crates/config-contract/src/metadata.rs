//! Cargo-metadata anti-vacuity checks for workspace binary targets.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use cargo_metadata::{Metadata, MetadataCommand};
use serde::Deserialize;
use thiserror::Error;

/// A workspace binary discovered from Cargo's resolved target model.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BinaryTarget {
    /// Cargo package name.
    pub package: String,
    /// Cargo binary target name.
    pub target: String,
    /// Features required for Cargo to build this target.
    pub required_features: Vec<String>,
}

/// Why a binary participates in, or is excluded from, the server config contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetClass {
    /// A production platform process. Later conversion steps require a compiled registry.
    Platform,
    /// The creator CLI, deliberately outside the server overlay contract.
    CreatorCli,
    /// A non-shipped test, development, benchmark, mock, or checker tool.
    TestDevTool,
}

impl TargetClass {
    fn parse(raw: &str) -> Result<Self, MetadataContractError> {
        match raw {
            "platform" => Ok(Self::Platform),
            "creator-cli" => Ok(Self::CreatorCli),
            "test-dev-tool" => Ok(Self::TestDevTool),
            other => Err(MetadataContractError::UnknownClass(other.to_owned())),
        }
    }
}

/// A package-manifest classification for one binary target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetClassification {
    /// Cargo package name containing the target.
    pub package: String,
    /// Cargo binary target name.
    pub target: String,
    /// Contract participation class.
    pub class: TargetClass,
    /// Human-readable reason recorded next to the target definition.
    pub reason: String,
}

/// Counts produced by a successful workspace classification check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetadataSummary {
    /// Total workspace binary targets.
    pub binaries: usize,
    /// Production platform targets that later steps must register.
    pub platform: usize,
    /// Explicitly classified creator or non-shipped targets.
    pub excluded: usize,
}

/// A failure in workspace-target extraction or classification.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MetadataContractError {
    /// Target extraction returned nothing, so a green classification would be vacuous.
    #[error("cargo metadata extraction found zero workspace binary targets")]
    EmptyBinaryExtraction,
    /// No production target was present in the extracted inventory.
    #[error("cargo metadata extraction found zero platform targets")]
    EmptyPlatformExtraction,
    /// A binary target has no package metadata record.
    #[error("binary target {package}/{target} has no zeroship-config classification")]
    MissingClassification {
        /// Package name.
        package: String,
        /// Target name.
        target: String,
    },
    /// More than one record attempted to classify the same target.
    #[error("binary target {package}/{target} has duplicate zeroship-config classifications")]
    DuplicateClassification {
        /// Package name.
        package: String,
        /// Target name.
        target: String,
    },
    /// Metadata names a target Cargo did not discover as a binary in that package.
    #[error("zeroship-config classification names stale target {package}/{target}")]
    StaleClassification {
        /// Package name.
        package: String,
        /// Target name.
        target: String,
    },
    /// The class is not one of the deliberately small Step 1 set.
    #[error("unknown zeroship-config target class {0:?}")]
    UnknownClass(String),
    /// A classification without an explanation can silently become an exemption.
    #[error("binary target {package}/{target} has a blank classification reason")]
    BlankReason {
        /// Package name.
        package: String,
        /// Target name.
        target: String,
    },
    /// Package metadata had the wrong JSON shape.
    #[error("invalid zeroship-config metadata for package {package}: {message}")]
    InvalidMetadata {
        /// Package name.
        package: String,
        /// Serde diagnostic. It contains manifest structure, never config values.
        message: String,
    },
    /// Cargo itself could not produce metadata.
    #[error("cargo metadata failed: {0}")]
    Cargo(String),
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawPackageMetadata {
    targets: Vec<RawTargetClassification>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTargetClassification {
    target: String,
    class: String,
    reason: String,
}

/// Run `cargo metadata --no-deps` and validate every workspace binary target.
///
/// # Errors
///
/// Returns Cargo execution errors, malformed package metadata, or any
/// anti-vacuity/classification error.
pub fn check_workspace(manifest_path: &Path) -> Result<MetadataSummary, Vec<MetadataContractError>> {
    let metadata = MetadataCommand::new()
        .manifest_path(manifest_path)
        .no_deps()
        .exec()
        .map_err(|error| vec![MetadataContractError::Cargo(error.to_string())])?;
    check_metadata(&metadata)
}

/// Validate an already extracted Cargo metadata model.
///
/// This split is intentional: mutation fixtures exercise the checker without
/// depending on Cargo process behavior.
///
/// # Errors
///
/// Returns every classification error found in the model.
pub fn check_metadata(metadata: &Metadata) -> Result<MetadataSummary, Vec<MetadataContractError>> {
    let workspace_ids = metadata
        .workspace_members
        .iter()
        .map(|id| id.repr.as_str())
        .collect::<BTreeSet<_>>();
    let mut binaries = Vec::new();
    let mut classifications = Vec::new();
    let mut extraction_errors = Vec::new();

    for package in metadata
        .packages
        .iter()
        .filter(|package| workspace_ids.contains(package.id.repr.as_str()))
    {
        for target in package.targets.iter().filter(|target| target.is_bin()) {
            binaries.push(BinaryTarget {
                package: package.name.to_string(),
                target: target.name.clone(),
                required_features: target.required_features.clone(),
            });
        }

        let Some(raw_config) = package.metadata.get("zeroship-config") else {
            continue;
        };
        let parsed = serde_json::from_value::<RawPackageMetadata>(raw_config.clone()).map_err(
            |error| MetadataContractError::InvalidMetadata {
                package: package.name.to_string(),
                message: error.to_string(),
            },
        );
        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                extraction_errors.push(error);
                continue;
            }
        };
        for record in parsed.targets {
            match TargetClass::parse(&record.class) {
                Ok(class) => classifications.push(TargetClassification {
                    package: package.name.to_string(),
                    target: record.target,
                    class,
                    reason: record.reason,
                }),
                Err(error) => extraction_errors.push(error),
            }
        }
    }

    if !extraction_errors.is_empty() {
        return Err(extraction_errors);
    }
    validate_target_classifications(&binaries, &classifications)
}

/// Compare extracted binary targets with package-owned classifications.
///
/// # Errors
///
/// Returns an explicit empty-extraction error or all missing, duplicate, stale,
/// blank-reason, and empty-platform errors.
pub fn validate_target_classifications(
    binaries: &[BinaryTarget],
    classifications: &[TargetClassification],
) -> Result<MetadataSummary, Vec<MetadataContractError>> {
    if binaries.is_empty() {
        return Err(vec![MetadataContractError::EmptyBinaryExtraction]);
    }

    let binary_keys = binaries
        .iter()
        .map(|binary| (binary.package.as_str(), binary.target.as_str()))
        .collect::<BTreeSet<_>>();
    let mut by_key: BTreeMap<(&str, &str), Vec<&TargetClassification>> = BTreeMap::new();
    for classification in classifications {
        by_key
            .entry((classification.package.as_str(), classification.target.as_str()))
            .or_default()
            .push(classification);
    }

    let mut errors = Vec::new();
    for binary in binaries {
        let key = (binary.package.as_str(), binary.target.as_str());
        match by_key.get(&key).map(Vec::as_slice) {
            None | Some([]) => errors.push(MetadataContractError::MissingClassification {
                package: binary.package.clone(),
                target: binary.target.clone(),
            }),
            Some([classification]) => {
                if classification.reason.trim().is_empty() {
                    errors.push(MetadataContractError::BlankReason {
                        package: binary.package.clone(),
                        target: binary.target.clone(),
                    });
                }
            }
            Some(_) => errors.push(MetadataContractError::DuplicateClassification {
                package: binary.package.clone(),
                target: binary.target.clone(),
            }),
        }
    }
    for classification in classifications {
        let key = (classification.package.as_str(), classification.target.as_str());
        if !binary_keys.contains(&key) {
            errors.push(MetadataContractError::StaleClassification {
                package: classification.package.clone(),
                target: classification.target.clone(),
            });
        }
    }

    let platform = classifications
        .iter()
        .filter(|classification| {
            classification.class == TargetClass::Platform
                && binary_keys.contains(&(
                    classification.package.as_str(),
                    classification.target.as_str(),
                ))
        })
        .count();
    if platform == 0 {
        errors.push(MetadataContractError::EmptyPlatformExtraction);
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    Ok(MetadataSummary {
        binaries: binaries.len(),
        platform,
        excluded: binaries.len() - platform,
    })
}
