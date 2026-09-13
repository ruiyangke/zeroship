use proc_macro2::TokenStream;
use quote::quote;
use syn::{
    braced, bracketed, parenthesized,
    parse::{Parse, ParseStream},
    Lit, Token,
};

#[derive(Clone, Debug)]
pub enum Literal {
    Null,
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
    Decimal(String),
    Timestamp(i64),
}

impl Parse for Literal {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.peek(syn::token::Bracket) {
            let content;
            bracketed!(content in input);
            return Ok(Self::Array(
                content
                    .parse_terminated(Self::parse, Token![,])?
                    .into_iter()
                    .collect(),
            ));
        }
        if input.peek(syn::token::Brace) {
            let content;
            braced!(content in input);
            let mut fields = Vec::new();
            let mut names = std::collections::HashSet::new();
            while !content.is_empty() {
                let name = if content.peek(syn::LitStr) {
                    content.parse::<syn::LitStr>()?.value()
                } else {
                    content
                        .parse::<syn::Ident>()?
                        .to_string()
                        .trim_start_matches("r#")
                        .to_owned()
                };
                if !names.insert(name.clone()) {
                    return Err(content.error("duplicate object key"));
                }
                content.parse::<Token![:]>()?;
                fields.push((name, content.parse()?));
                if content.is_empty() {
                    break;
                }
                content.parse::<Token![,]>()?;
            }
            return Ok(Self::Object(fields));
        }
        if input.peek(syn::Ident) {
            let name: syn::Ident = input.parse()?;
            match name.to_string().as_str() {
                "null" => return Ok(Self::Null),
                "decimal" => {
                    let content;
                    parenthesized!(content in input);
                    let value: syn::LitStr = content.parse()?;
                    if !content.is_empty() {
                        return Err(content.error("expected a decimal string"));
                    }
                    return Ok(Self::Decimal(value.value()));
                }
                "timestamp" => {
                    let content;
                    parenthesized!(content in input);
                    let value = content.parse::<Self>()?;
                    if !content.is_empty() {
                        return Err(content.error("expected timestamp milliseconds"));
                    }
                    return match value {
                        Self::Signed(value) => Ok(Self::Timestamp(value)),
                        _ => Err(syn::Error::new(
                            name.span(),
                            "timestamp requires signed integer milliseconds",
                        )),
                    };
                }
                _ => {
                    return Err(syn::Error::new(
                        name.span(),
                        "expected a native literal value",
                    ))
                }
            }
        }
        let negative = input.peek(Token![-]);
        if negative {
            input.parse::<Token![-]>()?;
        }
        let literal: Lit = input.parse()?;
        match literal {
            Lit::Int(value) => {
                let magnitude = value.base10_parse::<u64>()?;
                if negative {
                    let signed = i64::try_from(-i128::from(magnitude)).map_err(|_| {
                        syn::Error::new(value.span(), "integer literal is outside the signed range")
                    })?;
                    Ok(Self::Signed(signed))
                } else if let Ok(signed) = i64::try_from(magnitude) {
                    Ok(Self::Signed(signed))
                } else {
                    Ok(Self::Unsigned(magnitude))
                }
            }
            Lit::Float(value) => {
                let number = value.base10_parse::<f64>()? * if negative { -1.0 } else { 1.0 };
                if !number.is_finite() {
                    return Err(syn::Error::new(
                        value.span(),
                        "number literal must be finite",
                    ));
                }
                Ok(Self::Float(number))
            }
            Lit::Str(value) if !negative => Ok(Self::Text(value.value())),
            Lit::Bool(value) if !negative => Ok(Self::Bool(value.value)),
            Lit::ByteStr(value) if !negative => Ok(Self::Bytes(value.value())),
            _ => Err(input.error("expected a native literal value")),
        }
    }
}

impl Literal {
    pub fn tokens(&self, orm: &syn::Path) -> TokenStream {
        match self {
            Self::Null => quote!(#orm::Value::Null),
            Self::Bool(value) => quote!(#orm::Value::Bool(#value)),
            Self::Signed(value) => quote!(#orm::Value::from(#value)),
            Self::Unsigned(value) => quote!(#orm::Value::from(#value)),
            Self::Float(value) => quote!(#orm::Value::from(#value)),
            Self::Text(value) => quote!(#orm::Value::String(::std::string::String::from(#value))),
            Self::Bytes(values) => quote!(#orm::Value::Bytes(::std::vec![#(#values),*])),
            Self::Array(values) => {
                let values = values.iter().map(|v| v.tokens(orm));
                quote!(#orm::Value::Array(::std::vec![#(#values),*]))
            }
            Self::Object(fields) => {
                let fields = fields.iter().map(|(name, value)| {
                    let value = value.tokens(orm);
                    quote!((::std::string::String::from(#name), #value))
                });
                quote!(#orm::Value::Object([#(#fields),*].into_iter().collect()))
            }
            Self::Decimal(value) => {
                quote!(#orm::Value::Decimal(::std::string::String::from(#value)))
            }
            Self::Timestamp(value) => quote!(#orm::Value::Timestamp(#value)),
        }
    }
}
