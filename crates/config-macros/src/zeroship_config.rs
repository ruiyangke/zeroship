use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Attribute, Expr, Fields, GenericArgument, Ident, ItemStruct, LitStr, Meta, PathArguments,
    Token, Type, Visibility,
};

struct MacroArgs {
    binary: LitStr,
    scope: Option<LitStr>,
}

impl Parse for MacroArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let entries = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        let mut binary = None;
        let mut scope = None;

        for entry in entries {
            let Meta::NameValue(name_value) = entry else {
                return Err(syn::Error::new_spanned(
                    entry,
                    "expected `binary = \"...\"` or `scope = \"...\"`",
                ));
            };
            let Some(key) = name_value.path.get_ident() else {
                return Err(syn::Error::new_spanned(
                    name_value.path,
                    "configuration macro keys must be bare identifiers",
                ));
            };
            let Expr::Lit(expr_lit) = name_value.value else {
                return Err(syn::Error::new_spanned(
                    name_value.value,
                    "configuration macro values must be string literals",
                ));
            };
            let syn::Lit::Str(value) = expr_lit.lit else {
                return Err(syn::Error::new_spanned(
                    expr_lit,
                    "configuration macro values must be string literals",
                ));
            };

            match key.to_string().as_str() {
                "binary" => set_once(&mut binary, value, "binary")?,
                "scope" => set_once(&mut scope, value, "scope")?,
                _ => {
                    return Err(syn::Error::new_spanned(
                        key,
                        "unknown zeroship_config argument; expected `binary` or `scope`",
                    ));
                }
            }
        }

        let binary = binary.ok_or_else(|| {
            syn::Error::new(proc_macro2::Span::call_site(), "missing `binary = \"...\"`")
        })?;
        let effective_scope = scope.as_ref().unwrap_or(&binary);
        validate_scope(effective_scope)?;

        Ok(Self { binary, scope })
    }
}

fn set_once(slot: &mut Option<LitStr>, value: LitStr, key: &str) -> syn::Result<()> {
    if slot.replace(value).is_some() {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            format!("duplicate `{key}` argument"),
        ));
    }
    Ok(())
}

fn validate_scope(scope: &LitStr) -> syn::Result<()> {
    if valid_segment(&scope.value()) {
        Ok(())
    } else {
        Err(syn::Error::new(
            scope.span(),
            "scope must be one ASCII lowercase snake_case segment",
        ))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SupplyClass {
    Operational,
    Secret,
}

struct FieldConfig {
    ident: Ident,
    visibility: Visibility,
    class: SupplyClass,
    inner_type: Type,
    canonical: LitStr,
    default: Option<Expr>,
    retained_attrs: Vec<Attribute>,
    cfg_attrs: Vec<Attribute>,
}

struct ConfigAttribute {
    name: LitStr,
    default: Option<Expr>,
}

impl Parse for ConfigAttribute {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let entries = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        let mut name = None;
        let mut default = None;

        for entry in entries {
            let Meta::NameValue(name_value) = entry else {
                return Err(syn::Error::new_spanned(
                    entry,
                    "expected `name = \"...\"` or `default = EXPR`",
                ));
            };
            let Some(key) = name_value.path.get_ident() else {
                return Err(syn::Error::new_spanned(
                    name_value.path,
                    "configuration field keys must be bare identifiers",
                ));
            };
            match key.to_string().as_str() {
                "name" => {
                    if name.is_some() {
                        return Err(syn::Error::new_spanned(key, "duplicate `name` argument"));
                    }
                    let Expr::Lit(expr_lit) = name_value.value else {
                        return Err(syn::Error::new_spanned(
                            name_value.value,
                            "`name` must be a string literal",
                        ));
                    };
                    let syn::Lit::Str(value) = expr_lit.lit else {
                        return Err(syn::Error::new_spanned(
                            expr_lit,
                            "`name` must be a string literal",
                        ));
                    };
                    name = Some(value);
                }
                "default" => {
                    if default.replace(name_value.value).is_some() {
                        return Err(syn::Error::new_spanned(
                            key,
                            "duplicate `default` argument",
                        ));
                    }
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        key,
                        "unknown config argument; expected `name` or `default`",
                    ));
                }
            }
        }

        Ok(Self {
            name: name.ok_or_else(|| {
                syn::Error::new(proc_macro2::Span::call_site(), "missing `name = \"...\"`")
            })?,
            default,
        })
    }
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let args = syn::parse2::<MacroArgs>(attr)?;
    let mut resolved = syn::parse2::<ItemStruct>(item)?;

    if !resolved.generics.params.is_empty() || resolved.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            &resolved.generics,
            "zeroship_config structs cannot be generic",
        ));
    }

    let Fields::Named(fields) = &mut resolved.fields else {
        return Err(syn::Error::new_spanned(
            &resolved.fields,
            "zeroship_config requires a struct with named fields",
        ));
    };
    if fields.named.is_empty() {
        return Err(syn::Error::new_spanned(
            &resolved.ident,
            "zeroship_config requires at least one configuration field",
        ));
    }

    let scope = args.scope.as_ref().unwrap_or(&args.binary).clone();
    let mut configs = Vec::with_capacity(fields.named.len());
    for field in &mut fields.named {
        let ident = field.ident.clone().expect("named field");
        let config_attr_indices: Vec<usize> = field
            .attrs
            .iter()
            .enumerate()
            .filter_map(|(index, attr)| attr.path().is_ident("config").then_some(index))
            .collect();
        if config_attr_indices.len() != 1 {
            return Err(syn::Error::new_spanned(
                &field,
                "each zeroship_config field requires exactly one #[config(...)] attribute",
            ));
        }
        let config_index = config_attr_indices[0];
        let config = field.attrs[config_index].parse_args::<ConfigAttribute>()?;
        validate_canonical(&config.name)?;
        let (class, inner_type) = wrapper_type(&field.ty)?;
        if class == SupplyClass::Secret && config.default.is_some() {
            return Err(syn::Error::new_spanned(
                &field.attrs[config_index],
                "Secret<T> fields cannot declare a compiled secret default",
            ));
        }

        field.attrs.remove(config_index);
        let retained_attrs = field.attrs.clone();
        let cfg_attrs = retained_attrs
            .iter()
            .filter(|attribute| attribute.path().is_ident("cfg"))
            .cloned()
            .collect();
        configs.push(FieldConfig {
            ident,
            visibility: field.vis.clone(),
            class,
            inner_type,
            canonical: config.name,
            default: config.default,
            retained_attrs,
            cfg_attrs,
        });
    }

    emit(resolved, args.binary, scope, &configs)
}

fn wrapper_type(ty: &Type) -> syn::Result<(SupplyClass, Type)> {
    let Type::Path(type_path) = ty else {
        return Err(syn::Error::new_spanned(
            ty,
            "configuration fields must have type Operational<T> or Secret<T>",
        ));
    };
    if type_path.qself.is_some() {
        return Err(syn::Error::new_spanned(
            ty,
            "configuration wrapper types cannot use qualified-self syntax",
        ));
    }
    let Some(segment) = type_path.path.segments.last() else {
        return Err(syn::Error::new_spanned(ty, "missing configuration wrapper"));
    };
    let class = match segment.ident.to_string().as_str() {
        "Operational" => SupplyClass::Operational,
        "Secret" => SupplyClass::Secret,
        _ => {
            return Err(syn::Error::new_spanned(
                &segment.ident,
                "configuration fields must have type Operational<T> or Secret<T>",
            ));
        }
    };
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return Err(syn::Error::new_spanned(
            &segment.arguments,
            "configuration wrapper requires exactly one type argument",
        ));
    };
    let mut types = arguments.args.iter().filter_map(|argument| match argument {
        GenericArgument::Type(ty) => Some(ty.clone()),
        _ => None,
    });
    let Some(inner) = types.next() else {
        return Err(syn::Error::new_spanned(
            arguments,
            "configuration wrapper requires exactly one type argument",
        ));
    };
    if types.next().is_some() || arguments.args.len() != 1 {
        return Err(syn::Error::new_spanned(
            arguments,
            "configuration wrapper requires exactly one type argument",
        ));
    }
    Ok((class, inner))
}

fn emit(
    resolved: ItemStruct,
    binary: LitStr,
    scope: LitStr,
    configs: &[FieldConfig],
) -> syn::Result<TokenStream> {
    let resolved_ident = &resolved.ident;
    let resolved_visibility = &resolved.vis;
    let sources_ident = format_ident!("{resolved_ident}Sources");
    let consumer_ident = format_ident!("{resolved_ident}Consumer");

    let source_fields = configs.iter().map(|config| {
        let ident = &config.ident;
        let visibility = &config.visibility;
        let attrs = &config.retained_attrs;
        let inner = &config.inner_type;
        let canonical = config.canonical.value();
        let flag = flag_projection(&canonical, &scope.value(), config.class);
        match config.class {
            // `Option` is deliberately unqualified. clap's derive only treats a
            // field as optional when the type path has exactly one segment, so
            // `::std::option::Option<T>` is parsed as a required value whose
            // parser must accept `Option<T>`, and the carrier fails to compile.
            SupplyClass::Operational => {
                let env = env_projection(&canonical);
                quote! {
                    #(#attrs)*
                    #[arg(long = #flag, env = #env)]
                    #visibility #ident: Option<#inner>
                }
            }
            SupplyClass::Secret => quote! {
                #(#attrs)*
                #[arg(long = #flag, value_name = "PATH")]
                #visibility #ident: Option<::std::path::PathBuf>
            },
        }
    });

    let specs = configs.iter().map(|config| {
        let cfg_attrs = &config.cfg_attrs;
        let canonical = &config.canonical;
        let field = config.ident.to_string();
        let inner = &config.inner_type;
        let default = config
            .default
            .as_ref()
            .map(|value| {
                quote!(::core::option::Option::Some(::core::stringify!(#value)))
            })
            .unwrap_or_else(|| quote!(::core::option::Option::None));
        // `ConfigSpec::secret` takes no default parameter at all, so a compiled
        // secret default is not merely rejected by the attribute above but
        // unspellable in the emitted call.
        let (constructor, trailing) = match config.class {
            SupplyClass::Operational => (quote!(operational), quote!(#default,)),
            SupplyClass::Secret => (quote!(secret), quote!()),
        };
        quote! {
            #(#cfg_attrs)*
            ::zeroship_core::config::ConfigSpec::#constructor(
                ::zeroship_core::config::CanonicalName::from_static(#canonical),
                &[::zeroship_core::config::Consumer::new(#binary, #scope)],
                #field,
                #field,
                ::core::stringify!(#inner),
                #trailing
            )
        }
    });

    let read_sites: Vec<_> = configs
        .iter()
        .flat_map(|config| {
            let canonical = config.canonical.clone();
            let cfg_attrs = config.cfg_attrs.clone();
            let binary = binary.clone();
            let scope = scope.clone();
            let field_name = config.ident.to_string().to_ascii_uppercase();
            let struct_name = resolved_ident.to_string().to_ascii_uppercase();
            let mut kinds = vec![
                match config.class {
                    SupplyClass::Operational => ("CLI", quote!(Cli)),
                    SupplyClass::Secret => ("CLI_FILE", quote!(CliFile)),
                },
                ("TOML", quote!(Toml)),
            ];
            if config.class == SupplyClass::Operational {
                kinds.push(("ENV", quote!(Env)));
            }
            kinds.into_iter().map(move |(kind_name, kind)| {
                let static_ident = format_ident!(
                    "__ZEROSHIP_CONFIG_READ_SITE_{struct_name}_{field_name}_{kind_name}"
                );
                quote! {
                    #(#cfg_attrs)*
                    #[::zeroship_core::__private::linkme::distributed_slice(
                        ::zeroship_core::config::CONFIG_READ_SITES
                    )]
                    #[linkme(crate = ::zeroship_core::__private::linkme)]
                    static #static_ident: ::zeroship_core::config::ReadSite =
                        ::zeroship_core::config::ReadSite::new(
                            ::zeroship_core::config::CanonicalName::from_static(#canonical),
                            ::zeroship_core::config::Consumer::new(#binary, #scope),
                            ::zeroship_core::config::SourceKind::#kind,
                            ::core::file!(),
                            ::core::line!(),
                            ::core::column!(),
                        );
                }
            })
        })
        .collect();

    let resolved_fields = configs.iter().map(|config| {
        let ident = &config.ident;
        let canonical = &config.canonical;
        let cfg_attrs = &config.cfg_attrs;
        match config.class {
            SupplyClass::Operational => {
                let default = config
                    .default
                    .as_ref()
                    .map_or_else(|| quote!(::std::option::Option::None), |default| {
                        quote!(::std::option::Option::Some(#default))
                    });
                quote! {
                    #(#cfg_attrs)*
                    #ident: ::zeroship_core::config::resolve_operational(
                        ::zeroship_core::config::CanonicalName::from_static(#canonical),
                        sources.#ident,
                        overlay,
                        || #default,
                    )?
                }
            }
            SupplyClass::Secret => quote! {
                #(#cfg_attrs)*
                #ident: ::zeroship_core::config::resolve_secret_sources(
                    ::zeroship_core::config::CanonicalName::from_static(#canonical),
                    sources.#ident,
                    ::zeroship_core::read_config_env!(
                        ::zeroship_core::config::EnvKey::<
                            ::std::string::String,
                            #consumer_ident,
                        >::from_static(
                            ::zeroship_core::config::CanonicalName::from_static(#canonical),
                        ),
                        #consumer_ident,
                    )?,
                    overlay,
                )?
            },
        }
    });

    Ok(quote! {
        #resolved

        #[derive(::zeroship_core::__private::clap::Parser)]
        #resolved_visibility struct #sources_ident {
            #(#source_fields,)*
        }

        #[derive(Clone, Copy, Debug)]
        #resolved_visibility struct #consumer_ident;

        impl ::zeroship_core::config::ConfigConsumer for #consumer_ident {
            const BINARY: &'static str = #binary;
            const SCOPE: &'static str = #scope;
        }

        impl #resolved_ident {
            #resolved_visibility const CONFIG_SPECS: &'static [::zeroship_core::config::ConfigSpec] = &[
                #(#specs,)*
            ];
        }

        #(#read_sites)*

        impl ::zeroship_core::config::GeneratedConfig for #resolved_ident {
            type Sources = #sources_ident;
            type Consumer = #consumer_ident;

            const SPECS: &'static [::zeroship_core::config::ConfigSpec] = Self::CONFIG_SPECS;

            fn resolve_config(
                sources: Self::Sources,
                overlay: ::std::option::Option<&::zeroship_core::__private::toml::Value>,
            ) -> ::std::result::Result<Self, ::zeroship_core::config::ConfigResolveError> {
                ::std::result::Result::Ok(Self {
                    #(#resolved_fields,)*
                })
            }
        }
    })
}

fn validate_canonical(name: &LitStr) -> syn::Result<()> {
    let value = name.value();
    if !value.is_empty() && value.split('.').all(valid_segment) {
        Ok(())
    } else {
        Err(syn::Error::new(
            name.span(),
            "canonical config name must be ASCII lowercase dotted snake_case",
        ))
    }
}

fn valid_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_lowercase) {
        return false;
    }

    let mut previous_underscore = false;
    for byte in bytes {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            previous_underscore = false;
        } else if *byte == b'_' && !previous_underscore {
            previous_underscore = true;
        } else {
            return false;
        }
    }
    !previous_underscore
}

fn local_projection<'a>(canonical: &'a str, scope: &str) -> &'a str {
    canonical
        .strip_prefix(scope)
        .and_then(|rest| rest.strip_prefix('.'))
        .unwrap_or(canonical)
}

fn flag_projection(canonical: &str, scope: &str, class: SupplyClass) -> String {
    let mut flag = local_projection(canonical, scope).replace(['.', '_'], "-");
    if class == SupplyClass::Secret {
        flag.push_str("-file");
    }
    flag
}

fn env_projection(canonical: &str) -> String {
    format!("ZEROSHIP_{}", canonical.replace('.', "_").to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::{expand, validate_canonical};

    fn formatted(tokens: proc_macro2::TokenStream) -> String {
        let file = syn::parse2::<syn::File>(tokens).expect("expansion parses as a Rust file");
        prettyplease::unparse(&file)
    }

    fn compact(text: &str) -> String {
        text.chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    fn sample_expansion() -> String {
        let expanded = expand(
            quote!(binary = "control"),
            quote! {
                #[derive(Debug)]
                pub struct ControlConfig {
                    #[config(name = "control.port", default = 9090)]
                    pub port: Operational<u16>,
                    #[config(name = "control.database_url")]
                    pub database_url: Secret<DatabaseUrl>,
                }
            },
        )
        .expect("sample config expands");
        formatted(expanded)
    }

    #[test]
    fn expansion_emits_specs_and_real_read_site_registrations() {
        let output = sample_expansion();
        let compact = compact(&output);
        assert!(output.contains("ConfigSpec::operational"));
        assert!(output.contains("ConfigSpec::secret"));
        assert!(
            compact.contains("distributed_slice(::zeroship_core::config::CONFIG_READ_SITES)")
        );
        assert!(output.contains("SourceKind::Cli"));
        assert!(output.contains("SourceKind::CliFile"));
        assert!(output.contains("SourceKind::Toml"));
        assert!(output.contains("SourceKind::Env"));
        assert!(output.contains("file!()"));
        assert!(output.contains("__ZEROSHIP_CONFIG_READ_SITE_CONTROLCONFIG_PORT_ENV"));

        // This proves the macro emits registrations, not that the linker retains
        // and enumerates them. The downstream contract crate owns that proof.
    }

    #[test]
    fn expansion_emits_resolver_and_preserves_secret_wrapper() {
        let output = sample_expansion();
        let compact = compact(&output);
        assert!(output.contains("impl ::zeroship_core::config::GeneratedConfig for ControlConfig"));
        assert!(output.contains("resolve_operational"));
        assert!(output.contains("resolve_secret_sources"));
        assert!(output.contains("read_config_env!"));
        assert!(compact.contains("EnvKey::<"));
        assert!(compact.contains("::std::string::String,ControlConfigConsumer"));
        assert!(compact.contains(">::from_static"));
        assert!(output.contains("pub database_url: Secret<DatabaseUrl>"));
        assert!(!output.contains("read_config_env::<ControlConfigConsumer>"));

        // This does not prove Secret<T>'s Debug implementation redacts; core and
        // downstream compiled fixtures cover the wrapper's behavior.
    }

    #[test]
    fn secret_source_has_only_a_path_flag() {
        let output = sample_expansion();
        assert!(output.contains("long = \"database-url-file\""));
        assert!(output.contains("Option<::std::path::PathBuf>"));
        assert!(!output.contains("env = \"ZEROSHIP_CONTROL_DATABASE_URL\""));
        assert!(output.contains("env = \"ZEROSHIP_CONTROL_PORT\""));

        // This is an expansion assertion. The contract crate must also inspect
        // clap's compiled Command metadata so derive composition cannot hide a
        // secret value environment source on the path carrier.
    }

    #[test]
    fn scope_controls_only_the_local_flag_projection() {
        let expanded = expand(
            quote!(binary = "zeroship-control", scope = "control"),
            quote! {
                struct Scoped {
                    #[config(name = "control.port")]
                    port: Operational<u16>,
                    #[config(name = "auth.public_url")]
                    auth_url: Operational<String>,
                }
            },
        )
        .expect("scoped config expands");
        let output = formatted(expanded);
        assert!(output.contains("long = \"port\""));
        assert!(output.contains("long = \"auth-public-url\""));
        assert!(output.contains("env = \"ZEROSHIP_CONTROL_PORT\""));

        // This does not inspect global registry collisions; the contract crate
        // compares projections across every linked declaration.
    }

    #[test]
    fn canonical_validation_rejects_each_bad_grammar_shape() {
        let mut bad_names = vec![
            "",
            ".control",
            "control.",
            "control..port",
            "Control.port",
            "control._port",
            "control.port_",
            "control.max__apps",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let non_ascii = char::from_u32(0xf6).expect("valid scalar");
        bad_names.push(format!("control.p{non_ascii}rt"));

        for bad in bad_names {
            let name = syn::LitStr::new(&bad, proc_macro2::Span::call_site());
            assert!(validate_canonical(&name).is_err(), "accepted {bad:?}");
        }

        // This checks identity syntax only. Projection collisions are a global
        // registry concern and deliberately cannot be decided by one expansion.
    }
}
