use zeroship_bundle::{
    AssetEntry, AssetVariant, HttpMethod, Manifest, ManifestMetadata, Match,
    ProcedureKind, RedirectAction, RequiredPrincipal, ResourceEntry, StaticAction, WorkerCode,
};
use zeroship_core::net_policy::Verdict;
use zeroship_core::types::{
    AccountState, AppNetPolicy, AppRuntimeLimits, AppUsage, AppVersionInfo, ControlEvent,
    NetEgressEntry, RouteEntry, SpendState,
};
use std::collections::HashMap;
use uuid::Uuid;

#[test]
fn control_event_deploy_json() {
    let app_id = Uuid::new_v4();
    let event = ControlEvent::Deploy {
        app_id,
        hash: "abc123".to_string(),
    };

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(&app_id.to_string()));
    assert!(json.contains("abc123"));

    let decoded: ControlEvent = serde_json::from_str(&json).unwrap();
    match decoded {
        ControlEvent::Deploy { app_id: id, hash: h } => {
            assert_eq!(id, app_id);
            assert_eq!(h, "abc123");
        }
        other => panic!("expected Deploy, got {:?}", other),
    }
}

#[test]
fn control_event_delete_json() {
    let app_id = Uuid::new_v4();
    let event = ControlEvent::Delete { app_id };

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(&app_id.to_string()));

    let decoded: ControlEvent = serde_json::from_str(&json).unwrap();
    match decoded {
        ControlEvent::Delete { app_id: id } => assert_eq!(id, app_id),
        other => panic!("expected Delete, got {:?}", other),
    }
}

#[test]
fn app_usage_empty_custom_omitted_and_defaults() {
    // Empty `custom` must not appear on the wire, and a payload without
    // `custom` must deserialize with an empty map (pre-launch shape
    // discipline — readers that omit it still load).
    let usage = AppUsage {
        requests: 1,
        ..AppUsage::default()
    };
    let json = serde_json::to_string(&usage).unwrap();
    assert!(!json.contains("\"custom\""), "empty custom omitted: {json}");

    let legacy = r#"{"requests":3,"cpu_us":0,"wall_us":0,"egress_bytes":0,"ingress_bytes":0}"#;
    let decoded: AppUsage = serde_json::from_str(legacy).unwrap();
    assert_eq!(decoded.requests, 3);
    assert!(decoded.custom.is_empty());
}

#[test]
fn route_entry_roundtrip() {
    // A provisioned app: both OAuth identity fields carry `Some`. They
    // must survive the serialize/deserialize round-trip intact.
    let entry = RouteEntry {
        name: "my-app".to_string(),
        plan_id: "pro".to_string(),
        deploy_hash: Some("abc123".to_string()),
        manifest: Manifest::passthrough(),
        oauth_client_id: Some("oac_myapp".to_string()),
        sector_identifier: Some("https://my-app.zeroship.ai".to_string()),
        spend_state: SpendState::Degrade,
        account_state: AccountState::PastDue,
    };

    let json = serde_json::to_string(&entry).unwrap();
    let decoded: RouteEntry = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.name, "my-app");
    assert_eq!(decoded.spend_state, SpendState::Degrade);
    // G2: account_state survives the round trip independently of spend_state.
    assert_eq!(decoded.account_state, AccountState::PastDue);
    assert_eq!(decoded.plan_id, "pro");
    assert_eq!(decoded.deploy_hash, Some("abc123".to_string()));
    assert_eq!(decoded.oauth_client_id, Some("oac_myapp".to_string()));
    assert_eq!(
        decoded.sector_identifier,
        Some("https://my-app.zeroship.ai".to_string())
    );
    // Every route has a manifest after deserialization. The synthesized
    // passthrough has at least one resource entry (`*`).
    assert!(
        !decoded.manifest.resources.is_empty(),
        "passthrough has resources"
    );
}

#[test]
fn route_entry_oauth_fields_none_round_trip() {
    // An un-provisioned app: both OAuth fields are `None`. `None` must
    // round-trip as `None` (not error, not flip to `Some("")`) so the
    // gateway can hold the entry and hard-fail closed on the auth path.
    let entry = RouteEntry {
        name: "unprovisioned".to_string(),
        plan_id: "free".to_string(),
        deploy_hash: None,
        manifest: Manifest::passthrough(),
        oauth_client_id: None,
        sector_identifier: None,
        spend_state: SpendState::default(),
        account_state: AccountState::default(),
    };
    let json = serde_json::to_string(&entry).unwrap();
    let decoded: RouteEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.oauth_client_id, None);
    assert_eq!(decoded.sector_identifier, None);
    assert_eq!(decoded.spend_state, SpendState::Allow);
    assert_eq!(decoded.account_state, AccountState::Active);
}

#[test]
fn route_entry_missing_manifest_field_synthesizes_passthrough() {
    // Older rows (or rows produced before the manifest field) must still
    // deserialize cleanly — the missing field defaults to passthrough.
    let json = r#"{
        "name": "legacy-app",
        "plan_id": "free",
        "deploy_hash": null
    }"#;
    let decoded: RouteEntry = serde_json::from_str(json).unwrap();
    assert!(
        !decoded.manifest.resources.is_empty(),
        "default to passthrough"
    );
    assert_eq!(
        decoded.spend_state,
        SpendState::Allow,
        "missing spend_state defaults to Allow (forward-load tolerance)"
    );
}

#[test]
fn spend_state_serde_is_snake_case_and_defaults_allow() {
    // Wire / TEXT-column form is snake_case lower; Default is Allow.
    assert_eq!(serde_json::to_string(&SpendState::Allow).unwrap(), "\"allow\"");
    assert_eq!(serde_json::to_string(&SpendState::Warn).unwrap(), "\"warn\"");
    assert_eq!(
        serde_json::to_string(&SpendState::Degrade).unwrap(),
        "\"degrade\""
    );
    assert_eq!(serde_json::to_string(&SpendState::Block).unwrap(), "\"block\"");
    for (s, v) in [
        ("\"allow\"", SpendState::Allow),
        ("\"warn\"", SpendState::Warn),
        ("\"degrade\"", SpendState::Degrade),
        ("\"block\"", SpendState::Block),
    ] {
        assert_eq!(serde_json::from_str::<SpendState>(s).unwrap(), v);
    }
    assert_eq!(SpendState::default(), SpendState::Allow);
}

#[test]
fn control_event_spend_state_json() {
    let app_id = Uuid::new_v4();
    let event = ControlEvent::SpendState {
        app_id,
        state: SpendState::Block,
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("SpendState"));
    assert!(json.contains("block"));
    let decoded: ControlEvent = serde_json::from_str(&json).unwrap();
    match decoded {
        ControlEvent::SpendState { app_id: id, state } => {
            assert_eq!(id, app_id);
            assert_eq!(state, SpendState::Block);
        }
        other => panic!("expected SpendState, got {:?}", other),
    }
}

#[test]
fn route_entry_mixed_default_deserializes() {
    // The §1.5 "mixed default": a `RouteEntry` produced before the
    // OAuth-fields join lands (or by a hand-rolled fixture) omits BOTH
    // the manifest and the OAuth fields. It must deserialize with
    // `manifest = passthrough` AND `oauth_client_id/sector_identifier =
    // None` — all three serde defaults firing together.
    let json = r#"{
        "name": "mixed-default-app",
        "plan_id": "free",
        "deploy_hash": null
    }"#;
    let decoded: RouteEntry = serde_json::from_str(json).unwrap();
    assert!(
        !decoded.manifest.resources.is_empty(),
        "manifest defaults to passthrough"
    );
    assert_eq!(
        decoded.oauth_client_id, None,
        "oauth_client_id defaults to None"
    );
    assert_eq!(
        decoded.sector_identifier, None,
        "sector_identifier defaults to None"
    );
}

#[test]
fn match_exact() {
    let m = Match::Exact { method: None, path: "/robots.txt".into() };
    assert!(m.test("GET", "/robots.txt").is_some());
    assert!(m.test("GET", "/robots.txt/").is_none());
    assert!(m.test("GET", "/other").is_none());
}

#[test]
fn match_prefix_with_method() {
    let m = Match::Prefix { method: Some(HttpMethod::Post), path: "/_rpc/".into() };
    assert!(m.test("POST", "/_rpc/listTodos").is_some());
    assert!(m.test("GET", "/_rpc/listTodos").is_none());
    assert!(m.test("POST", "/api/listTodos").is_none());
}

#[test]
fn match_prefix_segment_boundary() {
    // Prefix `/admin` must NOT match `/administrator` — the boundary
    // check exists to prevent that classic footgun.
    let m = Match::Prefix { method: None, path: "/admin".into() };
    assert!(m.test("GET", "/admin").is_some());
    assert!(m.test("GET", "/admin/users").is_some());
    assert!(m.test("GET", "/administrator").is_none());
}

#[test]
fn match_glob_single() {
    let m = Match::Glob { method: None, path: "/blog/[slug]".into() };
    let caps = m.test("GET", "/blog/hello").expect("matches");
    assert_eq!(caps.get("slug").map(String::as_str), Some("hello"));
    assert!(m.test("GET", "/blog/").is_none(), "empty segment doesn't match");
}

#[test]
fn match_glob_catchall() {
    let m = Match::Glob { method: None, path: "/api/[...rest]".into() };
    let caps = m.test("POST", "/api/v1/users").expect("matches");
    assert_eq!(caps.get("rest").map(String::as_str), Some("v1/users"));
}

#[test]
fn match_any() {
    let m = Match::Any;
    assert!(m.test("GET", "/anything").is_some());
    assert!(m.test("DELETE", "/random/path").is_some());
}

// -- Manifest schema -------------------------------------------------------

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHA64: &str = "abababababababababababababababababababababababababababababababab";

#[test]
fn manifest_default_version_is_one() {
    let m = Manifest::default();
    assert_eq!(m.version, 1, "default schema version is v1");
}

#[test]
fn manifest_validate_rejects_v2() {
    let m = Manifest { version: 2, ..Manifest::default() };
    let err = m.validate().expect_err("v2 is no longer accepted");
    assert!(err.contains("version 2"), "error mentions bad version: {err}");
    assert!(
        err.to_lowercase().contains("version 1") || err.to_lowercase().contains("only version 1"),
        "error explains v1 is the accepted shape: {err}"
    );
}

#[test]
fn manifest_validate_rejects_v3() {
    let m = Manifest { version: 3, ..Manifest::default() };
    let err = m.validate().expect_err("v3 is no longer accepted");
    assert!(err.contains("version 3"), "error mentions bad version: {err}");
}

#[test]
fn manifest_validate_rejects_v0() {
    let m = Manifest { version: 0, ..Manifest::default() };
    assert!(m.validate().is_err(), "version 0 rejected");
}

#[test]
fn manifest_validate_rejects_unsupported_version() {
    let m = Manifest { version: 99, ..Manifest::default() };
    let err = m.validate().expect_err("unsupported version");
    assert!(err.contains("99"), "error mentions bad version: {err}");
}

#[test]
fn manifest_passthrough_v1_validates() {
    let p = Manifest::passthrough();
    assert_eq!(p.version, 1, "passthrough is v1-shape");
    assert!(p.worker.is_none(), "passthrough has no worker");
    assert_eq!(p.metadata.built_at, "1970-01-01T00:00:00Z");
    assert!(
        p.metadata
            .compiler
            .as_deref()
            .is_some_and(|s| s.starts_with("zeroship-passthrough@")),
        "passthrough compiler tag should be set: {:?}",
        p.metadata.compiler,
    );
    p.validate().expect("passthrough must validate");
}

#[test]
fn manifest_v1_minimal_round_trips() {
    // Minimal wire format with no `version` field: deserializes at the
    // default version (v1) and all collections default to empty.
    let json = r#"{
        "assets": {},
        "runtime_assets": {},
        "asset_version": 0
    }"#;
    let m: Manifest = serde_json::from_str(json).unwrap();
    assert_eq!(m.version, 1);
    assert!(m.assets.is_empty());
    assert!(m.sourcemaps.is_empty());
    assert!(m.resources.is_empty());
    assert!(m.worker.is_none());
    assert_eq!(m.metadata.built_at, "");
    m.validate().expect("minimal manifest must validate");
}

#[test]
fn manifest_v1_round_trips_resources() {
    let mut resources = HashMap::new();
    resources.insert(
        "*".into(),
        ResourceEntry {
            auth: Some(RequiredPrincipal::Anonymous),
            publicly_accessible: Some(true),
            ..Default::default()
        },
    );
    resources.insert(
        "rpc:todos.add".into(),
        ResourceEntry {
            kind: Some(ProcedureKind::Mutation),
            idempotent: Some(true),
            ..Default::default()
        },
    );
    resources.insert(
        "/api".into(),
        ResourceEntry {
            auth: Some(RequiredPrincipal::User),
            r#override: vec!["auth".into()],
            ..Default::default()
        },
    );
    let mut schemas = HashMap::new();
    schemas.insert(format!("sha256:{SHA64}"), serde_json::json!({"type": "object"}));
    let mut aliases = HashMap::new();
    aliases.insert(
        "src/server/todos.ts::list".to_string(),
        "rpc:todos.list".to_string(),
    );
    let m = Manifest {
        version: 1,
        resources,
        schemas,
        aliases,
        transformer: Some("superjson".into()),
        ..Manifest::default()
    };
    let json = serde_json::to_string(&m).expect("serialize");
    let d: Manifest = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(d.version, 1);
    assert_eq!(d.resources.len(), 3);
    assert_eq!(d.schemas.len(), 1);
    assert_eq!(d.aliases.len(), 1);
    assert_eq!(d.transformer.as_deref(), Some("superjson"));
    assert_eq!(d.resources["rpc:todos.add"].kind, Some(ProcedureKind::Mutation));
    assert_eq!(d.resources["rpc:todos.add"].idempotent, Some(true));
    assert_eq!(d.resources["/api"].auth, Some(RequiredPrincipal::User));
}

#[test]
fn manifest_validate_rejects_non_hex_sourcemap_key() {
    let m = Manifest {
        sourcemaps: HashMap::from([("NOT_HEX".to_string(), SHA_A.to_string())]),
        ..Manifest::default()
    };
    assert!(m.validate().is_err(), "non-hex key must be rejected");

    // Uppercase is also rejected.
    let upper = "A".repeat(64);
    let m2 = Manifest {
        sourcemaps: HashMap::from([(upper, SHA_A.to_string())]),
        ..Manifest::default()
    };
    assert!(m2.validate().is_err(), "uppercase hex must be rejected");

    // Wrong length.
    let m3 = Manifest {
        sourcemaps: HashMap::from([("abcd".to_string(), SHA_A.to_string())]),
        ..Manifest::default()
    };
    assert!(m3.validate().is_err(), "length != 64 must be rejected");

    // Non-hex value.
    let m4 = Manifest {
        sourcemaps: HashMap::from([(SHA_A.to_string(), "not_a_hash".to_string())]),
        ..Manifest::default()
    };
    assert!(m4.validate().is_err(), "non-hex value must be rejected");
}

#[test]
fn manifest_validate_rejects_entry_not_in_modules() {
    // The flat WorkerCode shape requires `entry` to be a key of `modules`.
    let m = Manifest {
        worker: Some(WorkerCode {
            entry: "src/a.js".into(),
            modules: HashMap::from([("src/b.js".to_string(), SHA_A.to_string())]),
        }),
        ..Manifest::default()
    };
    let err = m.validate().unwrap_err();
    assert!(
        err.contains("entry") && err.contains("src/a.js"),
        "error mentions missing entry: {err}"
    );
}

#[test]
fn manifest_validate_rejects_bad_worker_hash() {
    let m = Manifest {
        worker: Some(WorkerCode {
            entry: "index.js".into(),
            modules: HashMap::from([("index.js".to_string(), "not-hex".to_string())]),
        }),
        ..Manifest::default()
    };
    let err = m.validate().unwrap_err();
    assert!(err.contains("worker"), "error mentions worker: {err}");
}

#[test]
fn worker_round_trips_through_json() {
    // Flat shape — no `kind` discriminator.
    let w = WorkerCode {
        entry: "src/index.js".into(),
        modules: HashMap::from([
            ("src/index.js".to_string(), SHA_A.to_string()),
            ("src/lib.js".to_string(), SHA_B.to_string()),
        ]),
    };
    let json = serde_json::to_string(&w).unwrap();
    assert!(!json.contains("\"kind\""), "no discriminator in flat shape: {json}");
    assert!(json.contains("src/index.js"), "entry present: {json}");
    let decoded: WorkerCode = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, w);
}

// -- AppVersionInfo wire format -------------------------------------------

#[test]
fn app_version_info_serializes_with_manifest() {
    let info = AppVersionInfo {
        deploy_hash: Some(SHA_A.to_string()),
        plan_id: "pro".into(),
        runtime: AppRuntimeLimits::default(),
        env_version: 7,
        manifest: Some(Manifest {
            worker: Some(WorkerCode {
                entry: "index.js".into(),
                modules: HashMap::from([("index.js".to_string(), SHA_B.to_string())]),
            }),
            ..Manifest::default()
        }),
        net_policy: AppNetPolicy {
            egress: vec![
                NetEgressEntry {
                    verdict: Verdict::Accept,
                    destination: "db.example.com".into(),
                    port: 5432,
                },
                NetEgressEntry {
                    verdict: Verdict::Reject,
                    destination: "93.184.216.0/24".into(),
                    port: 5432,
                },
            ],
            max_sockets: 4,
            egress_ceiling_bytes: 1024 * 1024,
        },
    };
    let json = serde_json::to_string(&info).unwrap();
    assert!(json.contains("\"manifest\""), "manifest is on the wire: {json}");
    assert!(json.contains(SHA_B), "worker module hash present: {json}");
    assert!(json.contains("\"net_policy\""), "net policy is on the wire: {json}");
    // The verdict must survive the wire. A rule that arrives without one is a
    // rule whose meaning depends on which side of a client upgrade you are on,
    // and ACCEPT is the wrong thing to guess. BOTH spellings are asserted: one
    // alone passes against a serializer that emits a constant.
    assert!(
        json.contains("\"verdict\":\"accept\"") && json.contains("\"verdict\":\"reject\""),
        "worker-facing net policy carries each rule's verdict: {json}"
    );

    let decoded: AppVersionInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.env_version, 7);
    assert_eq!(decoded.deploy_hash.as_deref(), Some(SHA_A));
    let worker = decoded.manifest.unwrap().worker.unwrap();
    assert_eq!(worker.entry, "index.js");
    assert_eq!(worker.modules.get("index.js").map(String::as_str), Some(SHA_B));
    assert_eq!(decoded.net_policy.egress[0].destination, "db.example.com");
    assert_eq!(decoded.net_policy.egress[0].verdict, Verdict::Accept);
    assert_eq!(decoded.net_policy.egress[1].destination, "93.184.216.0/24");
    assert_eq!(decoded.net_policy.egress[1].verdict, Verdict::Reject);
}

#[test]
fn app_version_info_omits_missing_manifest() {
    let info = AppVersionInfo {
        deploy_hash: None,
        plan_id: "free".into(),
        runtime: AppRuntimeLimits::default(),
        env_version: 0,
        manifest: None,
        net_policy: AppNetPolicy::default(),
    };
    let json = serde_json::to_string(&info).unwrap();
    assert!(
        !json.contains("\"manifest\""),
        "manifest absent when None: {json}"
    );
    let decoded: AppVersionInfo = serde_json::from_str(&json).unwrap();
    assert!(decoded.manifest.is_none());
}

#[test]
fn app_version_info_accepts_legacy_payload_without_manifest() {
    // Deserialising a payload from a control plane that doesn't yet
    // emit `manifest` must succeed — `manifest` is `#[serde(default)]`.
    let json = r#"{
        "deploy_hash": null,
        "plan_id": "free",
        "runtime": {},
        "env_version": 3
    }"#;
    let info: AppVersionInfo = serde_json::from_str(json).unwrap();
    assert!(info.manifest.is_none());
    assert_eq!(info.env_version, 3);
    assert_eq!(info.net_policy, AppNetPolicy::default());
}

// -- AssetEntry variants (Tier 4b: pre-compressed encoding variants) -----

const SHA_BR: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const SHA_GZ: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

fn entry_with_variants(variants: HashMap<String, AssetVariant>) -> AssetEntry {
    AssetEntry {
        hash: SHA_A.into(),
        content_type: "text/javascript".into(),
        size: 1024,
        cache: None,
        updated_at: 0,
        variants,
    }
}

#[test]
fn asset_variants_round_trip() {
    let mut variants = HashMap::new();
    variants.insert(
        "br".into(),
        AssetVariant { hash: SHA_BR.into(), size: 256 },
    );
    variants.insert(
        "gzip".into(),
        AssetVariant { hash: SHA_GZ.into(), size: 384 },
    );
    let entry = entry_with_variants(variants);

    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains("\"variants\""), "variants serialised: {json}");
    assert!(json.contains(SHA_BR), "br hash present: {json}");
    assert!(json.contains(SHA_GZ), "gzip hash present: {json}");

    let decoded: AssetEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.variants.len(), 2);
    assert_eq!(decoded.variants["br"].hash, SHA_BR);
    assert_eq!(decoded.variants["br"].size, 256);
    assert_eq!(decoded.variants["gzip"].hash, SHA_GZ);
    assert_eq!(decoded.variants["gzip"].size, 384);
}

#[test]
fn asset_variants_empty_omitted_from_json() {
    let entry = entry_with_variants(HashMap::new());
    let json = serde_json::to_string(&entry).unwrap();
    assert!(!json.contains("\"variants\""), "empty variants omitted: {json}");

    // Backwards compat: payloads without `variants` deserialize cleanly
    // with the field defaulting to an empty map.
    let legacy = format!(
        r#"{{"hash":"{}","content_type":"text/html","size":42,"updated_at":0}}"#,
        SHA_A
    );
    let decoded: AssetEntry = serde_json::from_str(&legacy).unwrap();
    assert!(decoded.variants.is_empty());
}

#[test]
fn asset_variants_unknown_encoding_rejected() {
    let mut variants = HashMap::new();
    variants.insert(
        "lz4".into(),
        AssetVariant { hash: SHA_BR.into(), size: 100 },
    );
    let m = Manifest {
        assets: HashMap::from([("/main.js".into(), entry_with_variants(variants))]),
        ..Manifest::default()
    };
    let err = m.validate().unwrap_err();
    assert!(
        err.to_lowercase().contains("encoding") || err.contains("lz4"),
        "error mentions encoding/lz4: {err}"
    );
}

#[test]
fn asset_variants_bad_hash_rejected() {
    let mut variants = HashMap::new();
    variants.insert(
        "br".into(),
        AssetVariant {
            hash: "not-a-real-hash".into(),
            size: 100,
        },
    );
    let m = Manifest {
        assets: HashMap::from([("/main.js".into(), entry_with_variants(variants))]),
        ..Manifest::default()
    };
    let err = m.validate().unwrap_err();
    assert!(
        err.contains("hash") || err.contains("hex"),
        "error mentions hash format: {err}"
    );

    let mut variants2 = HashMap::new();
    variants2.insert(
        "br".into(),
        AssetVariant { hash: "A".repeat(64), size: 100 },
    );
    let m2 = Manifest {
        assets: HashMap::from([("/main.js".into(), entry_with_variants(variants2))]),
        ..Manifest::default()
    };
    assert!(m2.validate().is_err(), "uppercase variant hash rejected");
}

#[test]
fn asset_variants_runtime_assets_validated_too() {
    let mut variants = HashMap::new();
    variants.insert(
        "deflate".into(),
        AssetVariant { hash: SHA_BR.into(), size: 100 },
    );
    let m = Manifest {
        runtime_assets: HashMap::from([("/dyn.js".into(), entry_with_variants(variants))]),
        ..Manifest::default()
    };
    assert!(m.validate().is_err(), "deflate is not in the v1 allow list");
}

// ---------------------------------------------------------------------------
// Resource-tree validation (rpc-v2 §7).
// ---------------------------------------------------------------------------

fn schema_ref() -> String {
    format!("sha256:{SHA64}")
}

fn schema_value() -> serde_json::Value {
    serde_json::json!({"type": "object"})
}

#[test]
fn validate_rejects_malformed_resource_key() {
    let mut resources = HashMap::new();
    resources.insert("invalid-key".to_string(), ResourceEntry::default());
    let m = Manifest {
        resources,
        ..Manifest::default()
    };
    let err = m.validate().unwrap_err();
    assert!(err.contains("malformed"), "{err}");
}

#[test]
fn validate_accepts_root_url_and_rpc_keys() {
    let mut resources = HashMap::new();
    resources.insert(
        "*".into(),
        ResourceEntry {
            auth: Some(RequiredPrincipal::Anonymous),
            publicly_accessible: Some(true),
            ..Default::default()
        },
    );
    resources.insert("/api".into(), ResourceEntry::default());
    resources.insert("rpc:todos".into(), ResourceEntry::default());
    let m = Manifest {
        resources,
        ..Manifest::default()
    };
    m.validate().expect("well-formed keys must validate");
}

#[test]
fn validate_rejects_multiple_routing_actions() {
    let mut resources = HashMap::new();
    resources.insert(
        "/foo".into(),
        ResourceEntry {
            redirect: Some(RedirectAction { to: "/bar".into(), status: 302 }),
            rewrite: Some("/baz".into()),
            ..Default::default()
        },
    );
    let m = Manifest {
        resources,
        ..Manifest::default()
    };
    let err = m.validate().unwrap_err();
    assert!(err.contains("at most one"), "{err}");
}

#[test]
fn validate_rejects_anonymous_without_publicly_accessible() {
    let mut resources = HashMap::new();
    resources.insert(
        "/api/public".into(),
        ResourceEntry {
            auth: Some(RequiredPrincipal::Anonymous),
            ..Default::default()
        },
    );
    let m = Manifest {
        resources,
        ..Manifest::default()
    };
    let err = m.validate().unwrap_err();
    assert!(err.contains("publicly_accessible"), "{err}");
}

#[test]
fn validate_anonymous_with_publicly_accessible_passes() {
    let mut resources = HashMap::new();
    resources.insert(
        "/api/public".into(),
        ResourceEntry {
            auth: Some(RequiredPrincipal::Anonymous),
            publicly_accessible: Some(true),
            ..Default::default()
        },
    );
    let m = Manifest {
        resources,
        ..Manifest::default()
    };
    m.validate().expect("anonymous + publicly_accessible is the secure-by-default opt-in");
}

#[test]
fn validate_redirect_status_must_be_3xx() {
    let mut resources = HashMap::new();
    resources.insert(
        "/old".into(),
        ResourceEntry {
            redirect: Some(RedirectAction { to: "/new".into(), status: 200 }),
            ..Default::default()
        },
    );
    let m = Manifest {
        resources,
        ..Manifest::default()
    };
    let err = m.validate().unwrap_err();
    assert!(err.contains("3xx"), "{err}");
}

#[test]
fn validate_schema_hash_format_is_enforced() {
    let mut resources = HashMap::new();
    resources.insert(
        "rpc:todos.list".into(),
        ResourceEntry {
            kind: Some(ProcedureKind::Query),
            input_schema: Some("not-a-hash".into()),
            ..Default::default()
        },
    );
    let m = Manifest {
        resources,
        ..Manifest::default()
    };
    let err = m.validate().unwrap_err();
    assert!(err.contains("sha256"), "{err}");
}

#[test]
fn validate_schema_must_exist_in_schemas_map() {
    let mut resources = HashMap::new();
    resources.insert(
        "rpc:todos.list".into(),
        ResourceEntry {
            kind: Some(ProcedureKind::Query),
            input_schema: Some(schema_ref()),
            ..Default::default()
        },
    );
    let m = Manifest {
        resources,
        // Note: no `schemas` entry for the referenced hash.
        ..Manifest::default()
    };
    let err = m.validate().unwrap_err();
    assert!(err.contains("not present"), "{err}");
}

#[test]
fn validate_schema_present_in_schemas_map_passes() {
    let mut resources = HashMap::new();
    resources.insert(
        "rpc:todos.list".into(),
        ResourceEntry {
            kind: Some(ProcedureKind::Query),
            input_schema: Some(schema_ref()),
            ..Default::default()
        },
    );
    let mut schemas = HashMap::new();
    schemas.insert(schema_ref(), schema_value());
    let m = Manifest {
        resources,
        schemas,
        ..Manifest::default()
    };
    m.validate().expect("schema present in schemas map must validate");
}

#[test]
fn validate_override_marker_required_for_inherited_field() {
    let mut resources = HashMap::new();
    resources.insert(
        "rpc:todos".into(),
        ResourceEntry {
            auth: Some(RequiredPrincipal::User),
            ..Default::default()
        },
    );
    // Child redeclares `auth` without listing it in `override`.
    resources.insert(
        "rpc:todos.delete".into(),
        ResourceEntry {
            auth: Some(RequiredPrincipal::Anonymous),
            publicly_accessible: Some(true),
            kind: Some(ProcedureKind::Mutation),
            ..Default::default()
        },
    );
    let m = Manifest {
        resources,
        ..Manifest::default()
    };
    let err = m.validate().unwrap_err();
    assert!(err.contains("override"), "{err}");
}

#[test]
fn validate_override_marker_satisfies_check() {
    let mut resources = HashMap::new();
    resources.insert(
        "rpc:todos".into(),
        ResourceEntry {
            auth: Some(RequiredPrincipal::User),
            ..Default::default()
        },
    );
    resources.insert(
        "rpc:todos.delete".into(),
        ResourceEntry {
            auth: Some(RequiredPrincipal::Anonymous),
            publicly_accessible: Some(true),
            kind: Some(ProcedureKind::Mutation),
            r#override: vec!["auth".into()],
            ..Default::default()
        },
    );
    let m = Manifest {
        resources,
        ..Manifest::default()
    };
    m.validate().expect("explicit override allows the redeclaration");
}

#[test]
fn validate_url_inheritance_chain_is_segment_aware() {
    let mut resources = HashMap::new();
    resources.insert(
        "/api".into(),
        ResourceEntry {
            auth: Some(RequiredPrincipal::User),
            ..Default::default()
        },
    );
    // /api/admin/users redeclares `auth` shadowed by /api → must list override.
    resources.insert(
        "/api/admin/users".into(),
        ResourceEntry {
            auth: Some(RequiredPrincipal::Anonymous),
            publicly_accessible: Some(true),
            ..Default::default()
        },
    );
    let m = Manifest {
        resources,
        ..Manifest::default()
    };
    let err = m
        .validate()
        .expect_err("inherits auth from /api ancestor without override");
    assert!(err.contains("override"), "{err}");
}

#[test]
fn redirect_action_status_defaults_to_302() {
    let json = r#"{"to":"/bar"}"#;
    let r: RedirectAction = serde_json::from_str(json).unwrap();
    assert_eq!(r.status, 302);
}

#[test]
fn static_action_round_trips() {
    let s = StaticAction { r#try: vec!["$path".into(), "/index.html".into()] };
    let json = serde_json::to_string(&s).unwrap();
    let d: StaticAction = serde_json::from_str(&json).unwrap();
    assert_eq!(d.r#try, vec!["$path".to_string(), "/index.html".to_string()]);
}

/// The wire spelling of `RequiredPrincipal` is the WHOLE creator contract:
/// the vite plugin emits these strings and the gateway compiles them into the
/// access decision. `"anonymous"` is spelled out; the abbreviation `"anon"` is
/// NOT a second accepted spelling, and neither is the deleted `"admin"`.
///
/// This replaces `auth_level_rank_ordering`, which asserted a three-level
/// strictness ladder. `rank()` is gone with the third level: two variants make
/// the merge a boolean OR, which the gateway proves in
/// `compiled::tests::effective_policy_stricter_auth_wins_along_chain`.
#[test]
fn required_principal_wire_spelling_is_exactly_two_values() {
    assert_eq!(
        serde_json::to_string(&RequiredPrincipal::Anonymous).unwrap(),
        "\"anonymous\"",
        "the public wire value is spelled out, never abbreviated"
    );
    assert_eq!(
        serde_json::to_string(&RequiredPrincipal::User).unwrap(),
        "\"user\""
    );
    for spelling in ["\"anon\"", "\"admin\"", "\"Anonymous\"", "\"\""] {
        assert!(
            serde_json::from_str::<RequiredPrincipal>(spelling).is_err(),
            "{spelling} must not deserialise: there is no alias and no admin level"
        );
    }
    let anonymous: RequiredPrincipal = serde_json::from_str("\"anonymous\"").unwrap();
    assert_eq!(anonymous, RequiredPrincipal::Anonymous);
    let user: RequiredPrincipal = serde_json::from_str("\"user\"").unwrap();
    assert_eq!(user, RequiredPrincipal::User);
}

#[test]
fn manifest_does_not_serialize_rules_field() {
    // The `rules: Vec<Rule>` field is gone. The wire shape carries
    // `resources` only.
    let m = Manifest::default();
    let json = serde_json::to_string(&m).unwrap();
    assert!(
        !json.contains("\"rules\""),
        "manifest must not serialize a `rules` field: {json}"
    );

    let mut resources = HashMap::new();
    resources.insert(
        "*".into(),
        ResourceEntry {
            auth: Some(RequiredPrincipal::User),
            ..Default::default()
        },
    );
    let m2 = Manifest {
        resources,
        ..Manifest::default()
    };
    let json2 = serde_json::to_string(&m2).unwrap();
    assert!(
        !json2.contains("\"rules\""),
        "manifest with resources must not serialize `rules` either: {json2}"
    );
}

// Suppress unused-import warning when the corresponding test references
// drop out; keep them imported for any future test additions that
// exercise the type definitions directly.
#[allow(dead_code)]
fn _force_imports_used() {
    let _ = ManifestMetadata::default();
}
