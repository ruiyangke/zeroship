mod platform;

use super::*;

const TEST_BROKER_MASTER: &[u8] = b"gateway-test-broker-master-secret-32-bytes";

/// `avatar` is declared `string | null` on the SDK's `User`, so the key has
/// to be there even when the user has none. Omitting it made
/// `user.avatar === null` answer differently depending on whether the app
/// ran under `pnpm dev` or deployed, and `"avatar" in user` differ with it.
///
/// This pins the serialised shape only. It does not pin the SDK type, and
/// nothing here would notice if `User.avatar` were later made optional.
#[test]
fn worker_user_keeps_avatar_when_the_user_has_none() {
    let absent = WorkerUser {
        id: "pws_x",
        email: "a@b.c",
        name: "n",
        avatar: None,
        email_verified: true,
        scopes: vec!["openid"],
    };
    let json = serde_json::to_value(&absent).expect("serialises");
    assert_eq!(
        json.get("avatar"),
        Some(&serde_json::Value::Null),
        "avatar must serialise as null, not vanish: the SDK type declares it required"
    );

    // One-variable control: a user WITH an avatar must still carry the
    // value, so this cannot be satisfied by always emitting null.
    let present = WorkerUser {
        avatar: Some("https://example.test/a.png"),
        ..absent
    };
    let json = serde_json::to_value(&present).expect("serialises");
    assert_eq!(
        json.get("avatar").and_then(serde_json::Value::as_str),
        Some("https://example.test/a.png")
    );
}

fn test_broker_secret() -> BrokerSecret {
    BrokerSecret::from_bytes(TEST_BROKER_MASTER.to_vec()).expect("valid test broker secret")
}

fn make_stash() -> Stash {
    Stash {
        state: "s".into(),
        client_id: "oac_myapp".into(),
        verifier: "v".into(),
        nonce: "n".into(),
        original_path: "/p".into(),
        redirect_uri: "https://x/cb".into(),
    }
}

#[test]
fn authorize_url_has_required_params() {
    let rp = OidcRp::new(
        "https://auth.zeroship.ai",
        test_broker_secret(),
        b"signing-key-1234".to_vec(),
    );
    let (url, stash) = rp.build_authorize_redirect(
        "oac_myapp",
        "/some/path",
        "https://myapp.zeroship.ai/__zeroship/auth/callback",
    );
    assert!(url.starts_with("https://auth.zeroship.ai/oauth2/authorize?"));
    assert!(url.contains("client_id=oac_myapp"));
    assert!(url.contains("response_type=code"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("code_challenge="));
    assert!(url.contains("state="));
    assert!(url.contains("nonce="));
    // `scope=openid offline_access email profile` URL-encoded
    // (form_urlencoded uses `+` for space).
    assert!(url.contains("scope=openid+offline_access+email+profile"));
    // redirect_uri URL-encoded.
    assert!(url.contains("redirect_uri=https%3A%2F%2Fmyapp.zeroship.ai%2F__zeroship%2Fauth%2Fcallback"));
    // Stash is non-empty and contains the dot-separator.
    assert!(stash.contains('.'));
}

#[test]
fn browser_authorize_url_carries_browser_pkce_and_no_stash() {
    // In the browser flow, the gateway is HANDED the
    // already-computed code_challenge + browser-chosen state/nonce, and
    // injects the per-app PUBLIC client_id. No stash cookie is minted
    // (the verifier lives in the browser). Assert every passthrough
    // param lands and the per-app client_id (not "gateway") is used.
    let rp = OidcRp::new(
        "https://auth.zeroship.ai",
        test_broker_secret(),
        b"k".repeat(32),
    );
    let params = BrowserAuthorizeParams {
        code_challenge: "BROWSER_CHALLENGE_abc",
        state: "STATE_xyz",
        nonce: "NONCE_123",
        scope: "openid profile read:billing",
        redirect_uri: "https://myapp.zeroship.ai/__zeroship/auth/popup-callback",
        prompt: None,
        idp_hint: None,
    };
    let url = rp.build_browser_authorize_url("oac_myapp", &params);
    assert!(url.starts_with("https://auth.zeroship.ai/oauth2/authorize?"), "{url}");
    // PER-APP client_id, never the gateway confidential client.
    assert!(url.contains("client_id=oac_myapp"), "{url}");
    assert!(!url.contains("client_id=gateway"), "{url}");
    assert!(url.contains("response_type=code"));
    assert!(url.contains("code_challenge=BROWSER_CHALLENGE_abc"), "{url}");
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("state=STATE_xyz"), "{url}");
    assert!(url.contains("nonce=NONCE_123"), "{url}");
    // scope URL-encoded (form_urlencoded uses `+` for space).
    assert!(url.contains("scope=openid+profile+read%3Abilling"), "{url}");
    assert!(
        url.contains("redirect_uri=https%3A%2F%2Fmyapp.zeroship.ai%2F__zeroship%2Fauth%2Fpopup-callback"),
        "{url}"
    );
    // No prompt in the common case (so OP's SSO skip fires).
    assert!(!url.contains("prompt="), "default omits prompt: {url}");
    // No idp_hint unless the SDK supplied a provider.
    assert!(!url.contains("idp_hint="), "default omits idp_hint: {url}");
}

#[test]
fn browser_authorize_url_passes_prompt_through_when_present() {
    // `prompt=consent` (incremental-scope step-up) and `prompt=login`
    // (re-auth) are passed straight through; an empty prompt is dropped.
    let rp = OidcRp::new("https://auth.zeroship.ai", test_broker_secret(), b"k".repeat(32));
    let base = BrowserAuthorizeParams {
        code_challenge: "c",
        state: "s",
        nonce: "n",
        scope: "openid",
        redirect_uri: "https://app/cb",
        prompt: Some("consent"),
        idp_hint: None,
    };
    let url = rp.build_browser_authorize_url("oac_app", &base);
    assert!(url.contains("prompt=consent"), "{url}");

    let login = BrowserAuthorizeParams { prompt: Some("login"), ..base.clone() };
    assert!(rp.build_browser_authorize_url("oac_app", &login).contains("prompt=login"));

    let empty = BrowserAuthorizeParams { prompt: Some(""), ..base };
    assert!(!rp.build_browser_authorize_url("oac_app", &empty).contains("prompt="));
}

/// Fix 5 (MAJOR): `SignInOptions.provider` is threaded through as the
/// `idp_hint` authorize-URL param so the login UI can route to the named
/// upstream IdP. A present hint lands in the OP URL; an empty one is
/// dropped (mirrors the `prompt` passthrough discipline).
#[test]
fn browser_authorize_url_passes_idp_hint_through_when_present() {
    let rp = OidcRp::new("https://auth.zeroship.ai", test_broker_secret(), b"k".repeat(32));
    let base = BrowserAuthorizeParams {
        code_challenge: "c",
        state: "s",
        nonce: "n",
        scope: "openid",
        redirect_uri: "https://app/cb",
        prompt: None,
        idp_hint: Some("google"),
    };
    let url = rp.build_browser_authorize_url("oac_app", &base);
    assert!(url.contains("idp_hint=google"), "provider must reach the authorize URL: {url}");

    let github = BrowserAuthorizeParams { idp_hint: Some("github"), ..base.clone() };
    assert!(rp.build_browser_authorize_url("oac_app", &github).contains("idp_hint=github"));

    let password = BrowserAuthorizeParams { idp_hint: Some("password"), ..base.clone() };
    assert!(rp.build_browser_authorize_url("oac_app", &password).contains("idp_hint=password"));

    // An empty idp_hint is dropped (no `idp_hint=` in the URL).
    let empty = BrowserAuthorizeParams { idp_hint: Some(""), ..base };
    assert!(!rp.build_browser_authorize_url("oac_app", &empty).contains("idp_hint="));
}

#[test]
fn browser_authorize_url_trims_trailing_slash() {
    let rp = OidcRp::new("https://auth.zeroship.ai/", test_broker_secret(), b"k".repeat(32));
    let params = BrowserAuthorizeParams {
        code_challenge: "c",
        state: "s",
        nonce: "n",
        scope: "openid",
        redirect_uri: "https://app/cb",
        prompt: None,
        idp_hint: None,
    };
    let url = rp.build_browser_authorize_url("oac_app", &params);
    assert!(url.starts_with("https://auth.zeroship.ai/oauth2/authorize?"), "no double slash: {url}");
}

#[test]
fn new_derives_issuer_from_auth_ui_url_with_trailing_slash() {
    let rp = OidcRp::new(
        "https://auth.zeroship.ai",
        test_broker_secret(),
        b"k".repeat(32),
    );
    assert_eq!(rp.issuer, "https://auth.zeroship.ai/oauth2");

    // Trimming is idempotent — trailing slash on auth_ui_url must not
    // produce `//`.
    let rp = OidcRp::new(
        "https://auth.zeroship.ai/",
        test_broker_secret(),
        b"k".repeat(32),
    );
    assert_eq!(rp.issuer, "https://auth.zeroship.ai/oauth2");
}

#[test]
fn with_issuer_overrides_default_iss() {
    // Tests dial loopback auth but expect the canonical OP
    // issuer string — `with_issuer` decouples the two.
    let rp = OidcRp::new(
        "http://127.0.0.1:4444",
        test_broker_secret(),
        b"k".repeat(32),
    )
    .with_issuer("https://auth.zeroship.ai/oauth2");
    // `auth_ui_url` still drives /token + JWKS (loopback).
    assert_eq!(rp.auth_ui_url, "http://127.0.0.1:4444");
    // `issuer` is the logical OP issuer that ID tokens carry.
    assert_eq!(rp.issuer, "https://auth.zeroship.ai/oauth2");
}

#[test]
fn authorize_url_trims_trailing_slash_on_auth_ui_url() {
    let rp = OidcRp::new(
        "https://auth.zeroship.ai/",
        test_broker_secret(),
        b"k".repeat(32),
    );
    let (url, _) = rp.build_authorize_redirect("oac_app", "/", "https://app/cb");
    // No double slash before `/authorize`.
    assert!(url.starts_with("https://auth.zeroship.ai/oauth2/authorize?"));
}

#[test]
fn stash_roundtrips_through_sign_verify() {
    let key = b"k".repeat(32);
    let stash = make_stash();
    let encoded = stash.encode(&key);
    let decoded = Stash::decode(&encoded, &key).expect("decode");
    assert_eq!(decoded.state, "s");
    assert_eq!(decoded.verifier, "v");
    assert_eq!(decoded.nonce, "n");
    assert_eq!(decoded.original_path, "/p");
    assert_eq!(decoded.redirect_uri, "https://x/cb");
}

#[test]
fn stash_rejects_tampering() {
    let key = b"k".repeat(32);
    let stash = make_stash();
    // Flip the last character of the encoded cookie to corrupt the MAC.
    let mut tampered = stash.encode(&key);
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'A' { 'B' } else { 'A' });
    assert!(Stash::decode(&tampered, &key).is_none());
}

#[test]
fn stash_rejects_wrong_key() {
    let stash = make_stash();
    let encoded = stash.encode(b"k1234567890abcdef");
    assert!(Stash::decode(&encoded, b"different-key-here").is_none());
}

#[test]
fn stash_rejects_malformed_input() {
    let key = b"k".repeat(32);
    // No dot separator.
    assert!(Stash::decode("not-a-signed-cookie", &key).is_none());
    // Bad base64.
    assert!(Stash::decode("@@@.@@@", &key).is_none());
    // Empty.
    assert!(Stash::decode("", &key).is_none());
}

// ─── App session cookie ─────────────────────────────────────────

#[test]
fn app_session_set_cookie_is_always_host_secure() {
    // The cookie now carries a signed zeroship-sess+jwt token, not a UUID.
    let token = "eyJ.signed.token";
    let c = set_app_session_cookie(token);
    assert!(c.starts_with("__Host-zeroship_app_session="));
    assert!(c.contains(token));
    assert!(c.contains("Path=/"));
    assert!(c.contains("HttpOnly"));
    assert!(c.contains("SameSite=Lax"));
    assert!(c.contains("Secure"));
    // Short-lived signed cookie (~15 min), NOT the old 12h opaque id.
    assert!(c.contains("Max-Age=900"));
}

#[test]
fn app_session_clear_cookie_zero_max_age() {
    let c = clear_app_session_cookie();
    assert!(c.contains("Max-Age=0"));
    assert!(c.contains("Secure"));
    assert!(c.starts_with("__Host-"));
}

#[test]
fn app_session_parse_cookie_roundtrips() {
    // The value is now a signed token string (opaque to the parser).
    let token = "eyJhbGc.eyJzdWI.sig";
    let header = format!("foo=bar; __Host-zeroship_app_session={token}; baz=qux");
    assert_eq!(parse_app_session_cookie(&header).as_deref(), Some(token));
    assert_eq!(parse_app_session_cookie("nothing-here"), None);
    // Empty value ⇒ None (no token to verify).
    assert_eq!(parse_app_session_cookie("__Host-zeroship_app_session="), None);
    assert_eq!(parse_app_session_cookie(&format!("zeroship_app_session={token}")), None);
}

// ─── Stash cookie ───────────────────────────────────────────────

#[test]
fn stash_set_cookie_is_always_host_secure() {
    let c = set_stash_cookie("payload.signed");
    assert!(c.starts_with("__Host-zs_oidc_stash=payload.signed"));
    assert!(c.contains("Path=/"));
    assert!(c.contains("HttpOnly"));
    assert!(c.contains("SameSite=Lax"));
    assert!(c.contains("Secure"));
    assert!(c.contains("Max-Age=600")); // 10 min
}

#[test]
fn stash_clear_cookie_zero_max_age() {
    let c = clear_stash_cookie();
    assert!(c.contains("Max-Age=0"));
    assert!(c.contains("Secure"));
    assert!(c.starts_with("__Host-"));
}

#[test]
fn stash_parse_cookie_roundtrips() {
    let header = "foo=bar; __Host-zs_oidc_stash=abc.def; baz=qux";
    assert_eq!(parse_stash_cookie(header), Some("abc.def".into()));
    assert_eq!(parse_stash_cookie("nothing"), None);
    assert_eq!(parse_stash_cookie("zs_oidc_stash=abc.def"), None);
}

// ----- Cookie-arm granted-scope resolution --------------------------------

#[test]
fn granted_scopes_prefers_token_response_scope() {
    // When the token endpoint returns a non-blank `scope`, it is
    // authoritative and the access-token claim is ignored.
    let out = resolve_granted_scopes(
        Some("openid read:billing"),
        Some("openid offline_access SHOULD_NOT_APPEAR"),
    );
    assert_eq!(out, vec!["openid".to_string(), "read:billing".to_string()]);
}

#[test]
fn granted_scopes_falls_back_to_access_token_claim_when_response_scope_absent() {
    // RFC 6749 §5.1: the token-response `scope` MAY be omitted when the
    // granted scope equals the requested scope. The cookie arm must then
    // recover the consented scopes from the access-token `scope` claim —
    // NOT silently record `[]` (the bug this fix addresses).
    let out = resolve_granted_scopes(None, Some("openid offline_access read:billing"));
    assert_eq!(
        out,
        vec![
            "openid".to_string(),
            "offline_access".to_string(),
            "read:billing".to_string(),
        ],
        "absent token-response scope must fall back to the access-token claim"
    );
}

#[test]
fn granted_scopes_falls_back_when_response_scope_is_blank() {
    // An empty / whitespace-only `scope` string is treated the same as
    // absent — fall back to the access-token claim.
    let out = resolve_granted_scopes(Some("   "), Some("openid email"));
    assert_eq!(out, vec!["openid".to_string(), "email".to_string()]);
}

#[test]
fn granted_scopes_empty_when_both_sources_missing() {
    assert!(resolve_granted_scopes(None, None).is_empty());
    assert!(resolve_granted_scopes(Some(""), None).is_empty());
}
