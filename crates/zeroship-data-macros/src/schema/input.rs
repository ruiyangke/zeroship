use super::literal::Literal;
use std::collections::HashSet;
use syn::{
    braced, parenthesized,
    parse::{Parse, ParseStream},
    spanned::Spanned,
    Attribute, Ident, LitBool, LitInt, LitStr, Path, Token, Visibility,
};

pub struct Input {
    pub visibility: Visibility,
    pub name: Ident,
    pub collections: Vec<Collection>,
}
pub struct Collection {
    pub name: Ident,
    pub columns: Vec<Column>,
}
#[derive(Clone, Debug)]
pub struct Column {
    pub name: Ident,
    pub kind: Kind,
    pub nullable: bool,
    pub items: Option<Kind>,
    pub properties: Vec<Property>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
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
impl Kind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Integer => "Integer",
            Self::BigInt => "BigInt",
            Self::Number => "Number",
            Self::Boolean => "Boolean",
            Self::Bytes => "Bytes",
            Self::Timestamp => "Timestamp",
            Self::CalendarDate => "CalendarDate",
            Self::Time => "Time",
            Self::Json => "Json",
            Self::Object => "Object",
            Self::Array => "Array",
            Self::Union => "Union",
            Self::Vector => "Vector",
            Self::GeoPoint => "GeoPoint",
            Self::Enum => "Enum",
            Self::Literal => "Literal",
        }
    }
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let name: Ident = input.parse()?;
        match name.to_string().as_str() {
            "Text" => Ok(Self::Text),
            "Integer" => Ok(Self::Integer),
            "BigInt" => Ok(Self::BigInt),
            "Number" => Ok(Self::Number),
            "Boolean" => Ok(Self::Boolean),
            "Bytes" => Ok(Self::Bytes),
            "Timestamp" => Ok(Self::Timestamp),
            "CalendarDate" => Ok(Self::CalendarDate),
            "Time" => Ok(Self::Time),
            "Json" => Ok(Self::Json),
            "Object" => Ok(Self::Object),
            "Array" => Ok(Self::Array),
            "Union" => Ok(Self::Union),
            "Vector" => Ok(Self::Vector),
            "GeoPoint" => Ok(Self::GeoPoint),
            "Enum" => Ok(Self::Enum),
            "Literal" => Ok(Self::Literal),
            _ => Err(syn::Error::new(
                name.span(),
                "unsupported logical column type",
            )),
        }
    }
}
#[derive(Clone, Debug)]
pub enum Property {
    Bool(String, bool),
    Unsigned(String, u64),
    Text(String, String),
    Value(String, Literal),
    Number(String, Literal),
    Values(Vec<Literal>),
    Shape(Vec<Column>),
    Variants(Vec<Vec<Column>>),
    Assignment {
        event: Ident,
        generator: Ident,
        increment: Option<i64>,
    },
    Reference {
        collection: Ident,
        column: Ident,
    },
    Relation(Ident),
    Mask {
        kind: String,
        classification: String,
    },
    VectorMetric(Ident),
    ArrayStorage(ArrayStorage),
}

/// Physical storage declared for an array column: `"json"` or `"native"`.
#[derive(Clone, Copy, Debug)]
pub struct ArrayStorage {
    pub native: bool,
    pub span: proc_macro2::Span,
}
impl Column {
    pub fn flag(&self, name: &str, fallback: bool) -> bool {
        self.properties
            .iter()
            .find_map(|p| match p {
                Property::Bool(key, value) if key == name => Some(*value),
                _ => None,
            })
            .unwrap_or(fallback)
    }
    pub fn has_value(&self, name: &str) -> bool {
        self.properties
            .iter()
            .any(|p| matches!(p, Property::Value(key, _) if key == name))
    }
    pub fn assigned(&self) -> bool {
        self.properties
            .iter()
            .any(|p| matches!(p, Property::Assignment { .. }))
    }
    pub fn reference(&self) -> Option<(&Ident, &Ident)> {
        self.properties.iter().find_map(|p| match p {
            Property::Reference { collection, column } => Some((collection, column)),
            _ => None,
        })
    }
    pub fn relation(&self) -> Option<&Ident> {
        self.properties.iter().find_map(|p| match p {
            Property::Relation(name) => Some(name),
            _ => None,
        })
    }
    pub fn masked(&self) -> bool {
        self.properties
            .iter()
            .any(|p| matches!(p, Property::Mask { kind, .. } if kind != "none"))
    }
    pub fn array_storage(&self) -> Option<ArrayStorage> {
        self.properties.iter().find_map(|p| match p {
            Property::ArrayStorage(storage) => Some(*storage),
            _ => None,
        })
    }
    pub fn native_array(&self) -> bool {
        self.array_storage().is_some_and(|storage| storage.native)
    }
}

/// Native array storage is a physical column type: it applies to top-level
/// unprotected text arrays only, and JSON members stay JSON.
fn validate_array_storage(columns: &[Column], nested: bool) -> syn::Result<()> {
    for column in columns {
        if let Some(storage) = column.array_storage() {
            if nested {
                return Err(syn::Error::new(
                    storage.span,
                    "array_storage applies only to top-level columns",
                ));
            }
            if column.kind != Kind::Array {
                return Err(syn::Error::new(
                    storage.span,
                    "array_storage requires an Array column",
                ));
            }
            if column.native_array() && column.items != Some(Kind::Text) {
                return Err(syn::Error::new(
                    storage.span,
                    "native array storage supports Array<Text> only",
                ));
            }
            if column.native_array() && (column.flag("encrypted", false) || column.masked()) {
                return Err(syn::Error::new(
                    storage.span,
                    "native array storage cannot be encrypted or masked",
                ));
            }
        }
        for property in &column.properties {
            match property {
                Property::Shape(columns) => validate_array_storage(columns, true)?,
                Property::Variants(variants) => {
                    for columns in variants {
                        validate_array_storage(columns, true)?;
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}
pub fn spelling(name: &Ident) -> String {
    name.to_string().trim_start_matches("r#").to_owned()
}

impl Parse for Input {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let visibility = input.parse()?;
        let name = input.parse()?;
        let content;
        braced!(content in input);
        let mut collections = Vec::new();
        while !content.is_empty() {
            let name = content.parse()?;
            let fields;
            braced!(fields in content);
            let columns = parse_columns(&fields)?;
            collections.push(Collection { name, columns });
            if content.peek(Token![,]) {
                content.parse::<Token![,]>()?;
            }
        }
        if !input.is_empty() {
            return Err(input.error("unexpected schema declaration"));
        }
        let declaration = Self {
            visibility,
            name,
            collections,
        };
        declaration.validate()?;
        Ok(declaration)
    }
}

fn parse_columns(fields: ParseStream<'_>) -> syn::Result<Vec<Column>> {
    let mut columns = Vec::new();
    let mut names = HashSet::new();
    while !fields.is_empty() {
        let attributes = fields.call(Attribute::parse_outer)?;
        let name: Ident = fields.parse()?;
        if !names.insert(spelling(&name)) {
            return Err(syn::Error::new(name.span(), "duplicate column name"));
        }
        fields.parse::<Token![:]>()?;
        let nullable = fields.peek(Ident) && fields.fork().parse::<Ident>()? == "Nullable";
        if nullable {
            fields.parse::<Ident>()?;
            fields.parse::<Token![<]>()?;
        }
        let kind = Kind::parse(fields)?;
        let items = if kind == Kind::Array && fields.peek(Token![<]) {
            fields.parse::<Token![<]>()?;
            let item = Kind::parse(fields)?;
            fields.parse::<Token![>]>()?;
            Some(item)
        } else {
            None
        };
        if nullable {
            fields.parse::<Token![>]>()?;
        }
        columns.push(Column {
            name,
            kind,
            nullable,
            items,
            properties: parse_attributes(attributes)?,
        });
        if fields.is_empty() {
            break;
        }
        fields.parse::<Token![,]>()?;
    }
    Ok(columns)
}

fn parse_attributes(attributes: Vec<Attribute>) -> syn::Result<Vec<Property>> {
    let mut properties = Vec::new();
    let mut seen = HashSet::new();
    for attribute in attributes {
        if !attribute.path().is_ident("orm") {
            return Err(syn::Error::new(
                attribute.span(),
                "expected an orm column attribute",
            ));
        }
        attribute.parse_nested_meta(|meta| {
            let name = meta
                .path
                .get_ident()
                .ok_or_else(|| meta.error("expected a column option"))?
                .to_string();
            if !seen.insert(name.clone()) {
                return Err(meta.error("duplicate column option"));
            }
            let property = match name.as_str() {
                "primary_key" | "readable" | "projectable" | "filterable" | "sortable"
                | "writable" | "soft_delete" | "concurrency" | "encrypted" | "unique"
                | "case_sensitive" | "deferrable" | "raw_filterable" | "raw_sortable"
                | "raw_projectable" | "aggregateable" => {
                    let value = if meta.input.peek(Token![=]) {
                        meta.value()?.parse::<LitBool>()?.value
                    } else {
                        true
                    };
                    Property::Bool(name, value)
                }
                "max_length" | "char_len" | "precision" | "scale" | "vector_dims" => {
                    Property::Unsigned(name, meta.value()?.parse::<LitInt>()?.base10_parse()?)
                }
                "id_prefix" | "value_column" | "raw_column" | "format" | "pattern"
                | "on_delete" | "on_update" | "discriminator" => {
                    Property::Text(name, meta.value()?.parse::<LitStr>()?.value())
                }
                "default" | "generated" | "literal_value" | "identity" => {
                    Property::Value(name, meta.value()?.parse()?)
                }
                "min" | "max" => {
                    let value = meta.value()?.parse::<Literal>()?;
                    if !matches!(
                        value,
                        Literal::Signed(_) | Literal::Unsigned(_) | Literal::Float(_)
                    ) {
                        return Err(meta.error("constraint requires a number literal"));
                    }
                    Property::Number(name, value)
                }
                "enum_values" => match meta.value()?.parse::<Literal>()? {
                    Literal::Array(values) => Property::Values(values),
                    _ => return Err(meta.error("enum_values requires a literal array")),
                },
                "shape" => {
                    let content;
                    parenthesized!(content in meta.input);
                    Property::Shape(parse_columns(&content)?)
                }
                "variants" => {
                    let content;
                    parenthesized!(content in meta.input);
                    let mut variants = Vec::new();
                    while !content.is_empty() {
                        let fields;
                        braced!(fields in content);
                        variants.push(parse_columns(&fields)?);
                        if content.is_empty() {
                            break;
                        }
                        content.parse::<Token![,]>()?;
                    }
                    Property::Variants(variants)
                }
                "vector_metric" => {
                    let value: Ident = meta.value()?.parse()?;
                    if !matches!(
                        value.to_string().as_str(),
                        "cosine" | "l2" | "inner_product"
                    ) {
                        return Err(syn::Error::new(
                            value.span(),
                            "expected cosine, l2, or inner_product",
                        ));
                    }
                    Property::VectorMetric(value)
                }
                "array_storage" => {
                    let value: LitStr = meta.value()?.parse()?;
                    if !matches!(value.value().as_str(), "json" | "native") {
                        return Err(syn::Error::new(
                            value.span(),
                            "expected \"json\" or \"native\"",
                        ));
                    }
                    Property::ArrayStorage(ArrayStorage {
                        native: value.value() == "native",
                        span: value.span(),
                    })
                }
                "references" => {
                    let content;
                    parenthesized!(content in meta.input);
                    let target: Path = content.parse()?;
                    if !content.is_empty()
                        || target.leading_colon.is_some()
                        || target.segments.len() != 2
                        || target.segments.iter().any(|s| !s.arguments.is_empty())
                    {
                        return Err(meta.error("expected references(collection::column)"));
                    }
                    Property::Reference {
                        collection: target.segments[0].ident.clone(),
                        column: target.segments[1].ident.clone(),
                    }
                }
                "relation" => {
                    let content;
                    parenthesized!(content in meta.input);
                    let relation = content.parse()?;
                    if !content.is_empty() {
                        return Err(content.error("expected a relation name"));
                    }
                    Property::Relation(relation)
                }
                "assign" => {
                    let mut event = None;
                    let mut generator = None;
                    let mut increment = None;
                    meta.parse_nested_meta(|option| {
                        if option.path.is_ident("on") && event.is_none() {
                            let value: Ident = option.value()?.parse()?;
                            if !matches!(value.to_string().as_str(), "insert" | "write" | "delete")
                            {
                                return Err(syn::Error::new(
                                    value.span(),
                                    "expected insert, write, or delete",
                                ));
                            }
                            event = Some(value);
                        } else if option.path.is_ident("by") && generator.is_none() {
                            let value: Ident = option.value()?.parse()?;
                            match value.to_string().as_str() {
                                "now" | "typed_id" | "actor" | "identity" => {}
                                "increment" => {
                                    let content;
                                    parenthesized!(content in option.input);
                                    increment = Some(match content.parse::<Literal>()? {
                                        Literal::Signed(value) => value,
                                        _ => {
                                            return Err(content
                                                .error("increment requires a signed integer"))
                                        }
                                    });
                                    if !content.is_empty() {
                                        return Err(content.error("expected an increment amount"));
                                    }
                                }
                                _ => {
                                    return Err(syn::Error::new(
                                        value.span(),
                                        "unsupported assignment generator",
                                    ))
                                }
                            }
                            generator = Some(value);
                        } else {
                            return Err(option.error("unknown or duplicate assignment option"));
                        }
                        Ok(())
                    })?;
                    Property::Assignment {
                        event: event.ok_or_else(|| meta.error("assignment requires on"))?,
                        generator: generator.ok_or_else(|| meta.error("assignment requires by"))?,
                        increment,
                    }
                }
                "mask" => {
                    let mut kind = None;
                    let mut classification = None;
                    meta.parse_nested_meta(|option| {
                        let slot = if option.path.is_ident("kind") {
                            &mut kind
                        } else if option.path.is_ident("classification") {
                            &mut classification
                        } else {
                            return Err(option.error("expected kind or classification"));
                        };
                        if slot.is_some() {
                            return Err(option.error("duplicate mask option"));
                        }
                        *slot = Some(option.value()?.parse::<LitStr>()?.value());
                        Ok(())
                    })?;
                    Property::Mask {
                        kind: kind.ok_or_else(|| meta.error("mask requires kind"))?,
                        classification: classification
                            .ok_or_else(|| meta.error("mask requires classification"))?,
                    }
                }
                _ => return Err(meta.error("unsupported ORM column option")),
            };
            properties.push(property);
            Ok(())
        })?;
    }
    Ok(properties)
}

impl Input {
    fn validate(&self) -> syn::Result<()> {
        if self.collections.is_empty() {
            return Err(syn::Error::new(
                self.name.span(),
                "schema requires a collection",
            ));
        }
        let mut collections = HashSet::new();
        for collection in &self.collections {
            if !collections.insert(spelling(&collection.name)) {
                return Err(syn::Error::new(
                    collection.name.span(),
                    "duplicate collection name",
                ));
            }
            validate_array_storage(&collection.columns, false)?;
            let mut names = HashSet::new();
            for column in &collection.columns {
                names.insert(spelling(&column.name));
                if matches!(column.kind, Kind::Enum | Kind::Literal) {
                    return Err(syn::Error::new(
                        column.name.span(),
                        "logical type is only supported inside a JSON container",
                    ));
                }
            }
            let id = collection
                .columns
                .iter()
                .find(|c| spelling(&c.name) == "id")
                .ok_or_else(|| {
                    syn::Error::new(
                        collection.name.span(),
                        "collection requires an 'id' primary key",
                    )
                })?;
            if !id.flag("primary_key", false) {
                return Err(syn::Error::new(
                    id.name.span(),
                    "collection 'id' must be declared as its primary key",
                ));
            }
            if id.nullable {
                return Err(syn::Error::new(
                    id.name.span(),
                    "collection 'id' must be required and non-null",
                ));
            }
            if !matches!(id.kind, Kind::Text | Kind::Integer | Kind::BigInt) {
                return Err(syn::Error::new(
                    id.name.span(),
                    "collection 'id' must use text or integer storage",
                ));
            }
            if id.flag("encrypted", false) || id.masked() {
                return Err(syn::Error::new(
                    id.name.span(),
                    "collection 'id' cannot be encrypted or masked",
                ));
            }
            if id
                .properties
                .iter()
                .any(|p| matches!(p, Property::Assignment { event, .. } if event != "insert"))
            {
                return Err(syn::Error::new(
                    id.name.span(),
                    "collection 'id' can only be assigned on insertion",
                ));
            }
            if collection
                .columns
                .iter()
                .any(|c| spelling(&c.name) != "id" && c.flag("primary_key", false))
            {
                return Err(syn::Error::new(
                    collection.name.span(),
                    "collection 'id' must be its sole primary key",
                ));
            }
            let mut relations = HashSet::new();
            for column in &collection.columns {
                if let Some(relation) = column.relation() {
                    let name = spelling(relation);
                    if name.starts_with('_')
                        || name.len() > 63
                        || matches!(name.as_str(), "constructor" | "prototype")
                        || name.to_ascii_lowercase().starts_with("sqlite_")
                    {
                        return Err(syn::Error::new(
                            relation.span(),
                            "invalid or reserved relation name",
                        ));
                    }
                    if names.contains(&name) || !relations.insert(name) {
                        return Err(syn::Error::new(
                            relation.span(),
                            "relation name collides with another field or relation",
                        ));
                    }
                    if column.reference().is_none() {
                        return Err(syn::Error::new(
                            relation.span(),
                            "named relation requires a reference",
                        ));
                    }
                }
                if let Some((target, field)) = column.reference() {
                    let target = self
                        .collections
                        .iter()
                        .find(|c| spelling(&c.name) == spelling(target))
                        .ok_or_else(|| {
                            syn::Error::new(
                                target.span(),
                                "reference target collection does not exist",
                            )
                        })?;
                    if !target
                        .columns
                        .iter()
                        .any(|c| spelling(&c.name) == spelling(field))
                    {
                        return Err(syn::Error::new(
                            field.span(),
                            "reference target column does not exist",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}
