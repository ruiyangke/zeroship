//! The knob model (II.2.1) — a policy knob declared once. *The declaration IS the
//! machine.* Every `Grant`/`Require` rule references a [`KnobDef`] by [`KnobKey`];
//! the def supplies the knob's kind, polarity, default, enforcement, and object
//! attribution. The registry (`crate::registry`) is the open set of these defs.
//!
//! This module is pure data + validation: a [`KnobKind`] describes the shape of a
//! legal value, [`KnobValue`] is a concrete value validated against its kind on
//! load, and [`KnobDef::canonical_encoding`] emits the stable bytes the registry
//! digest (II.2.1 / II.7) hashes.

use serde::{Deserialize, Serialize};

/// A namespaced knob key: `core.raw_sql`, `pg.extension`, `op.lock_timeout_ms`,
/// `acme.hypertable`. Exactly `namespace.name` — one dot, both parts non-empty,
/// lowercase-ASCII + `_` + digits. Blunt on purpose (a security identifier).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KnobKey(String);

/// A knob-key parse error (kept structured for the loader's diagnostics).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KnobKeyError {
    /// Not exactly `namespace.name` (zero or ≥2 dots, or an empty part).
    Malformed,
    /// A byte outside `[a-z0-9_]` in a segment.
    IllegalChar,
}

impl KnobKey {
    /// Parse + validate a namespaced key. `namespace.name`, one dot, each part
    /// non-empty and drawn from `[a-z0-9_]`.
    pub fn parse(s: &str) -> Result<Self, KnobKeyError> {
        let parts: Vec<&str> = s.split('.').collect();
        let [ns, name] = parts.as_slice() else {
            return Err(KnobKeyError::Malformed);
        };
        if ns.is_empty() || name.is_empty() {
            return Err(KnobKeyError::Malformed);
        }
        let ok = |seg: &str| seg.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !ok(ns) || !ok(name) {
            return Err(KnobKeyError::IllegalChar);
        }
        Ok(Self(s.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for KnobKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The shape of a legal value for a knob. A [`KnobValue`] is validated against its
/// knob's `KnobKind` at document load.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KnobKind {
    /// A boolean flag (grant on/off, obligation on/off).
    Bool,
    /// A set of strings (e.g. an allow-list of roles). Values: any string set.
    StrSet,
    /// A monotone ceiling: a `u64` with a hard floor the value may not go below
    /// (the engine's no-indefinite-lock invariant is `hard_floor: 1` — II.5).
    UintCeiling { hard_floor: u64 },
    /// A rank-ordered enum: the legal variants in tightest→loosest order (e.g.
    /// `["forbid", "warn", "allow"]`). A value must be one of the variants.
    OrderedEnum { variants: Vec<String> },
    /// An opaque content digest (hex string). Used for `Pinned` transform digests.
    Digest,
}

/// A concrete knob value, validated against its [`KnobKind`] on load.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KnobValue {
    /// `Bool` value.
    Bool(bool),
    /// `UintCeiling` value.
    Uint(u64),
    /// `StrSet` value (order-insensitive semantically; stored as authored).
    StrSet(Vec<String>),
    /// `OrderedEnum` value (one variant name) or a `Digest` (hex string).
    Str(String),
}

/// Why a [`KnobValue`] is not legal for a given [`KnobKind`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KnobValueError {
    /// The value's shape does not match the kind (e.g. a bool for a `UintCeiling`).
    TypeMismatch,
    /// A `UintCeiling` value below the knob's `hard_floor`.
    BelowHardFloor { value: u64, hard_floor: u64 },
    /// An `OrderedEnum` value naming a variant the kind does not declare.
    UnknownVariant,
}

impl KnobValue {
    /// Validate this value against `kind`. Called by the document loader for every
    /// authored `Grant`/`Require` value and for a [`KnobDef`]'s own `default`.
    pub fn validate_for(&self, kind: &KnobKind) -> Result<(), KnobValueError> {
        match (kind, self) {
            (KnobKind::Bool, KnobValue::Bool(_)) => Ok(()),
            (KnobKind::StrSet, KnobValue::StrSet(_)) => Ok(()),
            (KnobKind::UintCeiling { hard_floor }, KnobValue::Uint(v)) => {
                if v < hard_floor {
                    Err(KnobValueError::BelowHardFloor { value: *v, hard_floor: *hard_floor })
                } else {
                    Ok(())
                }
            }
            (KnobKind::OrderedEnum { variants }, KnobValue::Str(s)) => {
                if variants.iter().any(|v| v == s) {
                    Ok(())
                } else {
                    Err(KnobValueError::UnknownVariant)
                }
            }
            (KnobKind::Digest, KnobValue::Str(_)) => Ok(()),
            _ => Err(KnobValueError::TypeMismatch),
        }
    }
}

/// How a knob's values compose across policy layers (II.2.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Polarity {
    /// Grants tighten DOWNWARD: effective must satisfy `draft ⊑ ceiling`.
    Grant,
    /// Obligations tighten UPWARD: `effective = join(ceiling, draft)`; ceiling
    /// requirements can never be removed.
    Require,
    /// Opaque/pinned content: draft must equal ceiling unless the ceiling marks it
    /// delegable. Backs only the opaque `TableShapeTransform` digest (II.4.5).
    Pinned {
        /// Whether a draft may override the pinned value.
        delegable: bool,
    },
}

/// Whether a knob actually reaches the guard/executor, or is metadata only (II.6).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    /// The knob flows into the guard/executor and does what it says.
    Enforced,
    /// The knob is declared metadata only; setting it to a non-default value in a
    /// document composed toward an enforced path is rejected (II.6).
    DeclaredOnly,
}

/// The object granularity at which a knob's authority is meaningful (II.2.5).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectModel {
    /// Authority is per table; scopes may name schemas or tables.
    PerTable,
    /// Authority is per schema; a table-granular (two-segment) scope is illegal.
    PerSchema,
    /// Authority is database-global; a rule's scope MUST be the syntactic `All`
    /// token and is EXEMPT from the default-scope meet (II.2.4/II.2.5).
    Global,
}

/// One policy knob, declared once (II.2.1). The declaration is the machine: the
/// guard, the composer, the seal validator, and diagnostics all read this def.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct KnobDef {
    /// Namespaced key.
    pub key: KnobKey,
    /// The shape of a legal value.
    pub kind: KnobKind,
    /// How values compose across layers.
    pub polarity: Polarity,
    /// The deny/none value — ALWAYS the tightest, and itself valid for `kind`.
    pub default: KnobValue,
    /// Whether the knob reaches enforcement or is declared metadata.
    pub enforcement: Enforcement,
    /// How values attribute to database objects; governs legal scopes.
    pub object_model: ObjectModel,
    /// Whether granting this knob presupposes a matching DB-role privilege (drives
    /// the least-privilege backing check, II.10.5). Enforcement-affecting → part of
    /// the sealed registry digest.
    pub requires_db_privilege: bool,
    /// Human-facing documentation for the knob.
    pub docs: String,
}

impl KnobDef {
    /// The CANONICAL byte encoding of this def — the stable input the registry
    /// digest (II.2.1 / II.7) hashes. It covers EVERY enforcement-affecting field
    /// (key, kind, polarity, default, enforcement, object_model,
    /// requires_db_privilege) but NOT `docs` (prose, non-enforcing). The encoding
    /// is a length-prefixed, field-tagged byte stream so no two distinct defs can
    /// collide and no field boundary is ambiguous.
    ///
    /// Format: for each field, a 1-byte tag, then the field's self-delimiting bytes
    /// (integers big-endian fixed-width; strings/sets length-prefixed with a `u32`
    /// count + `u32`-length elements). Stable across runs and machine word size.
    #[must_use]
    pub fn canonical_encoding(&self) -> Vec<u8> {
        let mut b = Vec::new();

        // key
        b.push(0x01);
        write_str(&mut b, self.key.as_str());

        // kind (discriminant + payload)
        b.push(0x02);
        match &self.kind {
            KnobKind::Bool => b.push(0x00),
            KnobKind::StrSet => b.push(0x01),
            KnobKind::UintCeiling { hard_floor } => {
                b.push(0x02);
                b.extend_from_slice(&hard_floor.to_be_bytes());
            }
            KnobKind::OrderedEnum { variants } => {
                b.push(0x03);
                write_str_vec(&mut b, variants);
            }
            KnobKind::Digest => b.push(0x04),
        }

        // polarity
        b.push(0x03);
        match self.polarity {
            Polarity::Grant => b.push(0x00),
            Polarity::Require => b.push(0x01),
            Polarity::Pinned { delegable } => {
                b.push(0x02);
                b.push(u8::from(delegable));
            }
        }

        // default value (discriminant + payload)
        b.push(0x04);
        write_value(&mut b, &self.default);

        // enforcement
        b.push(0x05);
        b.push(match self.enforcement {
            Enforcement::Enforced => 0x00,
            Enforcement::DeclaredOnly => 0x01,
        });

        // object_model
        b.push(0x06);
        b.push(match self.object_model {
            ObjectModel::PerTable => 0x00,
            ObjectModel::PerSchema => 0x01,
            ObjectModel::Global => 0x02,
        });

        // requires_db_privilege
        b.push(0x07);
        b.push(u8::from(self.requires_db_privilege));

        b
    }
}

/// Length-prefixed string: `u32` byte-length (BE) + bytes.
fn write_str(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(&(s.len() as u32).to_be_bytes());
    b.extend_from_slice(s.as_bytes());
}

/// Count-prefixed vec of length-prefixed strings.
fn write_str_vec(b: &mut Vec<u8>, v: &[String]) {
    b.extend_from_slice(&(v.len() as u32).to_be_bytes());
    for s in v {
        write_str(b, s);
    }
}

/// Discriminant-tagged, self-delimiting encoding of a [`KnobValue`].
fn write_value(b: &mut Vec<u8>, v: &KnobValue) {
    match v {
        KnobValue::Bool(x) => {
            b.push(0x00);
            b.push(u8::from(*x));
        }
        KnobValue::Uint(x) => {
            b.push(0x01);
            b.extend_from_slice(&x.to_be_bytes());
        }
        KnobValue::StrSet(xs) => {
            b.push(0x02);
            write_str_vec(b, xs);
        }
        KnobValue::Str(s) => {
            b.push(0x03);
            write_str(b, s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_parse_accepts_namespaced() {
        assert!(KnobKey::parse("core.raw_sql").is_ok());
        assert!(KnobKey::parse("pg.extension").is_ok());
        assert!(KnobKey::parse("op.lock_timeout_ms").is_ok());
        assert!(KnobKey::parse("acme.hypertable").is_ok());
    }

    #[test]
    fn key_parse_rejects_bad() {
        assert_eq!(KnobKey::parse("nodot"), Err(KnobKeyError::Malformed));
        assert_eq!(KnobKey::parse("a.b.c"), Err(KnobKeyError::Malformed));
        assert_eq!(KnobKey::parse(".name"), Err(KnobKeyError::Malformed));
        assert_eq!(KnobKey::parse("ns."), Err(KnobKeyError::Malformed));
        assert_eq!(KnobKey::parse("Ns.name"), Err(KnobKeyError::IllegalChar));
        assert_eq!(KnobKey::parse("ns.Name"), Err(KnobKeyError::IllegalChar));
    }

    #[test]
    fn value_validates_against_kind() {
        assert!(KnobValue::Bool(true).validate_for(&KnobKind::Bool).is_ok());
        assert!(KnobValue::Uint(5).validate_for(&KnobKind::Bool).is_err());
        assert_eq!(
            KnobValue::Uint(0).validate_for(&KnobKind::UintCeiling { hard_floor: 1 }),
            Err(KnobValueError::BelowHardFloor { value: 0, hard_floor: 1 })
        );
        assert!(KnobValue::Uint(1).validate_for(&KnobKind::UintCeiling { hard_floor: 1 }).is_ok());
        let oe = KnobKind::OrderedEnum { variants: vec!["forbid".into(), "allow".into()] };
        assert!(KnobValue::Str("allow".into()).validate_for(&oe).is_ok());
        assert_eq!(KnobValue::Str("nope".into()).validate_for(&oe), Err(KnobValueError::UnknownVariant));
    }

    #[test]
    fn canonical_encoding_is_field_sensitive() {
        let base = KnobDef {
            key: KnobKey::parse("pg.extension").unwrap(),
            kind: KnobKind::Bool,
            polarity: Polarity::Grant,
            default: KnobValue::Bool(false),
            enforcement: Enforcement::Enforced,
            object_model: ObjectModel::Global,
            requires_db_privilege: true,
            docs: "one thing".into(),
        };
        // docs does NOT affect the encoding.
        let mut only_docs = base.clone();
        only_docs.docs = "totally different prose".into();
        assert_eq!(base.canonical_encoding(), only_docs.canonical_encoding());

        // requires_db_privilege DOES.
        let mut flipped = base.clone();
        flipped.requires_db_privilege = false;
        assert_ne!(base.canonical_encoding(), flipped.canonical_encoding());

        // object_model DOES.
        let mut om = base.clone();
        om.object_model = ObjectModel::PerTable;
        assert_ne!(base.canonical_encoding(), om.canonical_encoding());
    }
}
