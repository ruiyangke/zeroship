//! The policy registry (II.2.1) — an OPEN set of [`KnobDef`]s: the engine builtins
//! plus consumer extensions registered at engine construction. Every `Grant`/
//! `Require` rule references a knob here by [`KnobKey`]; a document key unknown to
//! the registry is a hard load error (`deny_unknown_fields` against a
//! runtime-extensible known set).
//!
//! The registry has a canonical [`digest`](PolicyRegistry::digest): a stable
//! sha256 over the [`KnobDef::canonical_encoding`] of every knob, sorted by key so
//! the digest is insertion-order-independent. The digest binds into the seal (II.7)
//! — a `key` string means nothing without the whole def it resolves to.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::knob::{KnobDef, KnobKey};

/// An open registry of knob definitions, keyed by [`KnobKey`]. Insertion-order
/// independent: lookups and the digest both go through the sorted `BTreeMap`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PolicyRegistry {
    defs: BTreeMap<KnobKey, KnobDef>,
}

/// A registry construction error.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RegistryError {
    /// Two defs registered under the same key.
    DuplicateKey { key: KnobKey },
}

impl PolicyRegistry {
    /// An empty registry (no knobs). The engine's builtins are added by the
    /// consumer via [`with`](PolicyRegistry::with); this leaf crate ships no
    /// content — only the mechanism.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            defs: BTreeMap::new(),
        }
    }

    /// Extend the registry with additional knob defs (engine builtins or consumer
    /// extensions). A duplicate key is a hard error — one def per key.
    pub fn with(mut self, defs: impl IntoIterator<Item = KnobDef>) -> Result<Self, RegistryError> {
        for def in defs {
            if self.defs.contains_key(&def.key) {
                return Err(RegistryError::DuplicateKey { key: def.key });
            }
            self.defs.insert(def.key.clone(), def);
        }
        Ok(self)
    }

    /// Look up a def by key.
    #[must_use]
    pub fn get(&self, key: &KnobKey) -> Option<&KnobDef> {
        self.defs.get(key)
    }

    /// Whether a key is known to the registry.
    #[must_use]
    pub fn contains(&self, key: &KnobKey) -> bool {
        self.defs.contains_key(key)
    }

    /// The number of registered knobs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Iterate defs in key-sorted order.
    pub fn iter(&self) -> impl Iterator<Item = &KnobDef> {
        self.defs.values()
    }

    /// The canonical registry digest (II.2.1 / II.7): sha256 over the concatenated
    /// canonical encodings of every def, in key-sorted order, each length-prefixed
    /// so two adjacent encodings can never be confused for one. Insertion-order
    /// independent (the `BTreeMap` yields sorted keys) and stable across runs.
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        // Domain separator + count, so the empty registry has a well-defined digest
        // distinct from other structures hashing raw def bytes.
        hasher.update(b"zero-migrate-policy/registry/v1");
        hasher.update((self.defs.len() as u32).to_be_bytes());
        for def in self.defs.values() {
            let enc = def.canonical_encoding();
            hasher.update((enc.len() as u32).to_be_bytes());
            hasher.update(&enc);
        }
        hasher.finalize().into()
    }

    /// The digest as a lowercase hex string (for seals / diagnostics).
    #[must_use]
    pub fn digest_hex(&self) -> String {
        hex::encode(self.digest())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knob::{Enforcement, KnobKind, KnobValue, ObjectModel, Polarity};

    fn def(key: &str, priv_backed: bool, om: ObjectModel) -> KnobDef {
        KnobDef {
            key: KnobKey::parse(key).unwrap(),
            kind: KnobKind::Bool,
            polarity: Polarity::Grant,
            default: KnobValue::Bool(false),
            enforcement: Enforcement::Enforced,
            object_model: om,
            requires_db_privilege: priv_backed,
            inherit: true,
            docs: String::new(),
        }
    }

    #[test]
    fn digest_is_insertion_order_independent() {
        let a = def("core.raw_sql", false, ObjectModel::PerTable);
        let b = def("pg.extension", true, ObjectModel::Global);
        let c = def("core.create_table", false, ObjectModel::PerTable);

        let r1 = PolicyRegistry::empty()
            .with([a.clone(), b.clone(), c.clone()])
            .unwrap();
        let r2 = PolicyRegistry::empty().with([c, a, b]).unwrap();
        assert_eq!(r1.digest(), r2.digest());
    }

    #[test]
    fn digest_changes_on_requires_db_privilege() {
        let r1 = PolicyRegistry::empty()
            .with([def("pg.extension", true, ObjectModel::Global)])
            .unwrap();
        let r2 = PolicyRegistry::empty()
            .with([def("pg.extension", false, ObjectModel::Global)])
            .unwrap();
        assert_ne!(r1.digest(), r2.digest());
    }

    #[test]
    fn digest_changes_on_object_model() {
        let r1 = PolicyRegistry::empty()
            .with([def("core.raw_sql", false, ObjectModel::PerTable)])
            .unwrap();
        let r2 = PolicyRegistry::empty()
            .with([def("core.raw_sql", false, ObjectModel::PerSchema)])
            .unwrap();
        assert_ne!(r1.digest(), r2.digest());
    }

    #[test]
    fn duplicate_key_is_error() {
        let e = PolicyRegistry::empty()
            .with([def("core.raw_sql", false, ObjectModel::PerTable)])
            .unwrap()
            .with([def("core.raw_sql", true, ObjectModel::PerTable)]);
        assert!(matches!(e, Err(RegistryError::DuplicateKey { .. })));
    }

    #[test]
    fn empty_registry_has_stable_digest() {
        assert_eq!(
            PolicyRegistry::empty().digest(),
            PolicyRegistry::empty().digest()
        );
    }
}
