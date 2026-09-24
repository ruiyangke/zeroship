//! Identities declared by more than one binary.
//!
//! A shared setting has ONE canonical name, ONE supply class and ONE resolved
//! type across every consumer, because an operator sets it once: there is a
//! single `ZEROSHIP_*` spelling and a single overlay path behind it. Restating
//! the canonical string and the wrapper type in each consumer's own struct
//! makes two failures representable and neither detectable:
//!
//!   * a TYPO de-shared the identity. `observability.log_filtr` in one binary is
//!     a new, unique canonical name with its own env projection, its own flag
//!     and a matching read site, so every registry check passes while the
//!     operator's variable is silently ignored by that one service.
//!   * a DIVERGENT wrapper or inner type gave one environment variable two
//!     parse behaviours and, for `Secret` versus `Operational`, two different
//!     redaction rules.
//!
//! Naming the identity by SYMBOL removes both: an unknown symbol does not
//! compile, and the class and type are read from here rather than restated.
//!
//! What a consumer still chooses is the compiled DEFAULT, and that is
//! deliberate. `observability.log_filter` defaults to `info,zeroship_X=debug`
//! where `X` is the declaring crate; there is no single value that is correct
//! for every binary, so a rule requiring default agreement would have no
//! satisfiable form.
//!
//! This is not a checked-in name manifest. What is rejected about one is that
//! it is a second place a name is spelled, which can be edited to agree with a
//! typo while the declaration differs. Here the table is the ONLY place a
//! shared name is spelled: `#[config(name = "...")]` REFUSES a string that
//! matches a shared canonical name, so there is no second spelling to drift
//! from.

/// One shared identity: its symbol, canonical name, and required declaration.
pub(crate) struct SharedIdentity {
    /// The symbol a declaration writes in `#[config(shared = ...)]`.
    pub(crate) symbol: &'static str,
    /// The canonical dotted name every consumer projects from.
    pub(crate) canonical: &'static str,
    /// The required wrapper, spelled exactly as the field must spell it.
    pub(crate) wrapper: &'static str,
    /// The required inner type, spelled exactly as the field must spell it.
    pub(crate) inner: &'static str,
}

/// Every identity more than one binary is permitted to declare.
///
/// Adding an entry is deliberate: it asserts that the setting means the same
/// thing everywhere and that one operator-visible name governs all consumers.
pub(crate) const SHARED_IDENTITIES: &[SharedIdentity] = &[
    SharedIdentity {
        symbol: "CONFIG",
        canonical: "config",
        wrapper: "BootstrapControl",
        inner: "Option<PathBuf>",
    },
    SharedIdentity {
        symbol: "NO_CONFIG",
        canonical: "no_config",
        wrapper: "BootstrapControl",
        inner: "bool",
    },
    SharedIdentity {
        symbol: "CHECK_CONFIG",
        canonical: "check_config",
        wrapper: "CommandControl",
        inner: "bool",
    },
    SharedIdentity {
        symbol: "CHECK_CONFIG_FORMAT",
        canonical: "check_config_format",
        wrapper: "CommandControl",
        inner: "CheckFormat",
    },
    SharedIdentity {
        symbol: "OBSERVABILITY_LOG_FILTER",
        canonical: "observability.log_filter",
        wrapper: "Operational",
        inner: "String",
    },
    SharedIdentity {
        symbol: "OBSERVABILITY_LOG_FORMAT",
        canonical: "observability.log_format",
        wrapper: "Operational",
        inner: "LogFormat",
    },
    // Platform-global operational identities. Each has no component prefix
    // because it means the same thing to every consumer: one blob store, one
    // control plane, one worker fleet. A per-binary name here would be the
    // "one concept, six operator-visible names" outcome the design rejects.
    SharedIdentity {
        symbol: "BLOB_STORE",
        canonical: "blob_store",
        wrapper: "Operational",
        inner: "String",
    },
    SharedIdentity {
        symbol: "CONTROL_URL",
        canonical: "control_url",
        wrapper: "Operational",
        inner: "String",
    },
    SharedIdentity {
        symbol: "WORKER_URLS",
        canonical: "worker_urls",
        wrapper: "Operational",
        inner: "String",
    },
    SharedIdentity {
        symbol: "POLL_INTERVAL",
        canonical: "poll_interval",
        wrapper: "Operational",
        inner: "u64",
    },
    SharedIdentity {
        symbol: "ORIGIN_SCHEME",
        canonical: "origin_scheme",
        wrapper: "Operational",
        inner: "OriginScheme",
    },
    SharedIdentity {
        symbol: "TRUST_PROXY",
        canonical: "trust_proxy",
        wrapper: "Operational",
        inner: "bool",
    },
    // Which peers this deployment may reach over plaintext HTTP. ONE list for
    // the whole deployment, because an operator decides once which private
    // origins carry service calls in clear, and the three services that make
    // those calls - control, the worker and the workflow manager - would
    // otherwise each carry their own spelling of the same decision.
    SharedIdentity {
        symbol: "PLAINTEXT_PEERS",
        canonical: "plaintext_peers",
        wrapper: "Operational",
        inner: "Vec<PlaintextPeer>",
    },
    SharedIdentity {
        symbol: "OAUTH_AUDIENCE",
        canonical: "oauth_audience",
        wrapper: "Operational",
        inner: "String",
    },
    // Auth-domain identities. These sit under `auth.` because they configure
    // the auth domain rather than any one binary, and the overlay already
    // carries that table.
    //
    // The provider selector is ONE deployment decision seen from two vantage
    // points: auth SERVES the provider, control VERIFIES its tokens. Two
    // identities with two value sets (`native|supabase` against
    // `platform|supabase`) would have no mechanism keeping them coherent, so
    // an operator could set them to disagree and find out at request time. The
    // typed inner enum is what makes the disagreement unrepresentable; the
    // shared name is what makes it one operator-visible setting.
    SharedIdentity {
        symbol: "AUTH_PROVIDER",
        canonical: "auth.provider",
        wrapper: "Operational",
        inner: "AuthProviderKind",
    },
    SharedIdentity {
        symbol: "AUTH_PLATFORM_ISSUER",
        canonical: "auth.platform_issuer",
        wrapper: "Operational",
        inner: "String",
    },
    SharedIdentity {
        symbol: "AUTH_PLATFORM_JWKS_URL",
        canonical: "auth.platform_jwks_url",
        wrapper: "Operational",
        inner: "String",
    },
    // The GoTrue base URL is ONE deployment fact read by two binaries: control
    // verifies Supabase tokens against it and auth drives the browser-side
    // GoTrue login with it. A single consumer writes the name directly; two
    // readers declare the sharing here.
    SharedIdentity {
        symbol: "AUTH_SUPABASE_URL",
        canonical: "auth.supabase_url",
        wrapper: "Operational",
        inner: "String",
    },
    // Platform-global SECRETS. These are the shape the flat `[secrets]` table
    // could not express: one value, set once, read by several binaries. Without
    // a marker for sharing, the same key splits between literals in some
    // consumers and an env indirection in another, so the sharing is declared
    // here instead.
    SharedIdentity {
        symbol: "CONTROL_KEY",
        canonical: "control_key",
        wrapper: "Secret",
        inner: "String",
    },
    SharedIdentity {
        symbol: "PAIRWISE_SALT",
        canonical: "pairwise_salt",
        wrapper: "Secret",
        inner: "String",
    },
    // The Supabase anon key is one deployment fact read by two binaries: auth
    // drives the browser-side GoTrue login with it and control verifies tokens
    // from the same project. It stays OPERATIONAL, not secret: Supabase
    // publishes it to browsers by design, so redacting it here would claim a
    // protection the value does not have.
    SharedIdentity {
        symbol: "AUTH_SUPABASE_ANON_KEY",
        canonical: "auth.supabase_anon_key",
        wrapper: "Operational",
        inner: "String",
    },
    // Usage-metering stream identities. ONE deployment fact - which broker the
    // billing events go to, and under which topic - seen by the two producers
    // (worker and gateway). They sit under `metering.` rather than carrying a
    // binary prefix for the same reason `blob_store` does not: an operator
    // points a deployment at one stream, not at a per-service stream.
    //
    // The worker has no TOML overlay - its overlay source is removed as a
    // credential boundary - so without these identities it has no interface
    // for the stream and every metering harness has to set ambient variables
    // on the command prefix.
    SharedIdentity {
        symbol: "METERING_BROKERS",
        canonical: "metering.brokers",
        wrapper: "Operational",
        inner: "String",
    },
    SharedIdentity {
        symbol: "METERING_EVENTS_TOPIC",
        canonical: "metering.events_topic",
        wrapper: "Operational",
        inner: "String",
    },
    SharedIdentity {
        symbol: "METERING_PRODUCER_GROUP_ID",
        canonical: "metering.producer_group_id",
        wrapper: "Operational",
        inner: "String",
    },
    SharedIdentity {
        symbol: "METERING_OUTBOX_WAL_PATH",
        canonical: "metering.outbox_wal_path",
        wrapper: "Operational",
        inner: "String",
    },
];

/// Look up a shared identity by the symbol a declaration wrote.
pub(crate) fn by_symbol(symbol: &str) -> Option<&'static SharedIdentity> {
    SHARED_IDENTITIES
        .iter()
        .find(|identity| identity.symbol == symbol)
}

/// Look up a shared identity by canonical name.
///
/// Used to REFUSE `#[config(name = "...")]` for a shared canonical name, which
/// is what keeps the table the single spelling rather than a second one.
pub(crate) fn by_canonical(canonical: &str) -> Option<&'static SharedIdentity> {
    SHARED_IDENTITIES
        .iter()
        .find(|identity| identity.canonical == canonical)
}

/// The known symbols, for a diagnostic that tells the author what to write.
pub(crate) fn known_symbols() -> String {
    SHARED_IDENTITIES
        .iter()
        .map(|identity| identity.symbol)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Compare a declared type against a required spelling, ignoring whitespace.
///
/// Deliberately TEXTUAL. A proc macro cannot resolve a path to a type, so this
/// cannot tell `PathBuf` from a local alias of the same name; what it does
/// guarantee is that every consumer of one identity writes the same tokens,
/// which is the property that makes a divergence visible in review.
pub(crate) fn type_matches(declared: &str, required: &str) -> bool {
    fn squeeze(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }
    squeeze(declared) == squeeze(required)
}

#[cfg(test)]
mod tests {
    use super::{by_canonical, by_symbol, type_matches, SHARED_IDENTITIES};

    #[test]
    fn every_symbol_and_canonical_name_is_unique() {
        // Two entries sharing a symbol would make `shared = X` ambiguous; two
        // sharing a canonical name would let one identity carry two required
        // types, which is the divergence this table exists to prevent.
        let mut symbols: Vec<&str> = SHARED_IDENTITIES.iter().map(|i| i.symbol).collect();
        let count = symbols.len();
        symbols.sort_unstable();
        symbols.dedup();
        assert_eq!(symbols.len(), count, "duplicate shared symbol");

        let mut names: Vec<&str> = SHARED_IDENTITIES.iter().map(|i| i.canonical).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate shared canonical name");
    }

    #[test]
    fn lookups_agree_with_the_table() {
        let filter = by_symbol("OBSERVABILITY_LOG_FILTER").expect("known symbol");
        assert_eq!(filter.canonical, "observability.log_filter");
        assert_eq!(filter.wrapper, "Operational");
        assert_eq!(filter.inner, "String");
        assert_eq!(
            by_canonical("observability.log_filter").map(|i| i.symbol),
            Some("OBSERVABILITY_LOG_FILTER")
        );
        assert!(by_symbol("OBSERVABILITY_LOG_FILTR").is_none());
        assert!(by_canonical("control.port").is_none());
    }

    #[test]
    fn the_type_comparison_ignores_only_whitespace() {
        assert!(type_matches("Option < PathBuf >", "Option<PathBuf>"));
        assert!(type_matches("Option<PathBuf>", "Option<PathBuf>"));
        assert!(!type_matches("Option<String>", "Option<PathBuf>"));
        // Does not cover: a path-qualified spelling such as
        // `std::path::PathBuf`. That is REJECTED on purpose - the point is one
        // spelling per identity - and the diagnostic names the required form.
        assert!(!type_matches("std::path::PathBuf", "PathBuf"));
    }
}
