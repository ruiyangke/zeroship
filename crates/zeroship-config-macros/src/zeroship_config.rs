use proc_macro2::TokenStream;
use quote::{format_ident, quote, ToTokens};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Attribute, Expr, Fields, GenericArgument, Ident, ItemStruct, LitStr, Meta, PathArguments,
    Token, Type, Visibility,
};

use crate::shared;

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
    Bootstrap,
    Command,
}

impl SupplyClass {
    /// A command control has no environment tier and a secret never puts one on
    /// its clap carrier; the remaining classes may, subject to `env = false`.
    const fn has_clap_env(self) -> bool {
        matches!(self, Self::Operational | Self::Bootstrap)
    }
}

/// How the clap carrier for a control field is shaped.
///
/// `--no-config` must stay a bare flag and `--config` must stay optional, so a
/// control cannot use the operational `Option<T>` + required-default carrier
/// unconditionally.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CarrierShape {
    /// `bool` carrier: a bare presence flag that also accepts `=VALUE`.
    Flag,
    /// `Option<U>` carrier resolving to `Option<U>`: absence is a real value.
    Optional,
    /// `Option<T>` carrier resolving to `T` via the carrier or a default.
    Valued,
}

struct FieldConfig {
    ident: Ident,
    visibility: Visibility,
    class: SupplyClass,
    shape: CarrierShape,
    inner_type: Type,
    /// For [`CarrierShape::Optional`], the `U` inside the declared `Option<U>`.
    carrier_type: Type,
    canonical: LitStr,
    env_enabled: bool,
    default: Option<Expr>,
    retained_attrs: Vec<Attribute>,
    cfg_attrs: Vec<Attribute>,
}

struct ConfigAttribute {
    /// The canonical name, either written directly or read from the shared table.
    name: LitStr,
    /// Set when the identity came from `shared = SYMBOL`; carries the required
    /// wrapper and inner type so the field cannot restate them differently.
    shared: Option<&'static shared::SharedIdentity>,
    default: Option<Expr>,
    /// `env = false` on a bootstrap control; see [`SupplyClass::Bootstrap`].
    env: Option<syn::LitBool>,
}

impl Parse for ConfigAttribute {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let entries = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        let mut name: Option<LitStr> = None;
        let mut shared: Option<(&'static shared::SharedIdentity, Ident)> = None;
        let mut default = None;
        let mut env = None;

        for entry in entries {
            let Meta::NameValue(name_value) = entry else {
                return Err(syn::Error::new_spanned(
                    entry,
                    "expected `name = \"...\"`, `shared = SYMBOL`, `default = EXPR` \
                     or `env = false`",
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
                    if let Some(identity) = shared::by_canonical(&value.value()) {
                        return Err(syn::Error::new_spanned(
                            &value,
                            format!(
                                "`{}` is a shared identity; write \
                                 `#[config(shared = {})]` so its name, class and \
                                 type come from one place",
                                identity.canonical, identity.symbol
                            ),
                        ));
                    }
                    name = Some(value);
                }
                "shared" => {
                    let Expr::Path(path) = &name_value.value else {
                        return Err(syn::Error::new_spanned(
                            &name_value.value,
                            "`shared` must be a bare shared-identity symbol",
                        ));
                    };
                    let Some(symbol) = path.path.get_ident() else {
                        return Err(syn::Error::new_spanned(
                            &name_value.value,
                            "`shared` must be a bare shared-identity symbol",
                        ));
                    };
                    let Some(identity) = shared::by_symbol(&symbol.to_string()) else {
                        return Err(syn::Error::new_spanned(
                            symbol,
                            format!(
                                "unknown shared identity `{symbol}`; known symbols are {}",
                                shared::known_symbols()
                            ),
                        ));
                    };
                    if shared.replace((identity, symbol.clone())).is_some() {
                        return Err(syn::Error::new_spanned(key, "duplicate `shared` argument"));
                    }
                }
                "default" => {
                    if default.replace(name_value.value).is_some() {
                        return Err(syn::Error::new_spanned(
                            key,
                            "duplicate `default` argument",
                        ));
                    }
                }
                "env" => {
                    let Expr::Lit(expr_lit) = name_value.value else {
                        return Err(syn::Error::new_spanned(
                            key,
                            "`env` must be the literal `false`",
                        ));
                    };
                    let syn::Lit::Bool(value) = expr_lit.lit else {
                        return Err(syn::Error::new_spanned(
                            expr_lit,
                            "`env` must be the literal `false`",
                        ));
                    };
                    if value.value() {
                        return Err(syn::Error::new_spanned(
                            value,
                            "`env = true` is the default; only `env = false` is meaningful",
                        ));
                    }
                    if env.replace(value).is_some() {
                        return Err(syn::Error::new_spanned(key, "duplicate `env` argument"));
                    }
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        key,
                        "unknown config argument; expected `name`, `shared`, `default` \
                         or `env`",
                    ));
                }
            }
        }

        match (name, shared) {
            (Some(_), Some((_, symbol))) => Err(syn::Error::new_spanned(
                symbol,
                "a field declares `name` or `shared`, never both; a shared \
                 identity's canonical name lives in the shared table",
            )),
            (Some(name), None) => Ok(Self {
                name,
                shared: None,
                default,
                env,
            }),
            (None, Some((identity, symbol))) => Ok(Self {
                // The span is the symbol, so a canonical-name diagnostic points
                // at what the author actually wrote.
                name: LitStr::new(identity.canonical, symbol.span()),
                shared: Some(identity),
                default,
                env,
            }),
            (None, None) => Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "missing `name = \"...\"` or `shared = SYMBOL`",
            )),
        }
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
        if let Some(identity) = config.shared {
            // The shared table owns the class and the resolved type. Restating
            // either one differently is what gives a single ZEROSHIP_* name two
            // parse behaviours, so it is refused rather than coalesced later.
            let declared = field.ty.to_token_stream().to_string();
            let required = format!("{}<{}>", identity.wrapper, identity.inner);
            if !shared::type_matches(&declared, &required) {
                return Err(syn::Error::new_spanned(
                    &field.ty,
                    format!(
                        "shared identity `{}` is declared as `{required}`; every \
                         consumer must spell it identically",
                        identity.canonical
                    ),
                ));
            }
        }
        if class == SupplyClass::Secret && config.default.is_some() {
            return Err(syn::Error::new_spanned(
                &field.attrs[config_index],
                "Secret<T> fields cannot declare a compiled secret default",
            ));
        }
        if let Some(env) = &config.env
            && class != SupplyClass::Bootstrap
        {
            return Err(syn::Error::new_spanned(
                env,
                "only BootstrapControl<T> may disable its environment source; an \
                 operational or secret setting needs one, and a command control \
                 never has one",
            ));
        }
        let (shape, carrier_type) = carrier_shape(class, &inner_type);
        if shape != CarrierShape::Valued && config.default.is_some() {
            return Err(syn::Error::new_spanned(
                &field.attrs[config_index],
                "a bool control defaults to false and an Option<T> control defaults to None; \
                 remove the `default`",
            ));
        }

        field.attrs.remove(config_index);
        // Every remaining attribute travels to the clap CARRIER. `#[arg(..)]`
        // and `#[command(..)]` are then dropped from the resolved struct: it is
        // not a clap type, and leaving them there is an unknown-attribute error
        // rather than a no-op.
        let retained_attrs = field.attrs.clone();
        field
            .attrs
            .retain(|attr| !(attr.path().is_ident("arg") || attr.path().is_ident("command")));
        let cfg_attrs = retained_attrs
            .iter()
            .filter(|attribute| attribute.path().is_ident("cfg"))
            .cloned()
            .collect();
        configs.push(FieldConfig {
            ident,
            visibility: field.vis.clone(),
            class,
            shape,
            inner_type,
            carrier_type,
            canonical: config.name,
            env_enabled: config.env.is_none(),
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
            "configuration fields must have type Operational<T>, Secret<T>, \
             BootstrapControl<T> or CommandControl<T>",
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
        "BootstrapControl" => SupplyClass::Bootstrap,
        "CommandControl" => SupplyClass::Command,
        _ => {
            return Err(syn::Error::new_spanned(
                &segment.ident,
                "configuration fields must have type Operational<T>, Secret<T>, \
                 BootstrapControl<T> or CommandControl<T>",
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
        let canonical = config.canonical.value();
        let flag = flag_projection(&canonical, &scope.value(), config.class);
        if config.class == SupplyClass::Secret {
            return quote! {
                #(#attrs)*
                #[arg(long = #flag, value_name = "PATH")]
                #visibility #ident: Option<::std::path::PathBuf>
            };
        }
        // The environment name is attached to the clap carrier for every class
        // that has one, so `clap` itself performs the CLI-over-env merge and no
        // generated code re-implements that precedence.
        let env = (config.class.has_clap_env() && config.env_enabled)
            .then(|| env_projection(&canonical))
            .map(|env| quote!(, env = #env))
            .unwrap_or_default();
        // `Option` is deliberately unqualified. clap's derive only treats a
        // field as optional when the type path has exactly one segment, so
        // `::std::option::Option<T>` is parsed as a required value whose
        // parser must accept `Option<T>`, and the carrier fails to compile.
        match config.shape {
            // NOT `ArgAction::SetTrue`. That action parses an environment value
            // with clap's strict bool parser, so `ZEROSHIP_NO_CONFIG=1` is a
            // startup ERROR and only `true`/`false` are accepted - the one
            // spelling an operator is least likely to reach for. This is the
            // shape `--trust-proxy` already uses: bare presence means true,
            // `=false` still works, and the environment goes through the
            // workspace's single boolean grammar (1/0/true/false/yes/no).
            CarrierShape::Flag => quote! {
                #(#attrs)*
                #[arg(
                    long = #flag #env,
                    num_args = 0..=1,
                    default_value_t = false,
                    default_missing_value = "true",
                    value_parser = ::zeroship_core::config::parse_bool_flag
                )]
                #visibility #ident: bool
            },
            CarrierShape::Optional => {
                let carrier = &config.carrier_type;
                quote! {
                    #(#attrs)*
                    #[arg(long = #flag #env)]
                    #visibility #ident: Option<#carrier>
                }
            }
            CarrierShape::Valued => {
                let inner = &config.inner_type;
                quote! {
                    #(#attrs)*
                    #[arg(long = #flag #env)]
                    #visibility #ident: Option<#inner>
                }
            }
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
        let env_enabled = config.env_enabled;
        let (constructor, leading, trailing) = match config.class {
            SupplyClass::Operational => (quote!(operational), quote!(), quote!(#default,)),
            SupplyClass::Secret => (quote!(secret), quote!(), quote!()),
            SupplyClass::Bootstrap => {
                (quote!(bootstrap), quote!(#env_enabled,), quote!(#default,))
            }
            SupplyClass::Command => (quote!(command), quote!(), quote!(#default,)),
        };
        quote! {
            #(#cfg_attrs)*
            ::zeroship_core::config::ConfigSpec::#constructor(
                ::zeroship_core::config::CanonicalName::from_static(#canonical),
                &[::zeroship_core::config::Consumer::new(#binary, #scope)],
                #leading
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
            // Exactly the sources ConfigSpec::sources() derives for this class,
            // MINUS the ones the resolver registers itself: a secret's Env site
            // comes from the read_config_env! expansion in the resolver, not
            // from here.
            let mut kinds = vec![match config.class {
                SupplyClass::Secret => ("CLI_FILE", quote!(CliFile)),
                SupplyClass::Operational | SupplyClass::Bootstrap | SupplyClass::Command => {
                    ("CLI", quote!(Cli))
                }
            }];
            match config.class {
                SupplyClass::Operational => {
                    kinds.push(("TOML", quote!(Toml)));
                    kinds.push(("ENV", quote!(Env)));
                }
                SupplyClass::Secret => kinds.push(("TOML", quote!(Toml))),
                SupplyClass::Bootstrap if config.env_enabled => {
                    kinds.push(("ENV", quote!(Env)));
                }
                SupplyClass::Bootstrap | SupplyClass::Command => {}
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

    // A secret must never be dereferenced by a dry run, so the resolver needs to
    // know which run it is in. It reads that from the CHECK_CONFIG carrier on
    // this same declaration - already merged by clap - rather than from a
    // parameter a caller could forget to thread. That is why a declaration
    // carrying a secret is REQUIRED to carry the command control too: a secret
    // sitting in a struct with no check-config field would resolve for real
    // during `--check-config`, and nothing at the call site would show it.
    let secret_resolution = if configs.iter().any(|config| config.class == SupplyClass::Secret) {
        let Some(check_config) = configs
            .iter()
            .find(|config| config.canonical.value() == "check_config")
            .map(|config| &config.ident)
        else {
            return Err(syn::Error::new_spanned(
                &resolved.ident,
                "a declaration with a Secret<T> field must also declare \
                 `#[config(shared = CHECK_CONFIG)]`; without it the generated \
                 resolver cannot tell a dry run from a real boot and would read \
                 secret files during --check-config",
            ));
        };
        quote! {
            let __zeroship_secret_resolution = if sources.#check_config {
                ::zeroship_core::config::SecretResolution::CheckConfig
            } else {
                ::zeroship_core::config::SecretResolution::Boot
            };
        }
    } else {
        quote!()
    };

    let resolved_fields = configs.iter().map(|config| {
        let ident = &config.ident;
        let canonical = &config.canonical;
        let cfg_attrs = &config.cfg_attrs;
        let default = config
            .default
            .as_ref()
            .map_or_else(|| quote!(::std::option::Option::None), |default| {
                quote!(::std::option::Option::Some(#default))
            });
        match config.class {
            SupplyClass::Operational => {
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
            SupplyClass::Bootstrap | SupplyClass::Command => {
                let wrapper = match config.class {
                    SupplyClass::Command => quote!(CommandControl),
                    _ => quote!(BootstrapControl),
                };
                // `overlay` is deliberately not mentioned in any arm below.
                let value = match config.shape {
                    CarrierShape::Flag | CarrierShape::Optional => quote!(sources.#ident),
                    CarrierShape::Valued => quote! {
                        ::zeroship_core::config::resolve_control(
                            ::zeroship_core::config::CanonicalName::from_static(#canonical),
                            sources.#ident,
                            || #default,
                        )?
                    },
                };
                quote! {
                    #(#cfg_attrs)*
                    #ident: ::zeroship_core::config::#wrapper::new(#value)
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
                    __zeroship_secret_resolution,
                )?
            },
        }
    });

    Ok(quote! {
        #resolved

        // `Debug` is safe on the CARRIER: an operational carrier holds a value
        // that is operational by classification, and a secret carrier holds a
        // file PATH, never the material in it. The resolved struct is where
        // `Secret<T>`'s redaction applies.
        #[derive(::core::clone::Clone, ::core::fmt::Debug, ::zeroship_core::__private::clap::Parser)]
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
                #secret_resolution
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

/// Decide the clap carrier for a control field from its declared inner type.
///
/// Only bootstrap and command controls get the flag/optional shapes. An
/// operational or secret field always uses the valued carrier, so this cannot
/// quietly turn `Operational<bool>` into a presence flag with no env tier.
fn carrier_shape(class: SupplyClass, inner: &Type) -> (CarrierShape, Type) {
    if !matches!(class, SupplyClass::Bootstrap | SupplyClass::Command) {
        return (CarrierShape::Valued, inner.clone());
    }
    if is_bool(inner) {
        return (CarrierShape::Flag, inner.clone());
    }
    option_argument(inner).map_or_else(
        || (CarrierShape::Valued, inner.clone()),
        |argument| (CarrierShape::Optional, argument),
    )
}

fn is_bool(ty: &Type) -> bool {
    matches!(ty, Type::Path(path)
        if path.qself.is_none() && path.path.is_ident("bool"))
}

/// Return `U` when `ty` is written exactly as `Option<U>`.
fn option_argument(ty: &Type) -> Option<Type> {
    let Type::Path(path) = ty else {
        return None;
    };
    if path.qself.is_some() {
        return None;
    }
    let segment = path.path.segments.last()?;
    if segment.ident != "Option" {
        return None;
    }
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    if arguments.args.len() != 1 {
        return None;
    }
    match arguments.args.first()? {
        GenericArgument::Type(inner) => Some(inner.clone()),
        _ => None,
    }
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
                    #[config(shared = CHECK_CONFIG)]
                    pub check_config: CommandControl<bool>,
                }
            },
        )
        .expect("sample config expands");
        formatted(expanded)
    }

    // A secret whose declaration has no check-config control would be resolved
    // for real by `--check-config`: the resolver would have no way to know it
    // was a dry run, and the file read would happen anyway.
    #[test]
    fn a_secret_cannot_be_declared_without_the_check_config_control() {
        let error = expand(
            quote!(binary = "control"),
            quote! {
                struct Orphan {
                    #[config(name = "control.database_url")]
                    database_url: Secret<String>,
                }
            },
        )
        .expect_err("a secret with no check-config control must not expand");
        assert!(
            error.to_string().contains("must also declare"),
            "unexpected error: {error}"
        );

        // The positive control is `sample_expansion`, which differs only by
        // carrying the CHECK_CONFIG field and does expand.
    }

    // The dry-run wiring, in the expansion: the mode is taken from the
    // check-config CARRIER (already clap-merged) and handed to every secret.
    #[test]
    fn the_secret_resolver_is_told_which_run_it_is_in() {
        let output = sample_expansion();
        let compact = compact(&output);
        assert!(compact.contains("ifsources.check_config"));
        assert!(output.contains("SecretResolution::CheckConfig"));
        assert!(output.contains("SecretResolution::Boot"));
        assert!(compact.contains("__zeroship_secret_resolution,"));

        // This reads tokens. That a CheckConfig run then opens no file is a
        // property of `resolve_secret_sources` and is asserted in core.
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

    fn controls_expansion() -> String {
        let expanded = expand(
            quote!(binary = "zeroship-control", scope = "control"),
            quote! {
                pub struct Controls {
                    #[config(shared = CONFIG)]
                    pub config: BootstrapControl<Option<PathBuf>>,
                    #[config(shared = NO_CONFIG)]
                    pub no_config: BootstrapControl<bool>,
                    #[config(shared = CHECK_CONFIG)]
                    pub check_config: CommandControl<bool>,
                    #[config(shared = CHECK_CONFIG_FORMAT, default = CheckFormat::Text)]
                    pub check_config_format: CommandControl<CheckFormat>,
                }
            },
        )
        .expect("controls expand");
        formatted(expanded)
    }

    #[test]
    fn a_command_control_gets_a_flag_and_nothing_else() {
        let output = controls_expansion();
        assert!(output.contains("long = \"check-config\""));
        assert!(output.contains("ConfigSpec::command"));
        // The env projection of `check_config` would be ZEROSHIP_CHECK_CONFIG.
        // Its absence from the whole expansion is the claim: no clap env, and
        // no read_config_env! fallback either.
        assert!(
            !output.contains("ZEROSHIP_CHECK_CONFIG"),
            "a command control must reach no environment tier:\n{output}"
        );
        assert!(!output.contains("SourceKind::Toml"));
        assert!(!output.contains("overlay,"), "no arm may consult the overlay");

        // This reads the expansion. That clap actually builds an env-free
        // argument is asserted against the compiled Command in the contract
        // crate; token text alone cannot rule out a derive default.
    }

    #[test]
    fn a_bootstrap_control_gets_a_flag_and_an_env_but_no_overlay() {
        let output = controls_expansion();
        assert!(output.contains("long = \"no-config\""));
        assert!(output.contains("env = \"ZEROSHIP_NO_CONFIG\""));
        assert!(output.contains("env = \"ZEROSHIP_CONFIG\""));
        assert!(output.contains("ConfigSpec::bootstrap"));
        assert!(output.contains("__ZEROSHIP_CONFIG_READ_SITE_CONTROLS_NO_CONFIG_ENV"));
        assert!(!output.contains("__ZEROSHIP_CONFIG_READ_SITE_CONTROLS_NO_CONFIG_TOML"));

        // Does not cover precedence between the flag and the environment; clap
        // owns that merge and no code here re-implements it.
    }

    #[test]
    fn control_carriers_keep_bare_flags_bare_and_optionals_optional() {
        let output = controls_expansion();
        let compact = compact(&output);
        assert!(compact.contains("no_config:bool"), "{output}");
        assert!(compact.contains("check_config:bool"));
        assert!(output.contains("default_missing_value = \"true\""));
        assert!(output.contains("parse_bool_flag"));
        assert!(
            compact.contains("config:Option<PathBuf>"),
            "an Option<T> control keeps exactly one Option layer:\n{output}"
        );
        assert!(
            compact.contains("check_config_format:Option<CheckFormat>"),
            "{output}"
        );
        assert!(compact.contains("resolve_control("));
        assert!(compact.contains("CommandControl::new"));
        assert!(compact.contains("BootstrapControl::new"));
    }

    #[test]
    fn a_safety_control_can_refuse_the_environment_entirely() {
        // The transform table's "env when enabled". A protection that a stray
        // exported variable can switch off is not a protection.
        let output = formatted(
            expand(
                quote!(binary = "zeroship-worker", scope = "worker"),
                quote! {
                    struct Controls {
                        #[config(name = "worker.workflow_advance_unsigned", env = false)]
                        workflow_advance_unsigned: BootstrapControl<bool>,
                        #[config(shared = NO_CONFIG)]
                        no_config: BootstrapControl<bool>,
                    }
                },
            )
            .expect("env-free safety control expands"),
        );
        assert!(
            !output.contains("ZEROSHIP_WORKER_WORKFLOW_ADVANCE_UNSIGNED"),
            "the disabled environment name must appear nowhere:\n{output}"
        );
        assert!(
            !output.contains("__ZEROSHIP_CONFIG_READ_SITE_CONTROLS_WORKFLOW_ADVANCE_UNSIGNED_ENV"),
            "a disabled source must not register a read site"
        );
        // The one-variable control: the sibling field differs only by not
        // saying `env = false`, and it keeps both.
        assert!(output.contains("env = \"ZEROSHIP_NO_CONFIG\""));
        assert!(output.contains("__ZEROSHIP_CONFIG_READ_SITE_CONTROLS_NO_CONFIG_ENV"));
        assert!(output.contains("ConfigSpec::bootstrap"));
    }

    #[test]
    fn only_a_bootstrap_control_may_disable_its_environment() {
        for field in [
            quote! {
                #[config(name = "control.port", env = false, default = 1)]
                port: Operational<u16>
            },
            quote! {
                #[config(name = "control.database_url", env = false)]
                database_url: Secret<String>
            },
            quote! {
                #[config(shared = CHECK_CONFIG, env = false)]
                check_config: CommandControl<bool>
            },
        ] {
            let error = expand(
                quote!(binary = "zeroship-control", scope = "control"),
                quote! { struct Bad { #field, } },
            )
            .expect_err("env = false outside a bootstrap control must not expand");
            assert!(
                error.to_string().contains("only BootstrapControl<T>"),
                "unexpected error: {error}"
            );
        }
    }

    #[test]
    fn a_default_on_a_flag_or_optional_control_is_rejected() {
        for field in [
            quote! {
                #[config(shared = NO_CONFIG, default = true)]
                no_config: BootstrapControl<bool>
            },
            quote! {
                #[config(shared = CONFIG, default = None)]
                config: BootstrapControl<Option<PathBuf>>
            },
        ] {
            let error = expand(
                quote!(binary = "zeroship-control", scope = "control"),
                quote! { struct Controls { #field, } },
            )
            .expect_err("a defaulted flag/optional control must not expand");
            assert!(error.to_string().contains("remove the `default`"));
        }

        // The paired positive control is `controls_expansion`, which differs
        // only by having no `default` on those two fields and does expand.
    }

    #[test]
    fn operational_bool_is_not_silently_turned_into_a_presence_flag() {
        let output = formatted(
            expand(
                quote!(binary = "zeroship-worker", scope = "worker"),
                quote! {
                    struct Ops {
                        #[config(name = "worker.trust_proxy", default = false)]
                        trust_proxy: Operational<bool>,
                    }
                },
            )
            .expect("operational bool expands"),
        );
        assert!(
            !output.contains("default_missing_value"),
            "only a control may become a bare flag; an operational value keeps \
             its env and TOML tiers:\n{output}"
        );
        assert!(output.contains("SourceKind::Toml"));
    }

    #[test]
    fn consumers_of_one_shared_identity_may_disagree_about_the_default() {
        // THE 2026-08-12 AMENDMENT, as an executable claim. Section 4.1 first
        // required every consumer of a shared identity to agree on its default,
        // which would have rejected the five declarations Step 2 landed:
        // `observability.log_filter` defaults to a directive naming the
        // declaring crate, so no single value is correct for every binary and
        // the rule had no satisfiable form.
        //
        // What must still be identical is what the OPERATOR sees: one canonical
        // name, one environment spelling, one flag. That is asserted here on
        // the same pair of expansions that differ in default.
        let mut projections = Vec::new();
        for (binary, scope, default) in [
            ("zeroship-control", "control", "info,zeroship_control=debug"),
            ("zeroship-gate", "gateway", "info,zeroship_gateway=debug"),
        ] {
            let binary = syn::LitStr::new(binary, proc_macro2::Span::call_site());
            let scope = syn::LitStr::new(scope, proc_macro2::Span::call_site());
            let default = syn::LitStr::new(default, proc_macro2::Span::call_site());
            let output = formatted(
                expand(
                    quote!(binary = #binary, scope = #scope),
                    quote! {
                        struct Controls {
                            #[config(shared = OBSERVABILITY_LOG_FILTER, default = #default.to_owned())]
                            log_filter: Operational<String>,
                        }
                    },
                )
                .expect("a per-binary default on a shared identity must expand"),
            );
            assert!(
                output.contains(&default.value()),
                "the declared default did not reach the expansion:\n{output}"
            );
            projections.push((
                output.contains("env = \"ZEROSHIP_OBSERVABILITY_LOG_FILTER\""),
                output.contains("long = \"observability-log-filter\""),
                output.contains("\"observability.log_filter\""),
            ));
        }
        assert_eq!(projections[0], (true, true, true), "control projections");
        assert_eq!(
            projections[0], projections[1],
            "two consumers of one shared identity projected different names"
        );

        // Does not cover: the binaries' resolved run-time values. Each owning
        // service crate drives its real binary in a config integration target.
    }

    #[test]
    fn a_shared_identity_cannot_be_typoed_restated_or_retyped() {
        // The three ways Step 2's repeated string literals could go wrong, each
        // paired with the positive control above, which differs only by being
        // correct and does expand.
        let cases = [
            (
                quote! {
                    #[config(shared = OBSERVABILITY_LOG_FILTR, default = String::new())]
                    log_filter: Operational<String>
                },
                "unknown shared identity",
            ),
            (
                quote! {
                    #[config(name = "observability.log_filter", default = String::new())]
                    log_filter: Operational<String>
                },
                "is a shared identity; write",
            ),
            (
                quote! {
                    #[config(shared = OBSERVABILITY_LOG_FILTER, default = PathBuf::new())]
                    log_filter: Operational<PathBuf>
                },
                "every consumer must spell it identically",
            ),
            (
                quote! {
                    #[config(shared = OBSERVABILITY_LOG_FILTER, name = "gateway.log_filter")]
                    log_filter: Operational<String>
                },
                "never both",
            ),
        ];
        for (field, expected) in cases {
            let error = expand(
                quote!(binary = "zeroship-gate", scope = "gateway"),
                quote! { struct Controls { #field, } },
            )
            .expect_err("a mis-declared shared identity must not expand");
            assert!(
                error.to_string().contains(expected),
                "expected {expected:?}, got {error}"
            );
        }

        // Does not cover: a WRONG entry in the shared table itself. Nothing here
        // can tell a correct canonical name from an incorrect one; the table is
        // the authority and only review checks it.
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
