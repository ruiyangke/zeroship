use proc_macro2::TokenStream;
use quote::quote;
use std::collections::HashSet;
use syn::{spanned::Spanned, Data, DeriveInput, Fields, LitStr, Path};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Read,
    Insert,
    Update,
}

pub fn expand(input: DeriveInput, kind: Kind) -> syn::Result<TokenStream> {
    let orm = crate::engine()?;
    let mut entity: Option<Path> = None;
    for attr in input
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("orm"))
    {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("entity") {
                if entity.is_some() {
                    return Err(meta.error("duplicate entity attribute"));
                }
                entity = Some(meta.value()?.parse()?);
                Ok(())
            } else {
                Err(meta.error("expected entity = path::to::collection"))
            }
        })?;
    }
    let entity = entity.ok_or_else(|| {
        syn::Error::new(
            input.span(),
            "missing #[orm(entity = path::to::collection)]",
        )
    })?;
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new(
            input.span(),
            "ORM derives require a named-field struct",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new(
            data.fields.span(),
            "ORM derives require named fields",
        ));
    };
    if fields.named.is_empty() {
        return Err(syn::Error::new(
            fields.span(),
            "ORM mappings must declare a field",
        ));
    }
    let name = &input.ident;
    let mut generics = input.generics.clone();
    let mut statements = Vec::new();
    let mut columns = Vec::new();
    let mut presence = Vec::new();
    let mut seen = HashSet::new();
    for field in &fields.named {
        let ident = field.ident.as_ref().unwrap();
        let ty = &field.ty;
        let mut column_name: Option<LitStr> = None;
        let mut default = false;
        let mut decode_conversion: Option<Path> = None;
        let mut encode_conversion: Option<Path> = None;
        for attr in field
            .attrs
            .iter()
            .filter(|attr| attr.path().is_ident("orm"))
        {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("column") {
                    if column_name.is_some() {
                        return Err(meta.error("duplicate column attribute"));
                    }
                    column_name = Some(meta.value()?.parse()?);
                    Ok(())
                } else if meta.path.is_ident("default") && kind == Kind::Insert {
                    if default {
                        return Err(meta.error("duplicate default attribute"));
                    }
                    default = true;
                    Ok(())
                } else if meta.path.is_ident("decode_with") || meta.path.is_ident("encode_with") {
                    let conversion = if meta.path.is_ident("decode_with") {
                        &mut decode_conversion
                    } else {
                        &mut encode_conversion
                    };
                    if conversion.is_some() {
                        return Err(meta.error("duplicate conversion attribute"));
                    }
                    *conversion = Some(meta.value()?.parse()?);
                    Ok(())
                } else {
                    Err(meta.error("unsupported ORM field attribute for this derive"))
                }
            })?;
        }
        let spelling = column_name
            .as_ref()
            .map(LitStr::value)
            .unwrap_or_else(|| ident.to_string().trim_start_matches("r#").to_string());
        if !seen.insert(spelling.clone()) {
            return Err(syn::Error::new(
                field.span(),
                format!("column '{spelling}' is mapped more than once"),
            ));
        }
        let column = crate::identifier(&spelling, field.span())?;
        let column = quote!(#entity::columns::#column);
        let sql = quote!(<#column as #orm::Column>::SqlType);
        let predicates = &mut generics.make_where_clause().predicates;
        match kind {
            Kind::Read => {
                predicates.push(syn::parse_quote!(#column: #orm::ReadableColumn));
                if let Some(convert) = &decode_conversion {
                    statements.push(quote!(#ident: row.take_with::<#column, _, #ty>(#convert)?));
                } else {
                    predicates.push(syn::parse_quote!(#ty: #orm::DecodeValue<#sql>));
                    statements.push(quote!(#ident: row.take::<#column, #ty>()?));
                }
                columns.push(quote!(<#column as #orm::Column>::NAME));
            }
            Kind::Insert => {
                if default {
                    predicates.push(syn::parse_quote!(#column: #orm::DefaultableColumn));
                    if let Some(convert) = &encode_conversion {
                        statements.push(quote!(#orm::encode_default_with::<#column, _, _>(&mut record, self.#ident, #convert)?;));
                    } else {
                        predicates.push(syn::parse_quote!(#ty: #orm::DefaultInput<#column>));
                        statements.push(quote!(<#ty as #orm::DefaultInput<#column>>::encode_default(self.#ident, &mut record)?;));
                    }
                } else {
                    predicates.push(syn::parse_quote!(#column: #orm::WritableColumn));
                    if let Some(convert) = &encode_conversion {
                        statements.push(quote!(#orm::encode_field_with::<#column, _, _>(&mut record, self.#ident, #convert)?;));
                    } else {
                        predicates.push(syn::parse_quote!(#ty: #orm::EncodeValue<#sql>));
                        statements.push(
                            quote!(#orm::encode_field::<#column, #ty>(&mut record, self.#ident)?;),
                        );
                    }
                }
                presence.push(column);
            }
            Kind::Update => {
                predicates.push(syn::parse_quote!(#column: #orm::UpdatableColumn));
                if let Some(convert) = &encode_conversion {
                    statements.push(quote!(#orm::encode_change_with::<#column, _, _>(&mut record, self.#ident, #convert)?;));
                } else {
                    predicates.push(syn::parse_quote!(#ty: #orm::ChangeInput<#column>));
                    statements.push(quote!(<#ty as #orm::ChangeInput<#column>>::encode_change(self.#ident, &mut record)?;));
                }
            }
        }
    }
    if kind == Kind::Insert {
        generics
            .make_where_clause()
            .predicates
            .push(syn::parse_quote!(Self: #entity::CompleteInsert));
    }
    let (implementation, arguments, constraints) = generics.split_for_impl();
    let (original_impl, original_args, original_where) = input.generics.split_for_impl();
    Ok(match kind {
        Kind::Read => quote! {
            impl #implementation #orm::FromRow<#entity::Entity> for #name #arguments #constraints {
                const COLUMNS: &'static [&'static str] = &[#(#columns),*];
                fn from_row(mut row: #orm::Row) -> ::core::result::Result<Self, #orm::DbError> {
                    Ok(Self { #(#statements),* })
                }
            }
        },
        Kind::Insert => quote! {
            #(impl #original_impl #orm::HasColumn<#presence> for #name #original_args #original_where {})*
            impl #implementation #orm::Insertable<#entity::Entity> for #name #arguments #constraints {
                fn into_record(self) -> ::core::result::Result<#orm::Record, #orm::DbError> {
                    let mut record = #orm::Record::new();
                    #(#statements)*
                    Ok(record)
                }
            }
        },
        Kind::Update => quote! {
            impl #implementation #orm::Changeset<#entity::Entity> for #name #arguments #constraints {
                fn into_changes(self) -> ::core::result::Result<#orm::Patch<#entity::Entity>, #orm::DbError> {
                    let mut record = #orm::Record::new();
                    #(#statements)*
                    Ok(#orm::Patch::from_assignments(record))
                }
            }
        },
    })
}
