//! The MySQL PHYSICAL TYPE: this backend's own parsed answer to "what type is this
//! column, exactly", and the carrier leg it rides in.
//!
//! # Why this is not a field on the neutral column snapshot
//!
//! It used to be one - a named field on the neutral `ColumnSnapshot`, in
//! `zero-migrate-backend` - and the engine `match`ed on its variants in three
//! places. That put both halves of the hard limit in the wrong crate: the neutral
//! vocabulary spelled a vendor's name, and neutral code resolved a vendor's type
//! grammar to decide whether two columns were the same.
//!
//! The type lives here, where its parser and its speller already had to be, and
//! reaches the neutral snapshot as an opaque leg of
//! [`Dialectal<dyn VendorColumnFacts>`](zeroship_migrate_backend::dialectal::Dialectal)
//! keyed by [`crate::DIALECT`]. The engine carries it, clones it and compares it by ASKING
//! it; the engine cannot read it, because reading takes a `downcast_ref` to a type
//! declared in this crate.
//!
//! # What an absent leg means
//!
//! `None`, and nothing else: this column has no MySQL physical contract, which is
//! the state of every column from another dialect's catalog and of every
//! author-built desired snapshot that has not derived one. Both the identity
//! comparison and the drift report require the leg on BOTH sides, which
//! [`Dialectal::paired`](zeroship_migrate_backend::dialectal::Dialectal::paired)
//! enforces - a contract compared against an absent one describes nothing about the
//! database.

use std::any::Any;
use std::sync::Arc;

use crate::DIALECT;
use zeroship_migrate_backend::dialectal::{Dialectal, DialectalValue, VendorColumnFacts};
use zeroship_migrate_backend::snapshot::ColumnSnapshot;

/// The physical identity of one MySQL column, as parsed VALUES rather than as
/// rendered type text.
///
/// The portable `data_type` cannot answer this: MySQL's
/// [`SchemaRenderer::canonical_type`](zeroship_migrate_backend::schema::SchemaRenderer::canonical_type)
/// folds every `varchar(n)` to the literal `text`, so a live `varchar(64)` and a
/// declared `varchar(255)` are indistinguishable once stored. Both sides of a
/// comparison fold the same way, so the blindness is symmetric and silent.
///
/// COMPARING RENDERED TEXT INSTEAD WAS REJECTED, and the reason is measured rather
/// than stylistic. The engine emits `DECIMAL(65, 30)` where MySQL stores
/// `decimal(65,30)`, and emits `POINT SRID 4326` where MySQL stores bare `point`
/// because the SRID is a separate catalog column. A string comparison turns each of
/// those into a reported difference on a database that never changed, and every
/// future type spelling is another chance to add a third. Parsed values make both
/// disappear: precision and scale are integers, so spacing is not a concept, and a
/// facet MySQL does not put in `COLUMN_TYPE` is simply not part of the type.
///
/// The variants are FAMILY-GATED on purpose. Comparing every populated catalog facet
/// regardless of family is itself a source of false differences, because MySQL
/// populates numeric precision for temporal columns and character length for binary
/// ones.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MysqlPhysicalType {
    /// `VARCHAR(n)` / `CHAR(n)`, carrying the character length MySQL enforces.
    Character {
        /// `true` for the fixed-width `CHAR` spelling.
        fixed: bool,
        /// Declared length in characters.
        length: u32,
    },
    /// A `TEXT`/`BLOB` storage tier, which changes the capacity the column accepts.
    Lob {
        /// The catalog `DATA_TYPE`, e.g. `tinytext`, `text`, `mediumblob`.
        tier: String,
    },
    /// An integer family member. Display width is dropped because MySQL 8 no longer
    /// stores it - EXCEPT for `tinyint(1)`, which is how the renderer spells a
    /// boolean and which MySQL does preserve, so it is carried as its own flag
    /// rather than as a width.
    Integer {
        /// The catalog `DATA_TYPE`, e.g. `int`, `bigint`, `tinyint`.
        kind: String,
        /// `UNSIGNED` changes both the range and foreign-key compatibility.
        unsigned: bool,
        /// The `tinyint(1)` boolean spelling.
        boolean: bool,
    },
    /// `DECIMAL(p, s)` and friends, where both parameters are semantic.
    Decimal {
        /// Total digits.
        precision: u32,
        /// Digits after the point.
        scale: u32,
        /// `UNSIGNED` changes the representable range.
        unsigned: bool,
    },
    /// A date/time family member with its fractional-seconds precision. MySQL omits
    /// the precision entirely when it is zero, so an absent one means zero.
    Temporal {
        /// The catalog `DATA_TYPE`, e.g. `datetime`, `timestamp`, `time`.
        kind: String,
        /// Fractional-seconds precision, zero when MySQL stores none.
        fsp: u32,
    },
    /// `ENUM`/`SET` members, in declaration order. Members are compared as decoded
    /// values, so quoting and interior spaces cannot corrupt them.
    Members {
        /// `enum` or `set`.
        kind: String,
        /// Members in declaration order.
        members: Vec<String>,
    },
    /// A spatial column. The SRID is read from its own catalog column, never from
    /// the type text, because MySQL does not put it there.
    Spatial {
        /// The catalog `DATA_TYPE`, e.g. `point`, `geometry`.
        kind: String,
        /// `SRS_ID`, absent when the column is unconstrained.
        srid: Option<u32>,
    },
    /// A family carrying no parameters worth comparing, e.g. `json`, `double`.
    Plain {
        /// The catalog `DATA_TYPE`.
        kind: String,
    },
    /// A type this engine does not model yet.
    ///
    /// Deliberately NOT equal to itself in the comparators' sense: a consumer must
    /// decide what an unmodelled type means for it, because the safe direction
    /// differs. A guard must refuse to treat it as a match and adopt the object; a
    /// differ must refuse to report a difference it cannot actually establish.
    /// Collapsing both into one answer is what makes an unknown type either
    /// silently adopted or loudly false-reported.
    Unknown {
        /// The catalog `DATA_TYPE` as MySQL spelled it.
        raw: String,
    },
}

impl MysqlPhysicalType {
    /// Parse a MySQL type spelling into its physical identity.
    ///
    /// Deliberately accepts BOTH spellings the engine has to reconcile: MySQL's own
    /// `COLUMN_TYPE` (`decimal(65,30)`) and the renderer's emitted DDL type
    /// (`DECIMAL(65, 30)`). Because it reads values rather than normalising text,
    /// those two produce the same result and a space cannot be mistaken for a type
    /// change. That is the whole reason both sides can share one function.
    ///
    /// The base family is taken from the text BEFORE the first `(`, so quoted enum
    /// members can never be mistaken for it. A family this engine does not model
    /// becomes [`MysqlPhysicalType::Unknown`] rather than a guess.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        let unsigned = trimmed.to_ascii_lowercase().contains(" unsigned");
        let head = trimmed.split('(').next().unwrap_or(trimmed);
        let family = head.trim().to_ascii_lowercase();
        let family = family
            .split_whitespace()
            .next()
            .unwrap_or(&family)
            .to_string();

        let args = trimmed
            .find('(')
            .zip(trimmed.rfind(')'))
            .filter(|(open, close)| close > open)
            .map(|(open, close)| &trimmed[open + 1..close]);

        let numeric_args: Vec<u32> = args
            .map(|a| {
                a.split(',')
                    .filter_map(|part| part.trim().parse::<u32>().ok())
                    .collect()
            })
            .unwrap_or_default();

        match family.as_str() {
            "varchar" | "char" => numeric_args.first().map_or_else(
                || Self::Unknown {
                    raw: raw.to_string(),
                },
                |length| Self::Character {
                    fixed: family == "char",
                    length: *length,
                },
            ),
            "tinytext" | "text" | "mediumtext" | "longtext" | "tinyblob" | "blob"
            | "mediumblob" | "longblob" => Self::Lob { tier: family },
            "tinyint" | "smallint" | "mediumint" | "int" | "bigint" => Self::Integer {
                // MySQL 8 no longer stores a display width, with `tinyint(1)` the one
                // exception it keeps - and that exception is the boolean the renderer
                // emits, so it is carried as a flag rather than discarded as a width.
                boolean: family == "tinyint" && numeric_args.first() == Some(&1),
                kind: family,
                unsigned,
            },
            "decimal" | "numeric" => Self::Decimal {
                precision: numeric_args.first().copied().unwrap_or(10),
                scale: numeric_args.get(1).copied().unwrap_or(0),
                unsigned,
            },
            "datetime" | "timestamp" | "time" => Self::Temporal {
                // An absent precision means zero: MySQL omits `(0)` entirely.
                fsp: numeric_args.first().copied().unwrap_or(0),
                kind: family,
            },
            "date" | "year" => Self::Temporal {
                kind: family,
                fsp: 0,
            },
            "enum" | "set" => Self::Members {
                kind: family,
                members: args.map(split_quoted_members).unwrap_or_default(),
            },
            "json" | "float" | "double" | "bit" | "boolean" => Self::Plain { kind: family },
            _ => Self::Unknown {
                raw: raw.to_string(),
            },
        }
    }

    /// Spell this contract the way MySQL spells it, so a reader can take the text
    /// straight to the server.
    ///
    /// The exact inverse of [`Self::parse`], and beside it on purpose: two halves of
    /// one round trip, where changing either without the other is what silently
    /// breaks it. `Self::parse(x.type_text()) == x` holds for every family `parse` can
    /// produce, and that is what keeps two contracts that are NOT equal from rendering
    /// to the same text - the property `zeroship_migrate::apply::drift`'s `data_type`
    /// report rests on, because a collision there puts the difference straight back
    /// into the equal-strings hole the report exists to get out of.
    ///
    /// [`Self::Spatial`] is the one variant `parse` never yields - the SRID comes from
    /// its own catalog column, never from the type text - so it is spelled for a human
    /// rather than for the parser.
    #[must_use]
    pub fn type_text(&self) -> String {
        match self {
            Self::Character { fixed, length } => {
                format!("{}({length})", if *fixed { "char" } else { "varchar" })
            }
            Self::Lob { tier } => tier.clone(),
            Self::Integer {
                kind,
                unsigned,
                boolean,
            } => {
                let width = if *boolean { "(1)" } else { "" };
                let sign = if *unsigned { " unsigned" } else { "" };
                format!("{kind}{width}{sign}")
            }
            Self::Decimal {
                precision,
                scale,
                unsigned,
            } => {
                let sign = if *unsigned { " unsigned" } else { "" };
                format!("decimal({precision},{scale}){sign}")
            }
            Self::Temporal { kind, fsp } => {
                // MySQL omits `(0)` entirely, and `parse` reads an absent precision as zero.
                if *fsp == 0 {
                    kind.clone()
                } else {
                    format!("{kind}({fsp})")
                }
            }
            Self::Members { kind, members } => {
                let members = members
                    .iter()
                    .map(|member| format!("'{}'", member.replace('\'', "''")))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{kind}({members})")
            }
            Self::Spatial { kind, srid } => match srid {
                Some(srid) => format!("{kind} srid {srid}"),
                None => kind.clone(),
            },
            Self::Plain { kind } => kind.clone(),
            Self::Unknown { raw } => raw.clone(),
        }
    }
}

/// Split an `ENUM`/`SET` member list into decoded values.
///
/// Members are single-quoted and may contain commas, spaces and doubled quotes, so
/// splitting on a bare comma corrupts them, and lowercasing the whole string folds
/// `enum('a','A')` into one member.
fn split_quoted_members(args: &str) -> Vec<String> {
    let mut members = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = args.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' if in_quotes && chars.peek() == Some(&'\'') => {
                current.push('\'');
                chars.next();
            }
            '\'' => {
                in_quotes = !in_quotes;
                if !in_quotes {
                    members.push(std::mem::take(&mut current));
                }
            }
            _ if in_quotes => current.push(ch),
            _ => {}
        }
    }
    members
}

impl DialectalValue for MysqlPhysicalType {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn dialectal_eq(&self, other: &dyn Any) -> bool {
        other.downcast_ref::<Self>() == Some(self)
    }
}

impl VendorColumnFacts for MysqlPhysicalType {
    /// Two MySQL columns are the same physical column when their parsed contracts
    /// are equal - except that an unmodelled family cannot ESTABLISH a difference,
    /// so it declines by answering `true`.
    ///
    /// That is a DIFFER's safe direction and not a general rule: an existence guard
    /// asking the same question must refuse to adopt instead, because being wrong
    /// costs it a silently adopted column rather than a missed drift line.
    fn physical_identity(&self, other: &dyn VendorColumnFacts) -> bool {
        let Some(other) = other.as_any().downcast_ref::<Self>() else {
            // Another backend's facts under MySQL's key cannot be produced from
            // outside this crate; declining is the differ's safe direction anyway.
            return true;
        };
        if matches!(self, Self::Unknown { .. }) || matches!(other, Self::Unknown { .. }) {
            return true;
        }
        self == other
    }

    /// The two sides of a `data_type` drift line in MySQL's own spelling.
    ///
    /// The portable `data_type` cannot do this job and fails in two directions. The
    /// fold emits one dialect's `information_schema` spelling while the catalog side
    /// is folded through `canonical_type`, so a widened `decimal` reported
    /// `expected: "numeric", actual: "decimal"` - one type spelled two ways, naming
    /// nothing a reader can act on. And when the two spellings COINCIDE - a live
    /// `TEXT` narrowed to `VARCHAR(64)` is `"text"` on both sides - the report's
    /// equal-sides guard dropped the entry entirely, so it was blind exactly where
    /// the comparator was not.
    ///
    /// `None` only when the two contracts are EQUAL, so there is no difference to
    /// name and the caller keeps its portable pair.
    ///
    /// Two UNEQUAL contracts that nevertheless spell the same text fall back to
    /// their `Debug`, which prints every field, so two values that are not equal
    /// cannot render the same. That is unreachable for every family
    /// [`MysqlPhysicalType::parse`] produces - each renders its distinguishing
    /// values, and the round-trip test below says so - but a collision that returned
    /// the portable pair instead would re-lose the difference through the very
    /// equal-sides guard this method exists to get past, which is too quiet a failure
    /// to leave to inspection.
    fn type_drift_report(&self, other: &dyn VendorColumnFacts) -> Option<(String, String)> {
        let other = other.as_any().downcast_ref::<Self>()?;
        if self == other {
            return None;
        }
        let (mine, theirs) = (self.type_text(), other.type_text());
        if mine != theirs {
            return Some((mine, theirs));
        }
        Some((format!("{self:?}"), format!("{other:?}")))
    }
}

/// Record `physical` as MySQL's leg of `column`'s vendor facts.
///
/// The one place a `MysqlPhysicalType` is paired with the [`crate::DIALECT`] key,
/// so a leg
/// keyed by one dialect holding another's type is not expressible outside this
/// function.
pub fn record(column: &mut ColumnSnapshot, physical: MysqlPhysicalType) {
    column
        .vendor
        .insert(DIALECT, Arc::new(physical) as Arc<dyn VendorColumnFacts>);
}

/// MySQL's leg of `column`'s vendor facts, or `None` when this column carries no
/// MySQL physical contract.
#[must_use]
pub fn recorded(column: &ColumnSnapshot) -> Option<&MysqlPhysicalType> {
    column.vendor.get::<MysqlPhysicalType>(&DIALECT)
}

/// A carrier holding exactly MySQL's leg, for a caller that has a contract but not a
/// column to hang it on.
#[must_use]
pub fn carrier(physical: MysqlPhysicalType) -> Dialectal<dyn VendorColumnFacts> {
    let mut carrier = Dialectal::new();
    carrier.insert(DIALECT, Arc::new(physical) as Arc<dyn VendorColumnFacts>);
    carrier
}

#[cfg(test)]
mod mysql_physical_type_round_trip {
    //! `parse` and `type_text` are one contract read in two directions, so the tests
    //! that hold them to each other sit with them rather than with either consumer.
    //! They moved here from `zeroship_migrate::apply::drift`, where the speller used to
    //! live: they were never about the differ, and leaving them behind would have left
    //! the round trip asserted from a crate that no longer owns either half.

    use super::MysqlPhysicalType;

    /// Every family `MysqlPhysicalType::parse` can produce, spelled so it parses back
    /// to itself.
    ///
    /// This is what makes a drift report FAITHFUL rather than merely non-empty: a
    /// reader can take the printed string to the server, and two contracts that are not
    /// equal cannot render to the same text without one of these round-trips failing.
    const PARSEABLE_SPELLINGS: &[&str] = &[
        "varchar(255)",
        "varchar(64)",
        "char(8)",
        "char(36)",
        "text",
        "tinytext",
        "mediumtext",
        "longtext",
        "blob",
        "longblob",
        "int",
        "int unsigned",
        "bigint",
        "bigint unsigned",
        "tinyint",
        "tinyint(1)",
        "smallint",
        "mediumint",
        "decimal(12,2)",
        "decimal(30,10)",
        "decimal(65,30)",
        "decimal(10,0) unsigned",
        "datetime",
        "datetime(3)",
        "datetime(6)",
        "timestamp",
        "timestamp(6)",
        "time(3)",
        "date",
        "year",
        "enum('a','b')",
        "enum('a, b','c''d')",
        "set('x','y')",
        "json",
        "double",
        "float",
        "bit",
    ];

    #[test]
    fn a_reported_contract_parses_back_to_the_contract_it_came_from() {
        for spelling in PARSEABLE_SPELLINGS {
            let physical = MysqlPhysicalType::parse(spelling);
            assert!(
                !matches!(physical, MysqlPhysicalType::Unknown { .. }),
                "{spelling} is meant to exercise a MODELLED family, but parsed as Unknown"
            );
            let printed = physical.type_text();
            assert_eq!(
                MysqlPhysicalType::parse(&printed),
                physical,
                "{spelling} printed as {printed:?}, which does not parse back to itself"
            );
        }
    }

    #[test]
    fn two_different_contracts_never_print_the_same_text() {
        // The whole point of the report change is to get past `push`, which drops an
        // entry whose two sides are equal strings. A spelling collision would put the
        // difference straight back in the hole it was just pulled out of.
        for (i, left) in PARSEABLE_SPELLINGS.iter().enumerate() {
            for right in &PARSEABLE_SPELLINGS[i + 1..] {
                let (left_type, right_type) = (
                    MysqlPhysicalType::parse(left),
                    MysqlPhysicalType::parse(right),
                );
                if left_type == right_type {
                    continue;
                }
                assert_ne!(
                    left_type.type_text(),
                    right_type.type_text(),
                    "{left} and {right} are different contracts that print the same text"
                );
            }
        }
    }
}

#[cfg(test)]
mod physical_contract_rules {
    //! What makes two MySQL physical contracts THE SAME COLUMN, and how a difference
    //! between them is spelled.
    //!
    //! These moved out of `zeroship_migrate::apply::drift`'s in-src tests, where they
    //! asserted core's comparator against a MySQL rule core no longer holds. Core's
    //! half - that it asks a leg exactly when one is present on both sides, and falls
    //! through to the portable comparison otherwise - is asserted there still, against
    //! a stand-in contract. This is the vendor's half, asserted where the rule lives.

    use super::MysqlPhysicalType;
    use zeroship_migrate_backend::dialectal::VendorColumnFacts;

    fn same(left: &str, right: &str) -> bool {
        MysqlPhysicalType::parse(left).physical_identity(&MysqlPhysicalType::parse(right))
    }

    fn report(left: &str, right: &str) -> Option<(String, String)> {
        MysqlPhysicalType::parse(left).type_drift_report(&MysqlPhysicalType::parse(right))
    }

    #[test]
    fn a_varchar_length_change_is_a_difference() {
        // Both sides fold to the portable `text`, so `data_type` alone reports
        // agreement. The contract is what tells them apart.
        assert!(!same("varchar(255)", "varchar(64)"));
        assert!(same("varchar(255)", "varchar(255)"));
    }

    #[test]
    fn the_renderer_spelling_and_the_catalog_spelling_agree() {
        // The renderer emits DECIMAL(65, 30); MySQL stores decimal(65,30). Reporting
        // drift on that pair would be a false red on a database nobody touched.
        assert!(same("DECIMAL(65, 30)", "decimal(65,30)"));
    }

    #[test]
    fn an_unmodelled_family_does_not_assert_a_difference_it_cannot_establish() {
        // `point` and `geometry` both parse to `Unknown`. A DIFFER must decline to
        // report a difference it cannot establish.
        assert!(same("point", "geometry"));
    }

    #[test]
    fn a_width_change_the_portable_type_cannot_see_is_spelled_both_ways() {
        // The defect this contract exists for: both sides read `text` portably, so
        // the report used to print two equal strings and the caller's equal-sides
        // guard dropped the entry.
        let (expected, actual) = report("text", "varchar(64)").expect("a difference to name");
        assert_ne!(
            expected, actual,
            "a TEXT -> VARCHAR(64) narrowing must print two sides a reader can tell apart"
        );
        assert_eq!(expected, "text");
        assert_eq!(actual, "varchar(64)");
    }

    #[test]
    fn two_equal_contracts_name_no_difference() {
        // `None` returns the caller to its portable pair rather than printing a line
        // that names nothing.
        assert_eq!(report("varchar(64)", "varchar(64)"), None);
    }
}
