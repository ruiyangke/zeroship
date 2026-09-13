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

fn generate(input: Input, orm: &syn::Path) -> syn::Result<TokenStream> {
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
                #read #filter #write #update #default
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
                    fn schema() -> &'static #orm::schema::CollectionSchema {
                        static SCHEMA: ::std::sync::OnceLock<#orm::schema::CollectionSchema> = ::std::sync::OnceLock::new();
                        SCHEMA.get_or_init(|| #orm::schema::CollectionSchema::new([#(#definitions),*]))
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
            pub fn schema() -> #orm::schema::Schema {
                #orm::schema::Schema::new([#(#schema_entries),*])
            }
        }
    })
}

fn sql_type(column: &Column, orm: &syn::Path) -> TokenStream {
    let marker = match column.kind {
        Kind::Object | Kind::Array | Kind::Union | Kind::Json => "Json",
        Kind::Enum | Kind::Literal => unreachable!("validated top-level column type"),
        kind => kind.name(),
    };
    let marker = Ident::new(marker, column.name.span());
    let ty = quote!(#orm::sql_types::#marker);
    if column.nullable {
        quote!(#orm::sql_types::Nullable<#ty>)
    } else {
        ty
    }
}

fn column_schema(column: &Column, orm: &syn::Path) -> TokenStream {
    let kind = Ident::new(column.kind.name(), column.name.span());
    let required = !column.nullable;
    let mut fields = vec![quote!(required: #required)];
    if let Some(items) = column.items {
        let items = Ident::new(items.name(), column.name.span());
        fields.push(quote!(items: Some(#orm::schema::LogicalType::#items)));
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
                    literal::Literal::Signed(value) => quote!(#orm::schema::Number::from(#value)),
                    literal::Literal::Unsigned(value) => quote!(#orm::schema::Number::from(#value)),
                    literal::Literal::Float(value) => quote!(#orm::schema::Number::from_f64(#value).expect("finite schema literal")),
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
                    "now" => quote!(#orm::schema::AssignmentGenerator::Now),
                    "typed_id" => quote!(#orm::schema::AssignmentGenerator::TypedId),
                    "actor" => quote!(#orm::schema::AssignmentGenerator::Actor),
                    "identity" => quote!(#orm::schema::AssignmentGenerator::Identity),
                    _ => { let amount = increment.expect("validated increment"); quote!(#orm::schema::AssignmentGenerator::Increment(#amount)) }
                };
                fields.push(quote!(assignment: Some(#orm::schema::Assignment { by: #generator, on: #orm::schema::AssignmentEvent::#event })));
            }
            Property::Reference { collection, column: target } => {
                let collection = spelling(collection);
                let target = spelling(target);
                let relation = column.relation().map(spelling);
                let name = relation.map_or_else(|| quote!(None), |name| quote!(Some(::std::string::String::from(#name))));
                fields.push(quote!(reference: Some(#orm::schema::RelationSchema { collection: ::std::string::String::from(#collection), column: ::std::string::String::from(#target), name: #name })));
            }
            Property::Relation(_) => {}
            Property::Mask { kind, classification } => fields.push(quote!(mask: Some(#orm::schema::MaskSchema { kind: ::std::string::String::from(#kind), classification: ::std::string::String::from(#classification) }))),
            Property::VectorMetric(metric) => {
                let metric = Ident::new(match metric.to_string().as_str() { "cosine" => "Cosine", "l2" => "L2", _ => "InnerProduct" }, metric.span());
                fields.push(quote!(vector_metric: #orm::schema::VectorMetric::#metric));
            }
        }
    }
    if !storage.is_empty() {
        fields.push(quote!(storage: #orm::schema::StorageMapping { #(#storage),*, ..::core::default::Default::default() }));
    }
    quote!(#orm::schema::ColumnSchema { #(#fields),*, ..#orm::schema::ColumnSchema::new(#orm::schema::LogicalType::#kind) })
}

fn field_map(columns: &[Column], orm: &syn::Path) -> TokenStream {
    let fields = columns.iter().map(|column| {
        let name = spelling(&column.name);
        let metadata = column_schema(column, orm);
        quote!((::std::string::String::from(#name), #metadata))
    });
    quote!(::core::iter::IntoIterator::into_iter([#(#fields),*]).collect())
}
