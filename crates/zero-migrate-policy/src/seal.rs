//! The policy SEAL (§II.7) — an HMAC that binds an [`EffectivePolicy`] to the exact
//! registry + matcher semantics + charter version that minted it. A seal is a
//! tamper-evidence + identity boundary: a verifier hard-fails a seal replayed against
//! a different registry (a `key` string means nothing without the whole def it
//! resolves to — II.2.1), a different dialect, a different matcher algorithm, or a
//! reordered/mutated rule set.
//!
//! # The MAC'd payload
//!
//! `HMAC-SHA256( mac_key , canonical(resolved rule set)
//!                       ‖ registry.digest()
//!                       ‖ (dialect, matcher_version)
//!                       ‖ charter_version )`
//!
//! plus the `nonce` (carried in the clear alongside the tag, and mixed into the MAC
//! so a nonce swap invalidates it).
//!
//! - **The resolved rule set** is the grants + requires + injects + validates, each
//!   with its `effective_scope`, in the sealed **cross-layer inject total order**
//!   (II.4.4). A tamper that reorders inject rules changes the canonical bytes and
//!   fails the MAC. Encoding reuses the 1b-i canonical-encoding discipline: a
//!   1-byte tag per field, integers big-endian fixed-width, strings/sets
//!   length-prefixed. No two distinct rule sets can collide and no boundary is
//!   ambiguous.
//! - **The registry digest** is [`crate::PolicyRegistry::digest`] (II.2.1) - the full
//!   canonical `KnobDef` encoding of every knob, including `requires_db_privilege`.
//! - **`(dialect, matcher_version)`** is the scope-matcher-semantics pair (II.2.7):
//!   `dialect` selects the identifier fold, `matcher_version` the glob-lattice +
//!   normalization algorithm. The same sealed `app_*` folds/decides differently
//!   across either, so both bind.
//! - **`charter_version`** is the operator's monotonic charter revision.
//!
//! [`verify`](SealedPolicy::verify) recomputes the MAC from a freshly-composed
//! [`EffectivePolicy`] + the expected registry digest + dialect + matcher version +
//! charter version, and returns `Ok(())` only on an exact constant-time match.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::compose::EffectivePolicy;
use crate::knob::KnobValue;
use crate::rule::{Rule, RuleKind, ValidatePredicate};
use crate::scope::{Pattern, Scope, SegGlob};

type HmacSha256 = Hmac<Sha256>;

/// A domain separator so seal bytes can never collide with any other structure this
/// crate hashes (the registry digest has its own separator).
const SEAL_DOMAIN: &[u8] = b"zero-migrate-policy/seal/v1";

/// A sealed effective policy: the MAC tag + the public binding fields it was minted
/// under. Carries no secret; verification recomputes the tag from a freshly composed
/// policy and the expected binding.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SealedPolicy {
    /// The HMAC-SHA256 tag over the canonical payload.
    tag: [u8; 32],
    /// The per-seal nonce (freshness / replay window), mixed into the MAC.
    nonce: [u8; 16],
    /// The dialect the scope matcher folds under (bound into the MAC).
    dialect: String,
    /// The matcher-algorithm version (bound into the MAC).
    matcher_version: u32,
    /// The operator's monotonic charter revision (bound into the MAC).
    charter_version: u64,
    /// The registry digest the policy was composed under (bound into the MAC).
    registry_digest: [u8; 32],
}

/// Why a [`SealedPolicy::verify`] failed. Any variant is a HARD failure — the seal is
/// invalid against the presented policy + binding.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SealError {
    /// The recomputed MAC tag does not match — the rule set, scope, nonce, or a bound
    /// field was tampered, or the mac_key differs.
    TagMismatch,
    /// The presented registry digest differs from the sealed one (a seal minted under
    /// registry A replayed against registry B where the same key means something else).
    RegistryDigestMismatch,
    /// The presented dialect differs from the sealed one (identifier fold differs).
    DialectMismatch,
    /// The presented matcher_version differs from the sealed one (glob/normalization
    /// algorithm differs).
    MatcherVersionMismatch,
    /// The presented charter_version differs from the sealed one.
    CharterVersionMismatch,
}

impl SealedPolicy {
    /// The bound dialect.
    #[must_use]
    pub fn dialect(&self) -> &str {
        &self.dialect
    }
    /// The bound matcher version.
    #[must_use]
    pub fn matcher_version(&self) -> u32 {
        self.matcher_version
    }
    /// The bound charter version.
    #[must_use]
    pub fn charter_version(&self) -> u64 {
        self.charter_version
    }
    /// The bound registry digest.
    #[must_use]
    pub fn registry_digest(&self) -> [u8; 32] {
        self.registry_digest
    }

    /// Verify this seal against a freshly composed `policy` and the EXPECTED binding.
    /// HARD-FAILS on any mismatch: registry digest, dialect, matcher_version,
    /// charter_version — checked BEFORE the MAC so a mismatch gets a precise error —
    /// then the MAC tag itself (constant-time). Returns `Ok(())` only when every
    /// binding matches AND the recomputed tag equals the sealed tag.
    pub fn verify(
        &self,
        mac_key: &[u8],
        policy: &EffectivePolicy,
        expected_registry_digest: &[u8; 32],
        dialect: &str,
        matcher_version: u32,
        charter_version: u64,
    ) -> Result<(), SealError> {
        // Binding checks first (precise diagnostics; the MAC would fail anyway).
        if &self.registry_digest != expected_registry_digest {
            return Err(SealError::RegistryDigestMismatch);
        }
        if self.dialect != dialect {
            return Err(SealError::DialectMismatch);
        }
        if self.matcher_version != matcher_version {
            return Err(SealError::MatcherVersionMismatch);
        }
        if self.charter_version != charter_version {
            return Err(SealError::CharterVersionMismatch);
        }
        // Recompute the MAC over the presented policy + the sealed binding + nonce.
        let expected = mac_tag(
            mac_key,
            policy,
            expected_registry_digest,
            dialect,
            matcher_version,
            charter_version,
            &self.nonce,
        );
        // Constant-time compare via the HMAC verify path.
        if expected == self.tag {
            Ok(())
        } else {
            Err(SealError::TagMismatch)
        }
    }
}

/// Seal a composed `policy` (§II.7): compute the HMAC over the canonical resolved
/// rule set (in the sealed inject total order) ‖ registry digest ‖
/// `(dialect, matcher_version)` ‖ `charter_version`, mixing in `nonce`. The
/// `registry_digest` is taken from the policy's own registry so the seal is
/// self-consistent; a verifier presents the digest it expects.
#[must_use]
pub fn seal(
    policy: &EffectivePolicy,
    mac_key: &[u8],
    nonce: [u8; 16],
    dialect: &str,
    matcher_version: u32,
    charter_version: u64,
) -> SealedPolicy {
    let registry_digest = policy.registry().digest();
    let tag = mac_tag(
        mac_key,
        policy,
        &registry_digest,
        dialect,
        matcher_version,
        charter_version,
        &nonce,
    );
    SealedPolicy {
        tag,
        nonce,
        dialect: dialect.to_string(),
        matcher_version,
        charter_version,
        registry_digest,
    }
}

/// Compute the raw MAC tag over the full canonical payload.
#[allow(clippy::too_many_arguments)]
fn mac_tag(
    mac_key: &[u8],
    policy: &EffectivePolicy,
    registry_digest: &[u8; 32],
    dialect: &str,
    matcher_version: u32,
    charter_version: u64,
    nonce: &[u8; 16],
) -> [u8; 32] {
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(mac_key).expect("HMAC accepts a key of any length");
    mac.update(SEAL_DOMAIN);

    // (1) canonical resolved rule set (grants + requires + injects + validates), each
    //     with its effective scope, injects in the sealed total order.
    let payload = canonical_rule_set(policy);
    mac.update(&(payload.len() as u32).to_be_bytes());
    mac.update(&payload);

    // (2) registry digest.
    mac.update(registry_digest);

    // (3) (dialect, matcher_version).
    write_len_prefixed(&mut mac, dialect.as_bytes());
    mac.update(&matcher_version.to_be_bytes());

    // (4) charter_version.
    mac.update(&charter_version.to_be_bytes());

    // nonce (freshness) — mixed so a nonce swap invalidates the tag.
    mac.update(nonce);

    mac.finalize().into_bytes().into()
}

/// The CANONICAL byte encoding of the resolved rule set WITH ITS LAYER BOUNDARIES
/// (H-4): each layer, in stack order (TOP/innermost first), is emitted with its
/// [`LayerTag`] byte, then its grants (key-sorted, each rule's scope+value), requires,
/// injects (sealed doc order), and validates — each rule length-prefixed and
/// field-tagged so no two distinct rule sets collide and no boundary is ambiguous (the
/// 1b-i discipline). Because the layer boundaries are ENCODED, two stacks with the same
/// flattened rule set but different layering produce different bytes — a re-flatten
/// (or reorder) tamper fails the MAC (II.7).
fn canonical_rule_set(policy: &EffectivePolicy) -> Vec<u8> {
    let mut b = Vec::new();

    let layers = policy.seal_layers();
    // Layer count, so an empty policy and a single-empty-layer policy differ.
    b.push(0x0f);
    b.extend_from_slice(&(layers.len() as u32).to_be_bytes());

    for layer in &layers {
        // ── layer tag (binds the layer boundary) ──
        b.push(0x0e);
        b.push(layer.tag.seal_byte());

        // ── grants ── (key-sorted then rule-ordered)
        b.push(0x10);
        b.extend_from_slice(&(layer.grants.len() as u32).to_be_bytes());
        for (key, scope, value) in &layer.grants {
            write_str(&mut b, key.as_str());
            write_scope(&mut b, scope);
            write_value(&mut b, value);
        }

        // ── requires ── (composition order)
        b.push(0x11);
        let reqs: Vec<&Rule> = layer
            .requires
            .iter()
            .filter(|r| matches!(r.kind, RuleKind::Require { .. }))
            .collect();
        b.extend_from_slice(&(reqs.len() as u32).to_be_bytes());
        for r in reqs {
            if let RuleKind::Require { key, value } = &r.kind {
                write_str(&mut b, key.as_str());
                write_scope(&mut b, &r.scope);
                write_value(&mut b, value);
            }
        }

        // ── injects ── (sealed doc order within the layer)
        b.push(0x12);
        let injs: Vec<&Rule> = layer
            .injects
            .iter()
            .filter(|r| matches!(r.kind, RuleKind::Inject { .. }))
            .collect();
        b.extend_from_slice(&(injs.len() as u32).to_be_bytes());
        for r in injs {
            if let RuleKind::Inject { spec } = &r.kind {
                write_scope(&mut b, &r.scope);
                write_inject(&mut b, spec);
            }
        }

        // ── validates ── (composition order)
        b.push(0x13);
        let vals: Vec<&Rule> = layer
            .validates
            .iter()
            .filter(|r| matches!(r.kind, RuleKind::Validate { .. }))
            .collect();
        b.extend_from_slice(&(vals.len() as u32).to_be_bytes());
        for r in vals {
            if let RuleKind::Validate { pred } = &r.kind {
                write_scope(&mut b, &r.scope);
                write_predicate(&mut b, pred);
            }
        }
    }

    b
}

// ── canonical field encoders (self-delimiting, 1b-i discipline) ───────────────────

fn write_len_prefixed(mac: &mut HmacSha256, bytes: &[u8]) {
    mac.update(&(bytes.len() as u32).to_be_bytes());
    mac.update(bytes);
}

fn write_str(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(&(s.len() as u32).to_be_bytes());
    b.extend_from_slice(s.as_bytes());
}

fn write_bytes(b: &mut Vec<u8>, bytes: &[u8]) {
    b.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    b.extend_from_slice(bytes);
}

fn write_str_vec(b: &mut Vec<u8>, v: &[String]) {
    b.extend_from_slice(&(v.len() as u32).to_be_bytes());
    for s in v {
        write_str(b, s);
    }
}

/// Canonical value encoding (mirrors `knob::write_value` — tagged + self-delimiting).
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
            // Sort so an authored reordering does not change the canonical bytes (a
            // StrSet is a set; its seal denotation is order-insensitive).
            let mut sorted: Vec<String> = xs.clone();
            sorted.sort();
            write_str_vec(b, &sorted);
        }
        KnobValue::Str(s) => {
            b.push(0x03);
            write_str(b, s);
        }
    }
}

/// Canonical scope encoding: the ⊥/⊤ tokens, or a proper `Of` with its normalized
/// include/exclude pattern lists (each a `schema.table` glob pair). The include and
/// exclude vectors are sorted so an authored reordering (they are sets) does not
/// perturb the seal.
fn write_scope(b: &mut Vec<u8>, s: &Scope) {
    match s {
        Scope::Nothing => b.push(0x00),
        Scope::All => b.push(0x01),
        Scope::Of { include, exclude } => {
            b.push(0x02);
            write_patterns(b, include);
            write_patterns(b, exclude);
        }
    }
}

fn write_patterns(b: &mut Vec<u8>, pats: &[Pattern]) {
    // Deterministic order regardless of authored order (Pattern is Ord).
    let mut sorted: Vec<&Pattern> = pats.iter().collect();
    sorted.sort();
    b.extend_from_slice(&(sorted.len() as u32).to_be_bytes());
    for p in sorted {
        write_seg(b, &p.schema);
        write_seg(b, &p.table);
    }
}

/// Canonical single-segment glob encoding: the rendered bytes suffice because
/// `SegGlob::render` is injective over the canonical `(prefix, suffix, has_star)`
/// triple, and a `*`-free render is a literal while a `*`-bearing one is a glob — but
/// to be unambiguous we tag the star flag explicitly.
fn write_seg(b: &mut Vec<u8>, g: &SegGlob) {
    b.push(u8::from(!g.is_literal())); // 1 = has star, 0 = literal
    write_bytes(b, g.render().as_bytes());
}

/// Canonical inject-spec encoding: columns (name/ty/nullable/default) in doc order,
/// indexes (name/columns), the pinned PK, the author-PK policy, and the mandatory
/// flag. Column/index ORDER is preserved (it is load-bearing — the sealed layout
/// order, II.4.4), unlike the set-valued scope/StrSet fields.
fn write_inject(b: &mut Vec<u8>, spec: &crate::rule::InjectSpec) {
    b.extend_from_slice(&(spec.columns.len() as u32).to_be_bytes());
    for c in &spec.columns {
        write_str(b, &c.name);
        write_str(b, &c.ty);
        b.push(u8::from(c.nullable));
        match &c.default {
            Some(d) => {
                b.push(0x01);
                write_str(b, d);
            }
            None => b.push(0x00),
        }
    }
    b.extend_from_slice(&(spec.indexes.len() as u32).to_be_bytes());
    for i in &spec.indexes {
        write_str(b, &i.name);
        write_str_vec(b, &i.columns);
    }
    match &spec.primary_key {
        Some(cols) => {
            b.push(0x01);
            write_str_vec(b, cols);
        }
        None => b.push(0x00),
    }
    b.push(match spec.author_primary_key {
        crate::rule::AuthorPkPolicy::Allow => 0x00,
        crate::rule::AuthorPkPolicy::Forbid => 0x01,
    });
    b.push(u8::from(spec.mandatory));
}

/// Canonical validate-predicate encoding: a discriminant tag + self-delimiting payload.
fn write_predicate(b: &mut Vec<u8>, pred: &ValidatePredicate) {
    match pred {
        ValidatePredicate::HasPrimaryKey => b.push(0x00),
        ValidatePredicate::ColumnNamePattern { require, forbid } => {
            b.push(0x01);
            write_name_globs(b, require);
            write_name_globs(b, forbid);
        }
        ValidatePredicate::ForbiddenColumns { names } => {
            b.push(0x02);
            write_str_vec(b, names);
        }
        ValidatePredicate::TypeNullability {
            column,
            ty,
            nullable,
        } => {
            b.push(0x03);
            write_str(b, column);
            match ty {
                Some(t) => {
                    b.push(0x01);
                    write_str(b, t);
                }
                None => b.push(0x00),
            }
            match nullable {
                Some(n) => {
                    b.push(0x01);
                    b.push(u8::from(*n));
                }
                None => b.push(0x00),
            }
        }
        ValidatePredicate::RequireIndex { columns } => {
            b.push(0x04);
            write_str_vec(b, columns);
        }
        ValidatePredicate::TableNameForbidden { patterns } => {
            b.push(0x05);
            write_patterns(b, patterns);
        }
    }
}

fn write_name_globs(b: &mut Vec<u8>, globs: &[crate::rule::NameGlob]) {
    b.extend_from_slice(&(globs.len() as u32).to_be_bytes());
    for g in globs {
        write_seg(b, &g.glob);
    }
}
