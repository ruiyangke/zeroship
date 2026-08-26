//! WebIDL enum types for `Request`/`Response` per WHATWG Fetch.
//!
//! Each type derives [`WebIdlEnum`] so:
//!   - Construction from JS (`new Request(url, { mode: "cors" })`) goes
//!     through `WebIdlConvertible::from_v8` and rejects unknown values
//!     with a TypeError (Fetch §5.4 step 14 → WebIDL §3.13.7).
//!   - Round-trip from Rust to JS uses `as_str` (the canonical spec name).
//!
//! `Default` matches the spec default for each enum (the value the
//! constructor lands on when init.* is missing or undefined).
//!
//! # Behaviour change vs the v1 `RefCell<String>` storage
//!
//! v1 stored these fields as raw strings and silently accepted bogus
//! values like `mode: "bogus"`. WebIDL says unknown enum values must
//! throw TypeError, so this migration is a stricter, spec-correct
//! posture (Fetch §5.4 step 14 routes through WebIDL §3.13.7 step 4).
//!
//! Spec: <https://fetch.spec.whatwg.org/>.

use zeroship_runtime_macros::WebIdlEnum;

/// `RequestMode` per Fetch §5.4. Default is "no-cors" per the spec
/// (§5.4 step 14 sub-step "request mode" — for new requests built from
/// JS the default is "cors", but Fetch's storage default is "no-cors";
/// the constructor itself bumps to "cors" via the explicit
/// `state.mode = RequestMode::Cors` initial in `RequestState::default`).
///
/// We honour the JS-observable default ("cors") via `RequestState::
/// default()` rather than `Default::default()` on the enum — the macro's
/// dict-derive uses `Default::default()` when init.mode is missing,
/// which would land us on `NoCors` and break callers that read
/// `req.mode` after `new Request(url)`. The constructor body explicitly
/// sets `state.mode = RequestMode::Cors` before applying the dict, so
/// the dict's "missing" path leaves "cors" intact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, WebIdlEnum)]
pub enum RequestMode {
    #[default]
    #[webidl_name = "no-cors"]
    NoCors,
    #[webidl_name = "cors"]
    Cors,
    #[webidl_name = "same-origin"]
    SameOrigin,
    #[webidl_name = "navigate"]
    Navigate,
}

/// `RequestCredentials` per Fetch §5.4. Default is "same-origin".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, WebIdlEnum)]
pub enum RequestCredentials {
    #[default]
    #[webidl_name = "same-origin"]
    SameOrigin,
    #[webidl_name = "include"]
    Include,
    #[webidl_name = "omit"]
    Omit,
}

/// `RequestCache` per Fetch §5.4. Default is "default".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, WebIdlEnum)]
pub enum RequestCache {
    #[default]
    Default,
    #[webidl_name = "no-store"]
    NoStore,
    Reload,
    #[webidl_name = "no-cache"]
    NoCache,
    #[webidl_name = "force-cache"]
    ForceCache,
    #[webidl_name = "only-if-cached"]
    OnlyIfCached,
}

/// `RequestRedirect` per Fetch §5.4. Default is "follow".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, WebIdlEnum)]
pub enum RequestRedirect {
    #[default]
    Follow,
    Error,
    Manual,
}

/// `RequestDestination` per Fetch §5.4. Default is the empty string —
/// "request destination, by default, is the empty string". The empty
/// variant is named `Empty` to keep the Rust ident valid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, WebIdlEnum)]
pub enum RequestDestination {
    #[default]
    #[webidl_name = ""]
    Empty,
    Audio,
    #[webidl_name = "audioworklet"]
    AudioWorklet,
    Document,
    Embed,
    Font,
    Image,
    Manifest,
    Object,
    #[webidl_name = "paintworklet"]
    PaintWorklet,
    Report,
    Script,
    #[webidl_name = "sharedworker"]
    SharedWorker,
    Style,
    Track,
    Video,
    Worker,
    #[webidl_name = "xslt"]
    XSLT,
}

/// `ReferrerPolicy` per Fetch §5.4. Default is the empty string per
/// the Referrer Policy spec — "the user agent's default policy
/// applies when no policy is set".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, WebIdlEnum)]
pub enum ReferrerPolicy {
    #[default]
    #[webidl_name = ""]
    Empty,
    #[webidl_name = "no-referrer"]
    NoReferrer,
    #[webidl_name = "no-referrer-when-downgrade"]
    NoReferrerWhenDowngrade,
    #[webidl_name = "same-origin"]
    SameOrigin,
    #[webidl_name = "origin"]
    Origin,
    #[webidl_name = "strict-origin"]
    StrictOrigin,
    #[webidl_name = "origin-when-cross-origin"]
    OriginWhenCrossOrigin,
    #[webidl_name = "strict-origin-when-cross-origin"]
    StrictOriginWhenCrossOrigin,
    #[webidl_name = "unsafe-url"]
    UnsafeURL,
}

/// `ResponseType` per Fetch §5.5. Default per the spec is "default"
/// (response.type for a basic constructor-built response). Note Fetch
/// also distinguishes `basic` / `cors` / `error` / `opaque` /
/// `opaqueredirect`; for v1 we only emit `Default` (constructor) and
/// `Error` (Response.error()).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, WebIdlEnum)]
pub enum ResponseType {
    Basic,
    Cors,
    #[default]
    Default,
    Error,
    Opaque,
    #[webidl_name = "opaqueredirect"]
    OpaqueRedirect,
}

// ---------------------------------------------------------------------------
// Bridge to the algorithm-side enums in `algorithms.rs`.
//
// `algorithms::RedirectMode` and `algorithms::CredentialsMode` are
// internal to the fetch state machine; `RequestRedirect` and
// `RequestCredentials` are the JS-facing WebIDL enums. The two sides
// are isomorphic (variant-for-variant). Bridge via `From` so the
// kernel-side `snapshot_request` reads the typed state and converts
// once to the algorithm shape.
// ---------------------------------------------------------------------------

use super::algorithms::{CredentialsMode, RedirectMode};

impl From<RequestRedirect> for RedirectMode {
    fn from(value: RequestRedirect) -> Self {
        match value {
            RequestRedirect::Follow => RedirectMode::Follow,
            RequestRedirect::Error => RedirectMode::Error,
            RequestRedirect::Manual => RedirectMode::Manual,
        }
    }
}

impl From<RequestCredentials> for CredentialsMode {
    fn from(value: RequestCredentials) -> Self {
        match value {
            RequestCredentials::Omit => CredentialsMode::Omit,
            RequestCredentials::SameOrigin => CredentialsMode::SameOrigin,
            RequestCredentials::Include => CredentialsMode::Include,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_mode_round_trip() {
        assert_eq!(RequestMode::from_str("no-cors"), Some(RequestMode::NoCors));
        assert_eq!(RequestMode::from_str("cors"), Some(RequestMode::Cors));
        assert_eq!(
            RequestMode::from_str("same-origin"),
            Some(RequestMode::SameOrigin),
        );
        assert_eq!(RequestMode::from_str("navigate"), Some(RequestMode::Navigate));
        assert_eq!(RequestMode::from_str("bogus"), None);
        assert_eq!(RequestMode::Cors.as_str(), "cors");
        assert_eq!(RequestMode::SameOrigin.as_str(), "same-origin");
    }

    #[test]
    fn request_credentials_round_trip() {
        assert_eq!(
            RequestCredentials::from_str("same-origin"),
            Some(RequestCredentials::SameOrigin),
        );
        assert_eq!(
            RequestCredentials::from_str("include"),
            Some(RequestCredentials::Include),
        );
        assert_eq!(
            RequestCredentials::from_str("omit"),
            Some(RequestCredentials::Omit),
        );
        assert_eq!(RequestCredentials::from_str("BOGUS"), None);
    }

    #[test]
    fn request_cache_round_trip() {
        assert_eq!(RequestCache::from_str("default"), Some(RequestCache::Default));
        assert_eq!(RequestCache::from_str("no-store"), Some(RequestCache::NoStore));
        assert_eq!(RequestCache::from_str("no-cache"), Some(RequestCache::NoCache));
        assert_eq!(
            RequestCache::from_str("only-if-cached"),
            Some(RequestCache::OnlyIfCached),
        );
        assert_eq!(RequestCache::Default.as_str(), "default");
        assert_eq!(RequestCache::NoStore.as_str(), "no-store");
    }

    #[test]
    fn request_redirect_round_trip() {
        assert_eq!(RequestRedirect::from_str("follow"), Some(RequestRedirect::Follow));
        assert_eq!(RequestRedirect::from_str("error"), Some(RequestRedirect::Error));
        assert_eq!(RequestRedirect::from_str("manual"), Some(RequestRedirect::Manual));
        assert_eq!(RequestRedirect::Follow.as_str(), "follow");
    }

    #[test]
    fn request_destination_empty_default() {
        assert_eq!(RequestDestination::default(), RequestDestination::Empty);
        assert_eq!(RequestDestination::Empty.as_str(), "");
        assert_eq!(
            RequestDestination::from_str("audio"),
            Some(RequestDestination::Audio),
        );
        assert_eq!(
            RequestDestination::from_str("audioworklet"),
            Some(RequestDestination::AudioWorklet),
        );
        assert_eq!(
            RequestDestination::from_str(""),
            Some(RequestDestination::Empty),
        );
    }

    #[test]
    fn referrer_policy_round_trip() {
        assert_eq!(ReferrerPolicy::default(), ReferrerPolicy::Empty);
        assert_eq!(ReferrerPolicy::Empty.as_str(), "");
        assert_eq!(
            ReferrerPolicy::from_str("no-referrer"),
            Some(ReferrerPolicy::NoReferrer),
        );
        assert_eq!(
            ReferrerPolicy::from_str("strict-origin-when-cross-origin"),
            Some(ReferrerPolicy::StrictOriginWhenCrossOrigin),
        );
        assert_eq!(
            ReferrerPolicy::from_str("unsafe-url"),
            Some(ReferrerPolicy::UnsafeURL),
        );
    }

    #[test]
    fn response_type_round_trip() {
        assert_eq!(ResponseType::default(), ResponseType::Default);
        assert_eq!(ResponseType::Default.as_str(), "default");
        assert_eq!(ResponseType::Error.as_str(), "error");
        assert_eq!(
            ResponseType::from_str("opaqueredirect"),
            Some(ResponseType::OpaqueRedirect),
        );
    }

    #[test]
    fn redirect_mode_bridge() {
        assert_eq!(RedirectMode::from(RequestRedirect::Follow), RedirectMode::Follow);
        assert_eq!(RedirectMode::from(RequestRedirect::Error), RedirectMode::Error);
        assert_eq!(RedirectMode::from(RequestRedirect::Manual), RedirectMode::Manual);
    }

    #[test]
    fn credentials_mode_bridge() {
        assert_eq!(
            CredentialsMode::from(RequestCredentials::SameOrigin),
            CredentialsMode::SameOrigin,
        );
        assert_eq!(
            CredentialsMode::from(RequestCredentials::Include),
            CredentialsMode::Include,
        );
        assert_eq!(
            CredentialsMode::from(RequestCredentials::Omit),
            CredentialsMode::Omit,
        );
    }
}
