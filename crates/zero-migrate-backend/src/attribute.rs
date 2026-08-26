//! The vendor-facing half of vendor attributes: what a backend DECLARES.
//!
//! [`zero_migrate_ir::attribute`] holds the neutral vocabulary - the key, the value
//! carrier, the map, the scope. It knows no dialect ids and no attribute names, on
//! purpose. This module is where a backend says which keys it actually owns.
//!
//! # The declaration is the machine
//!
//! An [`AttrDef`] is not documentation. It is the single source the refusal messages, the
//! shape check, the scope check and (once generated) the TypeScript surface all read. A
//! vendor adds a knob by adding one `AttrDef` to its own [`AttributeVocabulary`]; nothing
//! in the neutral crates changes, and no `match` anywhere gains an arm.
//!
//! That is the whole point of the design. The alternative - the one the tree already has
//! in `IndexStorageParams`, now retired - was a vendor's
//! knobs as named fields in the neutral IR, which requires editing the neutral crate to
//! add one backend's storage parameter.
//!
//! # Two failures that must not read alike
//!
//! Because [`AttrKey`] carries its dialect, an unrecognized key splits into two cases the
//! engine must treat differently. Writing `acme` and `borealis` for two backends, since
//! this crate is the contract every backend shares and must name none of them:
//!
//! * `acme.fillfactor` heading for a `borealis` target - `acme` was never asked. SKIPPED,
//!   silently and correctly; the node stays portable.
//! * `acme.filfactor` heading for an `acme` target - `acme` IS the owner and declares no
//!   such leaf. REFUSED, naming the typo.
//!
//! A carrier that did not name the dialect could express the first but not the second.
//!
//! # Why the examples here are fictional
//!
//! Every dialect id in this module's docs and tests is invented. That is not squeamishness
//! about the neutrality census - it is a stronger test. A contract exercised only against
//! the ids that happen to ship could pass while special-casing one of them; a contract
//! exercised against `acme` cannot, because no such backend exists to special-case. Real
//! examples belong in each vendor's own `attribute` module, which is the one place naming
//! that vendor is correct.
//!
//! # What is NOT decided here
//!
//! Tolerance on the way IN from a live catalog is a separate question with the opposite
//! answer: an attribute read off a server that this build does not declare must be
//! CARRIED, not refused, or upgrading the server breaks introspection. That belongs with
//! the fold, not with this declaration, and is not built yet.

use serde::Serialize;
use std::fmt;

use zero_migrate_ir::attribute::{AttrKey, OpAttributes};
use zero_migrate_ir::ir::IrScalar;

/// The shape of a legal value for one attribute - the "verification" half of a
/// declaration.
///
/// Closed, and small on purpose. A vendor knob is a scalar with a domain; anything
/// needing more structure than this is an [`Op`](zero_migrate_ir::ir::Op) field, not an
/// attribute. Keeping the set closed is also what lets the TypeScript generator emit a
/// precise type per attribute - an `Enum` becomes a union of string literals, an `Int`
/// becomes `number` with its range in the doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AttrShape {
    /// A boolean flag - `acme.strict`, `acme.without_rowid`.
    Bool,
    /// An integer with an INCLUSIVE range. The range is part of the declaration because
    /// most vendor knobs have one the server enforces anyway - a fill-factor percentage
    /// is 10..=100 - and finding that out at apply time rather than at plan time is the
    /// difference between a refusal and a failed migration.
    Int {
        /// Smallest accepted value, inclusive.
        ///
        /// Exported as a decimal STRING, not a JSON number - see the note on `max`.
        #[serde(serialize_with = "i64_as_decimal_string")]
        min: i64,
        /// Largest accepted value, inclusive.
        ///
        /// Exported as a decimal STRING because the consumer is JavaScript, whose numbers
        /// are `f64`: an `i64` beyond 2^53 does not survive `JSON.parse`. This is not
        /// hypothetical - MySQL's `auto_increment` declares `i64::MAX`, and the first
        /// generated typings documented its bound as `9223372036854776000`, a number that
        /// is simply not the one declared.
        ///
        /// The IR solved this exact problem the same way:
        /// [`IrScalar::Int64`](zero_migrate_ir::ir::IrScalar) travels as its canonical
        /// decimal string for precisely this reason. A generator reading these as strings
        /// cannot lose precision, because it never converts them at all.
        #[serde(serialize_with = "i64_as_decimal_string")]
        max: i64,
    },
    /// One of a fixed set of spellings - `acme.row_format` might accept
    /// `DEFAULT`, `DYNAMIC` or `COMPRESSED`.
    ///
    /// The variants are stored exactly as the vendor spells them on the wire, and the
    /// comparison is EXACT. A vendor that accepts its own keywords case-insensitively
    /// declares the spelling it emits and normalizes on the way in.
    Enum {
        /// The accepted spellings, in the vendor's own casing.
        variants: &'static [&'static str],
    },
    /// Free text - an identifier-shaped value the vendor validates itself, such as a
    /// tablespace or character-set name.
    Text,
}

impl AttrShape {
    /// Check one value against this shape.
    ///
    /// # Errors
    /// [`AttrValueError::WrongType`] when the scalar is the wrong kind,
    /// [`AttrValueError::OutOfRange`] for an integer outside its declared bounds,
    /// [`AttrValueError::NotAVariant`] for a string outside a declared enum.
    pub fn check(&self, value: &IrScalar) -> Result<(), AttrValueError> {
        match (self, value) {
            (Self::Bool, IrScalar::Bool(_)) | (Self::Text, IrScalar::Str(_)) => Ok(()),
            (Self::Int { min, max }, IrScalar::Int(v) | IrScalar::Int64(v)) => {
                if v < min || v > max {
                    return Err(AttrValueError::OutOfRange {
                        got: *v,
                        min: *min,
                        max: *max,
                    });
                }
                Ok(())
            }
            (Self::Enum { variants }, IrScalar::Str(s)) => {
                if variants.contains(&s.as_str()) {
                    Ok(())
                } else {
                    Err(AttrValueError::NotAVariant {
                        got: s.clone(),
                        variants,
                    })
                }
            }
            _ => Err(AttrValueError::WrongType {
                want: *self,
                got: kind_of(value),
            }),
        }
    }

    /// How this shape reads in a refusal message.
    fn describe(&self) -> String {
        match self {
            Self::Bool => "a boolean".to_string(),
            Self::Int { min, max } => format!("an integer in {min}..={max}"),
            Self::Enum { variants } => format!("one of {}", variants.join(", ")),
            Self::Text => "a string".to_string(),
        }
    }
}

/// Serialize an `i64` as its canonical decimal string.
///
/// Every consumer of the exported vocabulary is JavaScript, and a JSON number there is an
/// `f64`. Emitting the bound as a string moves the value across intact and lets the
/// generator print it verbatim without ever constructing a `Number` from it.
fn i64_as_decimal_string<S: serde::Serializer>(v: &i64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&v.to_string())
}

/// The variant name of a scalar, for a message that has to say what arrived.
fn kind_of(value: &IrScalar) -> &'static str {
    match value {
        IrScalar::Null => "null",
        IrScalar::Bool(_) => "a boolean",
        IrScalar::Int(_) | IrScalar::Int64(_) => "an integer",
        IrScalar::Decimal(_) => "a decimal",
        IrScalar::Str(_) => "a string",
        IrScalar::Bytes(_) => "bytes",
    }
}

/// Why one attribute VALUE was refused. Structured, because it reaches a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrValueError {
    /// The scalar is the wrong kind entirely.
    WrongType {
        /// The declared shape.
        want: AttrShape,
        /// What arrived instead.
        got: &'static str,
    },
    /// An integer outside its declared inclusive bounds.
    OutOfRange {
        /// The value that arrived.
        got: i64,
        /// Declared lower bound, inclusive.
        min: i64,
        /// Declared upper bound, inclusive.
        max: i64,
    },
    /// A string that is not one of the declared spellings.
    NotAVariant {
        /// The value that arrived.
        got: String,
        /// The declared spellings.
        variants: &'static [&'static str],
    },
}

impl fmt::Display for AttrValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongType { want, got } => {
                write!(f, "expected {}, got {got}", want.describe())
            }
            Self::OutOfRange { got, min, max } => {
                write!(f, "{got} is outside the accepted range {min}..={max}")
            }
            Self::NotAVariant { got, variants } => {
                write!(f, "`{got}` is not one of {}", variants.join(", "))
            }
        }
    }
}

impl std::error::Error for AttrValueError {}

/// One vendor attribute, declared once by the backend that owns it.
///
/// Every field is REQUIRED - the struct derives no `Default` and is not
/// `#[non_exhaustive]`, for the same reason
/// [`BackendVendor`](crate::registry::BackendVendor)'s fields are: a knob that acquired a
/// scope or a shape by omission would be a knob nobody decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AttrDef {
    /// The namespaced key. Its dialect part MUST be the declaring vendor's own id; that
    /// is checked by a registry-walking test rather than by the compiler, because
    /// "which dialect owns this crate" is a registry fact, not a fact the key knows.
    ///
    /// Built with [`AttrKey::from_static`], so a MALFORMED key here fails the build in
    /// the vendor's own crate.
    pub key: AttrKey,
    /// Which kind of IR node this attribute may attach to.
    pub ops: &'static [&'static str],
    /// What a legal value looks like.
    pub shape: AttrShape,
    /// What the knob does, in the vendor's own words. Reaches the user twice: in a
    /// refusal that lists what IS declared, and as the doc comment on the generated
    /// TypeScript field.
    pub docs: &'static str,
}

/// Declare a backend's attributes, naming each op by its CARRIER TYPE rather than by a
/// string.
///
/// # The fail-open this closes
///
/// [`AttrDef::ops`] is `&[&str]`, so a hand-written declaration names its ops as bare
/// literals. `"createTabel"` then compiles, passes the ownership walk, passes the
/// duplicate-key walk, exports to the vocabulary artifact and generates a TypeScript
/// field - and matches nothing, forever, because [`AttributeVocabulary::get`] looks a knob
/// up by the op it was carried on. The knob is declared, typed, documented and dead.
///
/// Naming the op as a type moves that to the compiler: the misspelling is `E0425` in the
/// vendor's own crate, alongside the `E0080` a malformed [`AttrKey`] already produces. The
/// op STRING is then read back out of the carrier's own
/// [`OpAttributes::OP`](zero_migrate_ir::attribute::OpAttributes), so the declaration
/// and the carrier cannot disagree about what op they mean.
///
/// This does not retire `every_declared_op_is_a_real_op` - the op string still originates
/// in `op_attributes!` over in `zero-migrate-ir`, and a typo THERE is still just a string.
/// It moves that test's exposed surface from every vendor declaration to the six carrier
/// definitions in one file.
///
/// # What else it collapses
///
/// The dialect is written ONCE instead of being re-typed into every key, and the prose is
/// a `///` doc comment instead of a `docs:` string literal - so one source feeds rustdoc
/// AND the generated TypeScript comment. Note that `concat!` joins doc lines on their
/// leading space: a blank `///` line vanishes, so declarations get no paragraph breaks.
///
/// # Example
///
/// ```
/// use zero_migrate_backend::{attribute::{AttrDef, AttrShape}, declare_attributes};
/// use zero_migrate_ir::attribute::{CreateIndexAttributes, CreateTableAttributes};
///
/// static DEFS: &[AttrDef] = declare_attributes! {
///     dialect: "acme";
///
///     /// Percentage of each page left free for later updates.
///     fillfactor on [CreateTableAttributes, CreateIndexAttributes]
///         = AttrShape::Int { min: 10, max: 100 };
/// };
///
/// assert_eq!(DEFS[0].key.to_string(), "acme.fillfactor");
/// assert_eq!(DEFS[0].ops, &["createTable", "createIndex"]);
/// ```
///
/// A misspelled op is a compile error rather than a knob that silently matches nothing:
///
/// ```compile_fail,E0425
/// use zero_migrate_backend::{attribute::{AttrDef, AttrShape}, declare_attributes};
/// use zero_migrate_ir::attribute::CreateTableAttributes;
///
/// static DEFS: &[AttrDef] = declare_attributes! {
///     dialect: "acme";
///
///     /// A knob declared against an op that does not exist.
///     fillfactor on [CreateTabelAttributes] = AttrShape::Int { min: 10, max: 100 };
/// };
/// ```
#[macro_export]
macro_rules! declare_attributes {
    (dialect: $dialect:literal; $(
        $(#[doc = $doc:literal])+
        $name:ident on [$($carrier:ty),+ $(,)?] = $shape:expr;
    )+) => {
        &[$(
            $crate::attribute::AttrDef {
                key: ::zero_migrate_ir::attribute::AttrKey::from_static(
                    concat!($dialect, ".", stringify!($name)),
                ),
                // Read out of the carrier, not retyped. `OpAttributes` is in the bound
                // position deliberately: a type that is not an op carrier fails here
                // rather than contributing some other associated `OP`.
                ops: &[$(
                    <$carrier as ::zero_migrate_ir::attribute::OpAttributes>::OP
                ),+],
                shape: $shape,
                // Each `///` line arrives with its leading space, which is what joins the
                // lines into one sentence - but it also puts one at the FRONT. `trim_ascii`
                // is `const`, so the stored string is byte-identical to the hand-written
                // `docs:` literal this replaced rather than merely close to it.
                docs: concat!($($doc),+).trim_ascii(),
            }
        ),+]
    };
}

/// Everything one backend declares about its attributes.
///
/// A vendor builds this as a `const` from a `&'static [AttrDef]` and hands it to its
/// [`BackendVendor`](crate::registry::BackendVendor). A backend with no attributes at all
/// declares an EMPTY vocabulary explicitly - visible in the diff - rather than omitting
/// the field.
#[derive(Debug, Clone, Copy)]
pub struct AttributeVocabulary {
    defs: &'static [AttrDef],
}

impl AttributeVocabulary {
    /// Declare a vocabulary. `const`, so it can sit in a vendor's `static VENDOR`.
    #[must_use]
    pub const fn new(defs: &'static [AttrDef]) -> Self {
        Self { defs }
    }

    /// The empty vocabulary, for a backend that declares no attributes yet.
    ///
    /// Spelled out at the vendor's own definition site, never reached by omission.
    #[must_use]
    pub const fn empty() -> Self {
        Self { defs: &[] }
    }

    /// Every declared attribute.
    pub fn iter(&self) -> impl Iterator<Item = &'static AttrDef> {
        self.defs.iter()
    }

    /// How many attributes are declared.
    #[must_use]
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// True when this backend declares no attributes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// The declaration for one key AT ONE SCOPE, if this backend owns it.
    ///
    /// # Declaration identity is the PAIR, not the key
    ///
    /// A leaf may legitimately be declared at more than one scope, meaning the same
    /// thing in each. PostgreSQL's
    /// `fillfactor` is legal on both a table and an index and means the same thing in
    /// each." Until this method took a scope, the code could not do what that doc said -
    /// lookup was key-unique, so the second declaration was unreachable and the
    /// workspace census counted it a duplicate.
    ///
    /// That is not a nicety. It is the hard prerequisite for retiring
    /// `IndexStorageParams`, now retired, whose
    /// `fillfactor` must be declared at Index scope alongside the Table-scoped one that
    /// already ships.
    #[must_use]
    pub fn get(&self, key: &AttrKey, op: &str) -> Option<&'static AttrDef> {
        self.defs
            .iter()
            .find(|d| &d.key == key && d.ops.contains(&op))
    }

    /// Every scope at which this backend declares `key`, in declaration order.
    ///
    /// Used to tell two refusals apart that would otherwise read alike: a key that is
    /// declared but was written in the wrong position, versus a key nobody declares. A
    /// lookup miss alone cannot distinguish them, and the difference is the difference
    /// between "move this" and "you typo'd this".
    #[must_use]
    pub fn ops_declaring(&self, key: &AttrKey) -> Vec<&'static str> {
        self.defs
            .iter()
            .filter(|d| &d.key == key)
            .flat_map(|d| d.ops.iter().copied())
            .collect()
    }

    /// Every key declared for one scope, in declaration order - what a refusal lists
    /// when it says "did you mean".
    #[must_use]
    pub fn keys_for_op(&self, op: &str) -> Vec<&'static AttrKey> {
        self.defs
            .iter()
            .filter(|d| d.ops.contains(&op))
            .map(|d| &d.key)
            .collect()
    }

    /// Check every attribute in `attrs` that belongs to `dialect`, at `scope`.
    ///
    /// Keys belonging to OTHER dialects are not this vocabulary's business and are left
    /// alone - that is the skip half of the two-failures rule in this module's header.
    /// Keys that ARE this dialect's must be declared, in scope, and shaped correctly.
    ///
    /// The scope is taken from the VALUE's type, not from an argument - see
    /// [`OpAttributes`] for why a hand-passed op is the `detachPartition` failure
    /// shape. A caller cannot check table attributes against column declarations, because
    /// there is no argument left to get wrong.
    ///
    /// # Errors
    /// The first failure, naming the key. One error rather than a list because these are
    /// authoring mistakes: a typo'd key is usually the only one.
    pub fn check<A: OpAttributes>(&self, dialect: &str, carried: &A) -> Result<(), AttrError> {
        let op = A::OP;
        let attrs = carried.attributes();
        for (key, value) in attrs.iter() {
            if key.dialect() != dialect {
                continue;
            }
            let Some(def) = self.get(key, op) else {
                // A miss on the PAIR has two very different causes, and collapsing them
                // would turn "you put this in the wrong place" into "you typo'd this".
                // Ask what scopes DO declare the key before deciding which refusal the
                // author has earned.
                let elsewhere = self.ops_declaring(key);
                return Err(match elsewhere.first() {
                    Some(_) => AttrError::NotLegalOnOp {
                        key: key.clone(),
                        declared_for: elsewhere.clone(),
                        used_on: op,
                    },
                    None => AttrError::Undeclared {
                        key: key.clone(),
                        declared: self
                            .keys_for_op(op)
                            .into_iter()
                            .map(ToString::to_string)
                            .collect(),
                    },
                });
            };
            def.shape
                .check(value)
                .map_err(|source| AttrError::BadValue {
                    key: key.clone(),
                    source,
                })?;
        }
        Ok(())
    }
}

/// Why one attribute was refused against a vendor's declared vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrError {
    /// The owning dialect is registered but declares no such leaf - a typo, or a knob
    /// from a newer version of this backend.
    Undeclared {
        /// The key that was not found.
        key: AttrKey,
        /// What IS declared at this scope, so the message can be acted on.
        declared: Vec<String>,
    },
    /// The key is declared, but for a different kind of node.
    NotLegalOnOp {
        /// The key.
        key: AttrKey,
        /// Every op the vendor DOES permit it on. Plural because one knob is commonly
        /// legal on several - `createTable` and `setTableOptions`, say - and naming only
        /// the first would read as if the others were forbidden.
        declared_for: Vec<&'static str>,
        /// The op it was written on.
        used_on: &'static str,
    },
    /// The key and scope are right; the value is not.
    BadValue {
        /// The key.
        key: AttrKey,
        /// What was wrong with the value.
        source: AttrValueError,
    },
}

impl fmt::Display for AttrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undeclared { key, declared } => {
                write!(
                    f,
                    "`{key}` is not an attribute the `{}` backend declares",
                    key.dialect()
                )?;
                if declared.is_empty() {
                    write!(f, " (it declares none at this position)")
                } else {
                    write!(f, " — it declares: {}", declared.join(", "))
                }
            }
            Self::NotLegalOnOp {
                key,
                declared_for,
                used_on,
            } => write!(
                f,
                "`{key}` cannot be set on `{used_on}` — this backend declares it on {}",
                declared_for.join(", ")
            ),
            Self::BadValue { key, source } => write!(f, "`{key}`: {source}"),
        }
    }
}

impl std::error::Error for AttrError {}

/// Render a backend's declared vocabulary as the JSON artifact its npm package is
/// generated from.
///
/// # Why this lives in the contract crate
///
/// Three vendors need byte-identical export logic, and a copy per vendor is three
/// chances for the artifacts to disagree in shape - at which point one npm package's
/// generator reads a field another vendor never wrote. The function names no vendor: it
/// takes the dialect id and the vocabulary, so it is the same kind of thing as the
/// traits beside it.
///
/// # Why an artifact exists at all
///
/// The route `generated/ir.ts` takes - derive a JSON Schema from the Rust type - CANNOT
/// type attributes. `schemars` derives from the TYPE, and the type is an open
/// `BTreeMap<AttrKey, IrScalar>`; the vocabulary is `static` DATA, invisible to it. That
/// route yields `Record<string, IrScalar>`: no key names, no autocomplete, and a typo
/// indistinguishable from a real knob. Hence a second artifact, exported as data.
///
/// # Errors
/// Propagates a serialization failure rather than panicking, so a caller in a test can
/// report it with its own context.
pub fn vocabulary_json(
    dialect: &str,
    vocabulary: AttributeVocabulary,
) -> Result<String, serde_json::Error> {
    /// The exported document. Versioned so a generator can refuse a shape it predates
    /// rather than mis-read one.
    #[derive(Serialize)]
    struct Document<'a> {
        version: u32,
        dialect: &'a str,
        attributes: Vec<&'static AttrDef>,
    }

    let doc = Document {
        version: VOCABULARY_FORMAT_VERSION,
        dialect,
        attributes: vocabulary.iter().collect(),
    };
    let mut s = serde_json::to_string_pretty(&doc)?;
    s.push('\n'); // trailing newline so the file is POSIX-clean
    Ok(s)
}

/// Wire-shape version of the exported vocabulary artifact - NOT of the IR.
///
/// Bump when the artifact's shape changes in a way a generator must notice. Each
/// vendor's generator refuses a version it does not understand, so a bump is a loud
/// failure rather than a silently mis-read file.
pub const VOCABULARY_FORMAT_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use zero_migrate_ir::attribute::{Attributes, CreateTableAttributes};

    const FILLFACTOR: AttrKey = AttrKey::from_static("acme.fillfactor");
    const TABLESPACE: AttrKey = AttrKey::from_static("acme.tablespace");
    const STORAGE: AttrKey = AttrKey::from_static("acme.storage");
    const ROW_FORMAT: AttrKey = AttrKey::from_static("borealis.row_format");

    static DEFS: &[AttrDef] = &[
        AttrDef {
            key: AttrKey::from_static("acme.fillfactor"),
            ops: &["createTable"],
            shape: AttrShape::Int { min: 10, max: 100 },
            docs: "percentage of a page left free for later updates",
        },
        AttrDef {
            key: AttrKey::from_static("acme.tablespace"),
            ops: &["createTable"],
            shape: AttrShape::Text,
            docs: "the storage area the table is created in",
        },
        AttrDef {
            key: AttrKey::from_static("acme.storage"),
            ops: &["addColumn"],
            shape: AttrShape::Enum {
                variants: &["PLAIN", "EXTERNAL", "EXTENDED", "MAIN"],
            },
            docs: "per-column out-of-line storage strategy",
        },
    ];

    fn vocab() -> AttributeVocabulary {
        AttributeVocabulary::new(DEFS)
    }

    /// Build a TABLE-scoped set. There is no `column(...)` helper because no IR node
    /// carries column attributes yet, and inventing one would be a landing pad nobody
    /// checks - see `OpAttributes`.
    fn table(pairs: &[(&AttrKey, IrScalar)]) -> CreateTableAttributes {
        let mut a = Attributes::new();
        for (k, v) in pairs {
            a.insert((*k).clone(), v.clone());
        }
        CreateTableAttributes::from(a)
    }

    #[test]
    fn a_declared_attribute_in_its_own_scope_with_a_legal_value_passes() {
        let a = table(&[
            (&FILLFACTOR, IrScalar::Int(85)),
            (&TABLESPACE, IrScalar::Str("fast_ssd".into())),
        ]);
        assert_eq!(vocab().check("acme", &a), Ok(()));
    }

    /// The REFUSE half of the two-failures rule: the owner is asked and says no.
    #[test]
    fn a_typo_in_a_key_this_backend_owns_is_refused_and_lists_what_is_declared() {
        let typo = AttrKey::parse("acme.filfactor").expect("shape is fine");
        let a = table(&[(&typo, IrScalar::Int(85))]);
        let err = vocab()
            .check("acme", &a)
            .expect_err("an undeclared leaf must be refused");
        let AttrError::Undeclared { key, declared } = &err else {
            panic!("expected Undeclared, got {err:?}");
        };
        assert_eq!(key, &typo);
        assert!(
            declared.contains(&"acme.fillfactor".to_string()),
            "the refusal must name the key the user meant: {declared:?}"
        );
        // The column-scoped key must NOT be offered as a table-scoped suggestion.
        assert!(
            !declared.contains(&"acme.storage".to_string()),
            "suggestions are scope-filtered: {declared:?}"
        );
    }

    /// The SKIP half: a key belonging to a dialect that was never asked passes through.
    /// This is what keeps a node authored for several backends portable to all of them.
    #[test]
    fn a_key_owned_by_another_dialect_is_not_this_vocabularys_business() {
        let a = table(&[
            (&FILLFACTOR, IrScalar::Int(85)),
            (&ROW_FORMAT, IrScalar::Str("DYNAMIC".into())),
        ]);
        assert_eq!(
            vocab().check("acme", &a),
            Ok(()),
            "`borealis.row_format` is borealis's to judge, not acme's"
        );
        // The control: it is not passing because the map is empty or the loop is dead.
        let bad = table(&[(&FILLFACTOR, IrScalar::Int(5))]);
        assert!(
            vocab().check("acme", &bad).is_err(),
            "the same loop must still judge acme's own keys"
        );
    }

    /// A column knob written on a table is refused BY SCOPE - and note what the call
    /// looks like: the scope is never named, it comes from `TableAttributes`.
    #[test]
    fn a_column_attribute_written_on_a_table_is_refused_by_scope() {
        let a = table(&[(&STORAGE, IrScalar::Str("EXTENDED".into()))]);
        let err = vocab()
            .check("acme", &a)
            .expect_err("a column knob on a table must be refused");
        assert!(
            matches!(
                err,
                AttrError::NotLegalOnOp {
                    used_on: "createTable",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// A range is declared so it can be refused at PLAN time. Both ends, and both
    /// just-inside controls, because an off-by-one here is a migration that fails
    /// mid-apply instead.
    #[test]
    fn an_integer_outside_its_declared_range_is_refused_at_both_ends() {
        for bad in [9_i64, 101] {
            let a = table(&[(&FILLFACTOR, IrScalar::Int(bad))]);
            let err = vocab().check("acme", &a).expect_err("outside 10..=100");
            assert!(
                matches!(
                    err,
                    AttrError::BadValue {
                        source: AttrValueError::OutOfRange {
                            min: 10,
                            max: 100,
                            ..
                        },
                        ..
                    }
                ),
                "got {err:?}"
            );
        }
        for ok in [10_i64, 100] {
            let a = table(&[(&FILLFACTOR, IrScalar::Int(ok))]);
            assert_eq!(
                vocab().check("acme", &a),
                Ok(()),
                "{ok} is INSIDE the inclusive range"
            );
        }
    }

    #[test]
    fn a_value_of_the_wrong_kind_entirely_is_refused() {
        let a = table(&[(&FILLFACTOR, IrScalar::Str("85".into()))]);
        let err = vocab()
            .check("acme", &a)
            .expect_err("a string is not an integer");
        assert!(
            matches!(
                err,
                AttrError::BadValue {
                    source: AttrValueError::WrongType { .. },
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    /// `Int64` exists so a value too large for JSON's exact range survives the wire. An
    /// attribute declared `Int` must accept BOTH spellings, or the same number refuses
    /// or passes depending on how it was encoded.
    #[test]
    fn an_int_attribute_accepts_both_wire_spellings_of_an_integer() {
        let a = table(&[(&FILLFACTOR, IrScalar::Int64(85))]);
        assert_eq!(vocab().check("acme", &a), Ok(()));
    }

    #[test]
    fn an_empty_vocabulary_refuses_every_key_of_its_own_dialect() {
        let empty = AttributeVocabulary::empty();
        assert!(empty.is_empty());
        let a = table(&[(&FILLFACTOR, IrScalar::Int(85))]);
        assert!(
            empty.check("acme", &a).is_err(),
            "a backend that declares nothing owns nothing"
        );
        // ...and still skips another dialect's keys rather than refusing them.
        let other = table(&[(&ROW_FORMAT, IrScalar::Str("DYNAMIC".into()))]);
        assert_eq!(empty.check("acme", &other), Ok(()));
    }

    /// The shape checks that only reach a user through a MESSAGE, kept together so the
    /// enum wording is exercised rather than only its variant.
    #[test]
    fn a_refusal_message_names_the_declared_variants_and_compares_them_exactly() {
        let wrong = table(&[(&STORAGE, IrScalar::Str("COMPRESSED".into()))]);
        // Scope refuses first here, so exercise the enum check directly: it is the shape,
        // not the scope, that owns the variant list.
        let def = vocab()
            .get(&STORAGE, "addColumn")
            .expect("declared at column scope");
        let err = def
            .shape
            .check(&IrScalar::Str("COMPRESSED".into()))
            .expect_err("COMPRESSED is not one of the declared strategies");
        assert!(
            err.to_string().contains("EXTENDED"),
            "must list the real variants: {err}"
        );
        assert!(
            def.shape.check(&IrScalar::Str("extended".into())).is_err(),
            "lowercase must not pass an exactly-spelled enum"
        );
        assert!(
            def.shape.check(&IrScalar::Str("EXTENDED".into())).is_ok(),
            "the control: the exact spelling IS accepted"
        );
        // And the scope refusal still fires for the same key on a table.
        assert!(vocab().check("acme", &wrong).is_err());
    }
}
