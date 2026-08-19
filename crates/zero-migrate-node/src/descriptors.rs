//! The boundary `CollectionDescriptor` mirror, in BOTH directions.
//!
//! INBOUND (`*_dto_to_engine`) is the manual `genArtifacts` source: a host declares a
//! schema and the producer turns it into `createTable` ops. OUTBOUND (`*_to_dto`) is
//! the structured export: the folded schema handed back as typed collections, so a
//! host can render its own files rather than re-parsing `schema.runtime.json` out of
//! the string the same reply carries.
//!
//! # Why this is a module and not two halves in two places
//!
//! The inbound half used to live in [`crate::bridge`], which is `napi`-gated. That was
//! right while the conversion was inbound-only — nothing but the N-API entrypoint
//! needed it. It stops being right the moment an EXPORT exists, for a reason about
//! measurement rather than tidiness: the only oracle that can catch a facet the wire
//! drops is `engine -> dto -> engine`, and a test that cannot see
//! [`crate::descriptors::field_dto_to_engine`] would have to re-implement it — at which
//! point the test measures its own copy and the addon's real conversion stays
//! unmeasured. (The ABSOLUTE path is load-bearing: a `//!` block resolves in the
//! PARENT module's scope, so a bare `[`field_dto_to_engine`]` here does not resolve to
//! the function two screens below it. That is the trap `single_fold.rs` documents, and
//! it caught this file too.) So both
//! directions live here, napi-free, where `cargo test -p zero-migrate-node
//! --no-default-features` reaches them and `bridge` calls the same functions the test
//! does.
//!
//! # What the round trip is, and is not
//!
//! `tests/collection_export_round_trip.rs` pins `engine -> dto -> engine` as the
//! IDENTITY over a folded corpus. That is a claim about the WIRE: every facet the fold
//! recovered survives the crossing. It is NOT a claim that re-importing the export
//! reproduces the original schema — the descriptor-to-ops producer is separately lossy
//! (`token_to_col_type` maps every `"string"` to `ColType::Text`, so a `VARCHAR(n)`
//! width crosses this wire intact and is then dropped by the producer), and it is NOT
//! a claim that the exported values are CORRECT, only that they are preserved.

use zero_migrate::render::declarative::{CollectionDescriptor, FieldDescriptor, IndexDescriptor};

use crate::wire::{
    CollectionDescriptorDto, FieldDescriptorDto, IndexDescriptorDto, RuntimeOptionsDto,
};

// ---------------------------------------------------------------------------
// INBOUND — boundary DTO to engine descriptor.
// ---------------------------------------------------------------------------

/// Convert a boundary [`CollectionDescriptorDto`] into the engine's declarative
/// `CollectionDescriptor`. The rich sub-object facets crossed as `JsonValue`, so
/// they `serde_json::from_value` into their closed engine types verbatim.
///
/// # Errors
/// A malformed `generated`/`identity` sub-object, or an unknown `strictness` token.
pub fn descriptor_dto_to_engine(
    dto: CollectionDescriptorDto,
) -> std::result::Result<CollectionDescriptor, String> {
    let fields = dto
        .fields
        .into_iter()
        .map(field_dto_to_engine)
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let indexes = dto
        .indexes
        .unwrap_or_default()
        .into_iter()
        .map(|i| IndexDescriptor {
            name: i.name,
            columns: i.columns,
            unique: i.unique.unwrap_or(false),
        })
        .collect();

    let runtime_options = runtime_options_dto_to_engine(dto.runtime_options)?;

    Ok(CollectionDescriptor {
        name: dto.name,
        owner_app: dto.owner_app,
        fields,
        indexes,
        runtime_options,
    })
}

/// Convert one boundary [`FieldDescriptorDto`] into the engine's `FieldDescriptor`.
///
/// # Errors
/// A `generated`/`identity` value that is not the closed engine facet shape.
pub fn field_dto_to_engine(
    dto: FieldDescriptorDto,
) -> std::result::Result<FieldDescriptor, String> {
    use zero_migrate::model::ir::{GeneratedCol, IdentityCol};

    let generated: Option<GeneratedCol> = match dto.generated {
        Some(v) => Some(
            serde_json::from_value(v)
                .map_err(|e| format!("field {:?}: invalid `generated` facet: {e}", dto.name))?,
        ),
        None => None,
    };
    let identity: Option<IdentityCol> = match dto.identity {
        Some(v) => Some(
            serde_json::from_value(v)
                .map_err(|e| format!("field {:?}: invalid `identity` facet: {e}", dto.name))?,
        ),
        None => None,
    };

    Ok(FieldDescriptor {
        name: dto.name,
        ty: dto.ty,
        required: dto.required.unwrap_or(false),
        unique: dto.unique.unwrap_or(false),
        references: dto.references,
        reference_column: dto.reference_column,
        reference_name: dto.reference_name,
        on_delete: dto.on_delete,
        on_update: dto.on_update,
        deferrable: dto.deferrable,
        // NOT on the wire, and unlike `unbounded_text` this one is not a derivation
        // but a dead end: no `ColType` carries a literal, so the fold never recovers
        // one, and the producer refuses the `"literal"` token outright — a wire slot
        // for it would be surface neither `genArtifacts` source can populate. Both
        // halves of that are measured by
        // `tests/collection_export_round_trip.rs::a_literal_field_is_unreachable_…`,
        // which turns red if either stops holding.
        literal_value: None,
        default: dto.default,
        min: dto.min,
        max: dto.max,
        enum_values: dto.enum_values,
        id_prefix: dto.id_prefix,
        vector_dims: dto.vector_dims,
        char_len: dto.char_len,
        max_length: dto.max_length,
        precision: dto.precision,
        scale: dto.scale,
        // NOT on the wire, and this is the one facet the round trip deliberately does
        // not carry. The engine marks it `#[serde(skip)]` and calls it render-only: it
        // is DERIVED by `ir_column_to_field` from the column's own `ColType::Text` plus
        // the absence of a value-format / id-prefix facet, and re-derived identically
        // the next time the same column is folded. Putting it on the wire would let a
        // host set an unbounded-TEXT spelling independently of the type that implies
        // it. `tests/collection_export_round_trip.rs` measures the re-derivation rather
        // than asserting it here.
        unbounded_text: false,
        vector_metric: dto.vector_metric,
        case_sensitive: dto.case_sensitive,
        encrypted: dto.encrypted,
        mask: dto.mask,
        fts: dto.fts.unwrap_or(false),
        fts_language: dto.fts_language,
        generated,
        identity,
    })
}

fn runtime_options_dto_to_engine(
    dto: Option<RuntimeOptionsDto>,
) -> std::result::Result<zero_migrate::TableRuntimeOptions, String> {
    use zero_migrate::{TableRuntimeOptions, TableStrictness};
    let Some(dto) = dto else {
        return Ok(TableRuntimeOptions::default());
    };
    let strictness = match dto.strictness.as_deref() {
        None | Some("strict") => TableStrictness::Strict,
        Some("lenient") => TableStrictness::Lenient,
        Some("off") => TableStrictness::Off,
        Some(other) => {
            return Err(format!(
                "invalid runtime option strictness {other:?} (expected strict|lenient|off)"
            ))
        }
    };
    Ok(TableRuntimeOptions {
        soft_delete: dto.soft_delete.unwrap_or(false),
        versioning: dto.versioning.unwrap_or(false),
        strictness,
    })
}

// ---------------------------------------------------------------------------
// OUTBOUND — engine descriptor to boundary DTO.
// ---------------------------------------------------------------------------

/// Project an engine `CollectionDescriptor` onto the boundary DTO for export.
///
/// Infallible: every engine facet either has a DTO slot or is documented above as
/// deliberately absent, and the two typed sub-object facets serialize by construction.
#[must_use]
pub fn descriptor_to_dto(descriptor: &CollectionDescriptor) -> CollectionDescriptorDto {
    CollectionDescriptorDto {
        name: descriptor.name.clone(),
        owner_app: descriptor.owner_app.clone(),
        fields: descriptor.fields.iter().map(field_to_dto).collect(),
        // Always `Some`, never a collapsed `None` for the empty case. The inbound
        // direction reads absent and empty alike, but a CONSUMER reading an export
        // must be able to tell "this collection has no named indexes" from "this
        // addon does not report indexes", and only presence carries that.
        indexes: Some(
            descriptor
                .indexes
                .iter()
                .map(|index| IndexDescriptorDto {
                    name: index.name.clone(),
                    columns: index.columns.clone(),
                    unique: Some(index.unique),
                })
                .collect(),
        ),
        runtime_options: Some(RuntimeOptionsDto {
            soft_delete: Some(descriptor.runtime_options.soft_delete),
            versioning: Some(descriptor.runtime_options.versioning),
            strictness: Some(
                match descriptor.runtime_options.strictness {
                    zero_migrate::TableStrictness::Strict => "strict",
                    zero_migrate::TableStrictness::Lenient => "lenient",
                    zero_migrate::TableStrictness::Off => "off",
                }
                .to_string(),
            ),
        }),
    }
}

/// Project one engine `FieldDescriptor` onto the boundary DTO.
///
/// The `Option<bool>` slots are emitted EXPLICITLY (`Some(false)`, not `None`) where
/// the engine holds a plain `bool`. Collapsing a false to absent would be lossless for
/// the inbound direction — which reads absent as false — but an export is read by a
/// consumer that has no such rule, and `required: undefined` invites the reading
/// "unknown" for a column the fold knows is nullable.
#[must_use]
pub fn field_to_dto(field: &FieldDescriptor) -> FieldDescriptorDto {
    FieldDescriptorDto {
        name: field.name.clone(),
        ty: field.ty.clone(),
        required: Some(field.required),
        unique: Some(field.unique),
        references: field.references.clone(),
        reference_column: field.reference_column.clone(),
        reference_name: field.reference_name.clone(),
        on_delete: field.on_delete.clone(),
        on_update: field.on_update.clone(),
        deferrable: field.deferrable,
        default: field.default.clone(),
        min: field.min,
        max: field.max,
        enum_values: field.enum_values.clone(),
        id_prefix: field.id_prefix.clone(),
        vector_dims: field.vector_dims,
        char_len: field.char_len,
        max_length: field.max_length,
        precision: field.precision,
        scale: field.scale,
        vector_metric: field.vector_metric.clone(),
        case_sensitive: field.case_sensitive,
        encrypted: field.encrypted.clone(),
        mask: field.mask.clone(),
        fts: Some(field.fts),
        fts_language: field.fts_language.clone(),
        generated: field
            .generated
            .as_ref()
            .map(|g| serde_json::to_value(g).expect("GeneratedCol serializes")),
        identity: field
            .identity
            .as_ref()
            .map(|i| serde_json::to_value(i).expect("IdentityCol serializes")),
    }
}
