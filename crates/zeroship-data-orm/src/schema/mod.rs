//! Native model contracts shared by Rust declarations and creator artifacts.

use std::{collections::HashSet, ops::Deref};

pub use crate::value::Number;
use crate::{error::DbError, value::Value};
pub use zeroship_migrate_policy::{Assignment, AssignmentEvent, AssignmentGenerator};

mod decode;
mod validate;

pub type FieldMap = indexmap::IndexMap<String, ColumnSchema>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalType {
    Text,
    Integer,
    BigInt,
    Number,
    Boolean,
    Bytes,
    Timestamp,
    CalendarDate,
    Time,
    Json,
    Object,
    Array,
    Union,
    Vector,
    GeoPoint,
    Enum,
    Literal,
}

impl LogicalType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "string",
            Self::Integer => "integer",
            Self::BigInt => "bigInt",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Bytes => "bytes",
            Self::Timestamp => "timestamp",
            Self::CalendarDate => "calendarDate",
            Self::Time => "time",
            Self::Json => "json",
            Self::Object => "object",
            Self::Array => "array",
            Self::Union => "union",
            Self::Vector => "vector",
            Self::GeoPoint => "geoPoint",
            Self::Enum => "enum",
            Self::Literal => "literal",
        }
    }

    pub const fn is_integer(self) -> bool {
        matches!(self, Self::Integer | Self::BigInt)
    }

    pub const fn is_json(self) -> bool {
        matches!(self, Self::Json | Self::Object | Self::Array | Self::Union)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VectorMetric {
    #[default]
    Cosine,
    L2,
    InnerProduct,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageMapping {
    pub value_column: Option<String>,
    pub raw_column: Option<String>,
    pub raw_filterable: bool,
    pub raw_sortable: bool,
    pub raw_projectable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaskSchema {
    pub kind: String,
    pub classification: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelationSchema {
    pub collection: String,
    pub column: String,
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnSchema {
    pub logical_type: LogicalType,
    pub required: bool,
    pub primary_key: bool,
    pub unique: bool,
    pub readable: bool,
    pub projectable: bool,
    pub filterable: bool,
    pub sortable: bool,
    pub aggregateable: bool,
    pub writable: bool,
    pub assignment: Option<Assignment>,
    pub default: Option<Value>,
    pub generated: Option<Value>,
    pub identity: Option<Value>,
    pub reference: Option<RelationSchema>,
    pub on_delete: Option<String>,
    pub on_update: Option<String>,
    pub deferrable: bool,
    pub soft_delete: bool,
    pub concurrency: bool,
    pub encrypted: bool,
    pub mask: Option<MaskSchema>,
    pub id_prefix: Option<String>,
    pub storage: StorageMapping,
    pub max_length: Option<u64>,
    pub char_len: Option<u64>,
    pub min: Option<Number>,
    pub max: Option<Number>,
    pub enum_values: Vec<Value>,
    pub case_sensitive: bool,
    pub format: Option<String>,
    pub pattern: Option<String>,
    pub precision: Option<u64>,
    pub scale: Option<u64>,
    pub items: Option<LogicalType>,
    pub vector_dims: Option<usize>,
    pub vector_metric: VectorMetric,
    pub shape: FieldMap,
    pub variants: Vec<FieldMap>,
    pub discriminator: Option<String>,
    pub literal_value: Option<Value>,
}

impl ColumnSchema {
    pub fn new(logical_type: LogicalType) -> Self {
        Self {
            logical_type,
            required: true,
            primary_key: false,
            unique: false,
            readable: true,
            projectable: true,
            filterable: true,
            sortable: true,
            aggregateable: true,
            writable: true,
            assignment: None,
            default: None,
            generated: None,
            identity: None,
            reference: None,
            on_delete: None,
            on_update: None,
            deferrable: false,
            soft_delete: false,
            concurrency: false,
            encrypted: false,
            mask: None,
            id_prefix: None,
            storage: StorageMapping::default(),
            max_length: None,
            char_len: None,
            min: None,
            max: None,
            enum_values: Vec::new(),
            case_sensitive: true,
            format: None,
            pattern: None,
            precision: None,
            scale: None,
            items: None,
            vector_dims: None,
            vector_metric: VectorMetric::default(),
            shape: FieldMap::new(),
            variants: Vec::new(),
            discriminator: None,
            literal_value: None,
        }
    }

    pub fn is_readable(&self) -> bool {
        self.readable && self.projectable
    }

    pub fn is_generated(&self) -> bool {
        self.assignment.is_some() || self.generated.is_some()
    }

    pub fn is_writable(&self) -> bool {
        self.writable && !self.is_generated()
    }

    pub fn is_masked(&self) -> bool {
        self.mask.as_ref().is_some_and(|mask| mask.kind != "none")
    }

    fn normalize_defaults(&mut self) {
        if self.is_generated() {
            self.writable = false;
        }
        if self.mask.as_ref().is_some_and(|mask| mask.kind == "none") {
            self.mask = None;
        }
        if self.logical_type == LogicalType::Number && self.precision.is_some() {
            self.scale.get_or_insert(0);
        }
        if self.logical_type == LogicalType::Array {
            self.items.get_or_insert(LogicalType::Json);
        }
    }

    fn normalize(&mut self, name: &str) {
        self.normalize_defaults();
        self.storage.value_column.get_or_insert_with(|| name.into());
        if self.is_masked() {
            self.storage
                .raw_column
                .get_or_insert_with(|| crate::sql::mapping::raw_column_name(name));
        }
        normalize_fields(&mut self.shape);
        for variant in &mut self.variants {
            normalize_fields(variant);
        }
        if let Some(mut default) = self.default.clone() {
            if crate::sql::codecs::prepare_value(name, self, &mut default).is_ok() {
                self.default = Some(default);
            }
        }
    }
}

fn normalize_fields(fields: &mut FieldMap) {
    for (name, column) in fields {
        column.normalize(name);
    }
}

/// A declared collection. Registration validates its identity and references.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CollectionSchema {
    fields: FieldMap,
    duplicate_field: Option<String>,
}

impl CollectionSchema {
    pub fn new(fields: impl IntoIterator<Item = (String, ColumnSchema)>) -> Self {
        let mut definitions = FieldMap::new();
        let mut duplicate_field = None;
        for (name, column) in fields {
            if definitions.insert(name.clone(), column).is_some() {
                duplicate_field.get_or_insert(name);
            }
        }
        normalize_fields(&mut definitions);
        Self {
            fields: definitions,
            duplicate_field,
        }
    }

    pub fn fields(&self) -> &FieldMap {
        &self.fields
    }

    pub fn into_fields(self) -> FieldMap {
        self.fields
    }
}

impl Deref for CollectionSchema {
    type Target = FieldMap;

    fn deref(&self) -> &Self::Target {
        self.fields()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Schema {
    collections: Vec<(String, CollectionSchema)>,
}

impl Schema {
    pub fn new(collections: impl IntoIterator<Item = (String, CollectionSchema)>) -> Self {
        Self {
            collections: collections.into_iter().collect(),
        }
    }

    pub fn collections(&self) -> impl Iterator<Item = (&str, &CollectionSchema)> {
        self.collections
            .iter()
            .map(|(name, schema)| (name.as_str(), schema))
    }

    pub fn into_collections(self) -> Vec<(String, CollectionSchema)> {
        self.collections
    }

    pub fn validate(&self) -> Result<(), DbError> {
        let mut names = HashSet::new();
        for (name, schema) in &self.collections {
            crate::sql::mapping::validate_collection(name)?;
            if !names.insert(name) {
                return Err(invalid("duplicate collection declaration"));
            }
            validate::collection(name, schema)?;
        }
        validate::relations(self)
    }
}

fn invalid(message: impl Into<String>) -> DbError {
    DbError::validation("invalid_schema", message)
}
