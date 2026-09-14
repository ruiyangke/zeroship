use proc_macro2::TokenStream;
use quote::quote;
use syn::Ident;

mod input;
mod literal;
#[cfg(test)]
mod native_tests;

pub use input::Input;
use input::{spelling, Column, Kind, Property};

pub fn expand(input: Input) -> syn::Result<TokenStream> {
    generate(input, &crate::engine()?)
}

fn metadata_path(orm: &syn::Path) -> syn::Path {
    let mut path = orm.clone();
    path.segments.pop();
    path.segments.push(syn::parse_quote!(schema));
    path
}

fn generate(input: Input, orm: &syn::Path) -> syn::Result<TokenStream> {
    let schema = metadata_path(orm);
    let Input {
        visibility,
        name,
        collections,
    } = input;
    let mut modules = Vec::new();
    let mut schema_entries = Vec::new();
    for collection in collections {
        let collection_name = spelling(&collection.name);
        let module = crate::identifier(&collection_name, collection.name.span())?;
        let mut columns = Vec::new();
        let mut relations = Vec::new();
        let mut constants = Vec::new();
        let mut required = Vec::new();
        let mut definitions = Vec::new();
        for definition in collection.columns {
            let field = spelling(&definition.name);
            let column = crate::identifier(&field, definition.name.span())?;
            let sql_type = sql_type(&definition, orm);
            let readable =
                definition.flag("readable", true) && definition.flag("projectable", true);
            let filterable =
                definition.flag("filterable", true) && !definition.flag("encrypted", false);
            let writable = definition.flag("writable", true)
                && !definition.assigned()
                && !definition.has_value("generated");
            let defaultable = writable && (definition.has_value("default") || definition.nullable);
            let read = readable.then(|| quote!(impl #orm::ReadableColumn for #column {}));
            let filter = filterable.then(|| quote!(impl #orm::FilterableColumn for #column {}));
            let write = writable.then(|| quote!(impl #orm::WritableColumn for #column {}));
            let update = (writable && field != "id")
                .then(|| quote!(impl #orm::UpdatableColumn for #column {}));
            let array = (definition.kind == Kind::Array).then(|| {
                let item = type_marker(
                    definition.items.unwrap_or(Kind::Json),
                    &definition.name,
                    orm,
                );
                quote!(impl #orm::ArrayColumn for #column { type ItemSqlType = #item; })
            });
            let default = defaultable.then(|| quote!(impl #orm::DefaultableColumn for #column {}));
            if writable && !defaultable {
                required.push(quote!(#orm::HasColumn<columns::#column>));
            }
            columns.push(quote! {
                #[allow(non_camel_case_types)]
                #[derive(Debug)]
                pub enum #column {}
                impl #orm::Column for #column {
                    type Entity = super::Entity;
                    type SqlType = #sql_type;
                    const NAME: &'static str = #field;
                }
                #read #filter #write #update #default #array
            });
            constants.push(quote! {
                #[allow(non_upper_case_globals)]
                pub const #column: #orm::Field<columns::#column> = #orm::Field::new();
            });
            if let (Some(relation), Some((target, target_column))) =
                (definition.relation(), definition.reference())
            {
                if readable {
                    let selector = crate::identifier(&spelling(relation), relation.span())?;
                    let target_module = crate::identifier(&spelling(target), target.span())?;
                    let relation = spelling(relation);
                    let target_column = spelling(target_column);
                    relations.push(quote! {
                        #[allow(non_camel_case_types)]
                        #[derive(Debug, Clone, Copy)]
                        pub struct #selector;
                        impl #orm::Relation for #selector {
                            type Source = super::Entity;
                            type Target = super::super::#target_module::Entity;
                            const NAME: &'static str = #relation;
                            const FIELD: &'static str = #field;
                            const TARGET_COLUMN: &'static str = #target_column;
                        }
                    });
                }
            }
            let metadata = column_schema(&definition, orm);
            definitions.push(quote!((::std::string::String::from(#field), #metadata)));
        }
        let complete = if required.is_empty() {
            quote!(
                impl<T> CompleteInsert for T {}
            )
        } else {
            quote!(impl<T> CompleteInsert for T where T: #(#required)+* {})
        };
        schema_entries.push(quote!((::std::string::String::from(#collection_name), <#module::Entity as #orm::Entity>::schema().clone())));
        modules.push(quote! {
            pub mod #module {
                #[derive(Debug)]
                pub enum Entity {}
                impl #orm::Entity for Entity {
                    const COLLECTION: &'static str = #collection_name;
                    fn schema() -> &'static #schema::CollectionSchema {
                        static SCHEMA: ::std::sync::OnceLock<#schema::CollectionSchema> = ::std::sync::OnceLock::new();
                        SCHEMA.get_or_init(|| #schema::CollectionSchema::new(::std::vec![#(#definitions),*]))
                    }
                }
                pub mod columns { #(#columns)* }
                pub mod relations { #(#relations)* }
                #(#constants)*
                #[doc(hidden)]
                pub trait CompleteInsert {}
                #complete
            }
        });
    }
    Ok(quote! {
        #visibility mod #name {
            #(#modules)*
            pub fn schema() -> #schema::Schema {
                #schema::Schema::new(::std::vec![#(#schema_entries),*])
            }
        }
    })
}

fn sql_type(column: &Column, orm: &syn::Path) -> TokenStream {
    let ty = if column.kind == Kind::Array {
        let item = type_marker(column.items.unwrap_or(Kind::Json), &column.name, orm);
        quote!(#orm::sql_types::Array<#item>)
    } else {
        type_marker(column.kind, &column.name, orm)
    };
    if column.nullable {
        quote!(#orm::sql_types::Nullable<#ty>)
    } else {
        ty
    }
}

fn type_marker(kind: Kind, name: &Ident, orm: &syn::Path) -> TokenStream {
    let marker = match kind {
        Kind::Object | Kind::Array | Kind::Union | Kind::Json | Kind::Enum | Kind::Literal => {
            "Json"
        }
        kind => kind.name(),
    };
    let marker = Ident::new(marker, name.span());
    quote!(#orm::sql_types::#marker)
}

fn column_schema(column: &Column, orm: &syn::Path) -> TokenStream {
    let schema = metadata_path(orm);
    let kind = Ident::new(column.kind.name(), column.name.span());
    let required = !column.nullable;
    let mut fields = vec![quote!(required: #required)];
    if let Some(items) = column.items {
        let items = Ident::new(items.name(), column.name.span());
        fields.push(quote!(items: Some(#schema::LogicalType::#items)));
    }
    let mut storage = Vec::new();
    for property in &column.properties {
        match property {
            Property::Bool(name, value) => {
                let name = Ident::new(name, column.name.span());
                let field = quote!(#name: #value);
                if matches!(name.to_string().as_str(), "raw_filterable" | "raw_sortable" | "raw_projectable") { storage.push(field); }
                else { fields.push(field); }
            }
            Property::Unsigned(name, value) => {
                let name = Ident::new(name, column.name.span());
                if name == "vector_dims" { let value = syn::LitInt::new(&value.to_string(), column.name.span()); fields.push(quote!(#name: Some(#value))); }
                else { fields.push(quote!(#name: Some(#value))); }
            }
            Property::Text(name, value) => {
                let name = Ident::new(name, column.name.span());
                let field = quote!(#name: Some(::std::string::String::from(#value)));
                if name == "value_column" || name == "raw_column" { storage.push(field); }
                else { fields.push(field); }
            }
            Property::Value(name, value) => {
                let name = Ident::new(name, column.name.span());
                let value = value.tokens(orm);
                fields.push(quote!(#name: Some(#value)));
            }
            Property::Number(name, value) => {
                let name = Ident::new(name, column.name.span());
                let value = match value {
                    literal::Literal::Signed(value) => quote!(#schema::Number::from(#value)),
                    literal::Literal::Unsigned(value) => quote!(#schema::Number::from(#value)),
                    literal::Literal::Float(value) => quote!(#schema::Number::from_f64(#value).expect("finite schema literal")),
                    _ => unreachable!("validated number constraint"),
                };
                fields.push(quote!(#name: Some(#value)));
            }
            Property::Values(values) => {
                let values = values.iter().map(|value| value.tokens(orm));
                fields.push(quote!(enum_values: ::std::vec![#(#values),*]));
            }
            Property::Shape(columns) => {
                let shape = field_map(columns, orm);
                fields.push(quote!(shape: #shape));
            }
            Property::Variants(variants) => {
                let variants = variants.iter().map(|columns| field_map(columns, orm));
                fields.push(quote!(variants: ::std::vec![#(#variants),*]));
            }
            Property::Assignment { event, generator, increment } => {
                let event = Ident::new(match event.to_string().as_str() { "insert" => "Insert", "write" => "Write", _ => "Delete" }, event.span());
                let generator = match generator.to_string().as_str() {
                    "now" => quote!(#schema::AssignmentGenerator::Now),
                    "typed_id" => quote!(#schema::AssignmentGenerator::TypedId),
                    "actor" => quote!(#schema::AssignmentGenerator::Actor),
                    "identity" => quote!(#schema::AssignmentGenerator::Identity),
                    _ => { let amount = increment.expect("validated increment"); quote!(#schema::AssignmentGenerator::Increment(#amount)) }
                };
                fields.push(quote!(assignment: Some(#schema::Assignment { by: #generator, on: #schema::AssignmentEvent::#event })));
            }
            Property::Reference { collection, column: target } => {
                let collection = spelling(collection);
                let target = spelling(target);
                let relation = column.relation().map(spelling);
                let name = relation.map_or_else(|| quote!(None), |name| quote!(Some(::std::string::String::from(#name))));
                fields.push(quote!(reference: Some(#schema::RelationSchema { collection: ::std::string::String::from(#collection), column: ::std::string::String::from(#target), name: #name })));
            }
            Property::Relation(_) => {}
            Property::Mask { kind, classification } => fields.push(quote!(mask: Some(#schema::MaskSchema { kind: ::std::string::String::from(#kind), classification: ::std::string::String::from(#classification) }))),
            Property::VectorMetric(metric) => {
                let metric = Ident::new(match metric.to_string().as_str() { "cosine" => "Cosine", "l2" => "L2", _ => "InnerProduct" }, metric.span());
                fields.push(quote!(vector_metric: #schema::VectorMetric::#metric));
            }
            Property::ArrayStorage(array_storage) => {
                let representation = Ident::new(if array_storage.native { "Native" } else { "Json" }, array_storage.span);
                storage.push(quote!(array: #schema::ArrayStorage::#representation));
            }
        }
    }
    if !storage.is_empty() {
        fields.push(quote!(storage: #schema::StorageMapping { #(#storage),*, ..::core::default::Default::default() }));
    }
    quote!(#schema::ColumnSchema { #(#fields),*, ..#schema::ColumnSchema::new(#schema::LogicalType::#kind) })
}

fn field_map(columns: &[Column], orm: &syn::Path) -> TokenStream {
    let fields = columns.iter().map(|column| {
        let name = spelling(&column.name);
        let metadata = column_schema(column, orm);
        quote!((::std::string::String::from(#name), #metadata))
    });
    quote!(::core::iter::IntoIterator::into_iter(::std::vec![#(#fields),*]).collect())
}
