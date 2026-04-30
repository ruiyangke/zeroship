use zeroship_core::types::{
    Action, AppRuntimeLimits, AppUsage, AppVersionInfo, AssetEntry, ControlEvent, Cors, HttpMethod,
    Manifest, ManifestMetadata, Match, RouteEntry, Rule, UsageReport, WorkerCode, WorkerMode,
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
fn usage_report_roundtrip() {
    let app_id = Uuid::new_v4();

    let mut counters: HashMap<Uuid, AppUsage> = HashMap::new();
    counters.insert(app_id, AppUsage {
        requests: 42,
        cpu_us: 1000,
        wall_us: 2000,
        egress_bytes: 512,
        ingress_bytes: 256,
    });

    let report = UsageReport {
        worker_id: "w1".to_string(),
        counters,
    };
    let json = serde_json::to_string(&report).unwrap();
    let decoded: UsageReport = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.worker_id, "w1");
    let usage = decoded.counters.get(&app_id).unwrap();
    assert_eq!(usage.requests, 42);
    assert_eq!(usage.cpu_us, 1000);
}

#[test]
fn route_entry_roundtrip() {
    let entry = RouteEntry {
        name: "my-app".to_string(),
        plan_id: "pro".to_string(),
        api_key_hash: "deadbeef".repeat(8),
        deploy_hash: Some("abc123".to_string()),
        manifest: Manifest::passthrough(),
    };

    let json = serde_json::to_string(&entry).unwrap();
    let decoded: RouteEntry = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.name, "my-app");
    assert_eq!(decoded.plan_id, "pro");
    assert_eq!(decoded.deploy_hash, Some("abc123".to_string()));
    // Every route has a manifest after deserialization.
    assert!(!decoded.manifest.rules.is_empty(), "passthrough has rules");
}

#[test]
fn route_entry_missing_manifest_field_synthesizes_passthrough() {
    // Older rows (or rows produced before the manifest field) must still
    // deserialize cleanly — the missing field defaults to passthrough.
    let json = r#"{
        "name": "legacy-app",
        "plan_id": "free",
        "api_key_hash": "h",
        "deploy_hash": null
    }"#;
    let decoded: RouteEntry = serde_json::from_str(json).unwrap();
    assert!(!decoded.manifest.rules.is_empty(), "default to passthrough");
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

// -- shadow / unreachable rule detection ------------------------------------

#[test]
fn validate_detects_any_shadowing_subsequent_rule() {
    let m = Manifest {
        rules: vec![
            Rule {
                r#match: Match::Any,
                action: Action::Worker {
                    mode: WorkerMode::Ssr,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
            Rule {
                r#match: Match::Exact { method: None, path: "/foo".into() },
                action: Action::Worker {
                    mode: WorkerMode::Ssr,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
        ],
        ..Manifest::default()
    };
    assert!(m.validate().is_err(), "rule below Any → Worker(all methods) is unreachable");
}

#[test]
fn validate_allows_any_at_end() {
    let m = Manifest {
        rules: vec![
            Rule {
                r#match: Match::Exact { method: None, path: "/foo".into() },
                action: Action::Worker {
                    mode: WorkerMode::Ssr,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
            Rule {
                r#match: Match::Any,
                action: Action::Worker {
                    mode: WorkerMode::Ssr,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
        ],
        ..Manifest::default()
    };
    assert!(m.validate().is_ok(), "Any at end is the canonical catch-all");
}

#[test]
fn validate_detects_prefix_shadowing_exact() {
    let m = Manifest {
        rules: vec![
            Rule {
                r#match: Match::Prefix { method: None, path: "/admin".into() },
                action: Action::Worker {
                    mode: WorkerMode::Ssr,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
            Rule {
                r#match: Match::Exact { method: None, path: "/admin/users".into() },
                action: Action::Worker {
                    mode: WorkerMode::Ssr,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
        ],
        ..Manifest::default()
    };
    assert!(m.validate().is_err(), "/admin/users sits under /admin prefix");
}

#[test]
fn validate_detects_prefix_shadowing_longer_prefix() {
    let m = Manifest {
        rules: vec![
            Rule {
                r#match: Match::Prefix { method: None, path: "/admin".into() },
                action: Action::Worker {
                    mode: WorkerMode::Ssr,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
            Rule {
                r#match: Match::Prefix { method: None, path: "/admin/users".into() },
                action: Action::Worker {
                    mode: WorkerMode::Ssr,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
        ],
        ..Manifest::default()
    };
    assert!(m.validate().is_err(), "/admin/users prefix is contained in /admin prefix");
}

#[test]
fn validate_allows_method_disjoint_rules() {
    let m = Manifest {
        rules: vec![
            Rule {
                r#match: Match::Exact {
                    method: Some(HttpMethod::Get),
                    path: "/a".into(),
                },
                action: Action::Worker {
                    mode: WorkerMode::Ssr,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
            Rule {
                r#match: Match::Exact {
                    method: Some(HttpMethod::Post),
                    path: "/a".into(),
                },
                action: Action::Worker {
                    mode: WorkerMode::Rpc,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
        ],
        ..Manifest::default()
    };
    assert!(m.validate().is_ok(), "GET and POST on same path don't shadow each other");
}

#[test]
fn validate_static_does_not_shadow_non_get_head_rule() {
    // Tier 1 invariant: Static actions only fire for GET/HEAD; a POST
    // rule below `Any → Static` is reachable, not shadowed.
    let m = Manifest {
        rules: vec![
            Rule {
                r#match: Match::Any,
                action: Action::Static {
                    r#try: vec!["/index.html".into()],
                    cache: None,
                    status: None,
                },
                cors: None,
            },
            Rule {
                r#match: Match::Prefix {
                    method: Some(HttpMethod::Post),
                    path: "/api/".into(),
                },
                action: Action::Worker {
                    mode: WorkerMode::Ssr,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
        ],
        ..Manifest::default()
    };
    assert!(m.validate().is_ok(), "Static narrows to GET/HEAD; POST rule is reachable");
}

#[test]
fn validate_detects_duplicate_exact_rules() {
    let m = Manifest {
        rules: vec![
            Rule {
                r#match: Match::Exact { method: None, path: "/a".into() },
                action: Action::Worker {
                    mode: WorkerMode::Ssr,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
            Rule {
                r#match: Match::Exact { method: None, path: "/a".into() },
                action: Action::Worker {
                    mode: WorkerMode::Rpc,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
        ],
        ..Manifest::default()
    };
    assert!(m.validate().is_err(), "duplicate Exact /a is dead second time");
}

#[test]
fn validate_passthrough_is_valid() {
    // The synthesized default for apps without their own manifest must
    // pass shadow detection; if it didn't, every legacy app would refuse
    // to load.
    assert!(Manifest::passthrough().validate().is_ok());
}

#[test]
fn manifest_validate_rejects_invalid_status() {
    let bad = Manifest {
        rules: vec![Rule {
            r#match: Match::Any,
            action: Action::Static {
                r#try: vec!["/x.html".into()],
                cache: None,
                status: Some(99), // out of range
            },
            cors: None,
        }],
        ..Manifest::default()
    };
    assert!(bad.validate().is_err(), "status=99 must be rejected");

    let good = Manifest {
        rules: vec![Rule {
            r#match: Match::Any,
            action: Action::Static {
                r#try: vec!["/x.html".into()],
                cache: None,
                status: Some(404),
            },
            cors: None,
        }],
        ..Manifest::default()
    };
    assert!(good.validate().is_ok(), "status=404 must be accepted");

    let unset = Manifest::default();
    assert!(unset.validate().is_ok(), "no status is fine");
}

#[test]
fn match_prefix_segment_boundary() {
    // /admin must match exactly, with trailing slash, or with a sub-path —
    // but NOT span across a segment boundary (no /administrator match).
    let m = Match::Prefix { method: None, path: "/admin".into() };
    assert!(m.test("GET", "/admin").is_some(), "exact /admin");
    assert!(m.test("GET", "/admin/").is_some(), "trailing slash");
    assert!(m.test("GET", "/admin/users").is_some(), "sub-path");
    assert!(m.test("GET", "/administrator").is_none(), "must not span segment");
    assert!(m.test("GET", "/admin-panel").is_none(), "must not span segment");
}

#[test]
fn match_glob_single() {
    let m = Match::Glob { method: None, path: "/blog/[slug]".into() };
    let caps = m.test("GET", "/blog/hello").unwrap();
    assert_eq!(caps.get("slug").unwrap(), "hello");
    assert!(m.test("GET", "/blog/").is_none());
    assert!(m.test("GET", "/blog/hello/extra").is_none());
}

#[test]
fn match_glob_catchall() {
    let m = Match::Glob { method: None, path: "/api/[...rest]".into() };
    let caps = m.test("GET", "/api/v1/users/42").unwrap();
    assert_eq!(caps.get("rest").unwrap(), "v1/users/42");
}

#[test]
fn match_any() {
    let m = Match::Any;
    assert!(m.test("GET", "/anything").is_some());
    assert!(m.test("DELETE", "/random/path").is_some());
}

#[test]
fn manifest_roundtrip_json() {
    let m = Manifest {
        rules: vec![
            Rule {
                r#match: Match::Prefix {
                    method: Some(HttpMethod::Post),
                    path: "/_rpc/".into(),
                },
                action: Action::Worker {
                    mode: WorkerMode::Rpc,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
            Rule {
                r#match: Match::Glob {
                    method: None,
                    path: "/blog/[slug]".into(),
                },
                action: Action::Worker {
                    mode: WorkerMode::Ssr,
                    cache: None,
                    rate_limit: None,
                },
                cors: None,
            },
            Rule {
                r#match: Match::Any,
                action: Action::Static {
                    r#try: vec!["$path".into(), "/index.html".into()],
                    cache: None,
                    status: None,
                },
                cors: None,
            },
        ],
        assets: HashMap::from([(
            "/index.html".to_string(),
            AssetEntry {
                hash: SHA_A.into(),
                content_type: "text/html".into(),
                size: 1024,
                cache: None,
                updated_at: 0,
            },
        )]),
        worker: Some(WorkerCode {
            entry: "index.js".into(),
            modules: HashMap::from([("index.js".to_string(), SHA_B.to_string())]),
        }),
        ..Manifest::default()
    };

    let json = serde_json::to_string(&m).unwrap();
    let decoded: Manifest = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.rules.len(), 3);
    assert_eq!(
        decoded.worker,
        Some(WorkerCode {
            entry: "index.js".into(),
            modules: HashMap::from([("index.js".to_string(), SHA_B.to_string())]),
        })
    );
    assert_eq!(decoded.assets["/index.html"].hash, SHA_A);
}

// -- v2 schema -------------------------------------------------------------

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[test]
fn manifest_v2_round_trips_through_json() {
    let m = Manifest {
        version: 2,
        deploy_hash: Some(SHA_A.to_string()),
        worker: Some(WorkerCode {
            entry: "index.js".into(),
            modules: HashMap::from([("index.js".to_string(), SHA_B.to_string())]),
        }),
        rules: vec![Rule {
            r#match: Match::Any,
            action: Action::Worker {
                mode: WorkerMode::Ssr,
                cache: None,
                rate_limit: None,
            },
            cors: None,
        }],
        assets: HashMap::from([(
            "/about.html".to_string(),
            AssetEntry {
                hash: SHA_A.into(),
                content_type: "text/html".into(),
                size: 42,
                cache: None,
                updated_at: 0,
            },
        )]),
        runtime_assets: HashMap::new(),
        asset_version: 0,
        sourcemaps: HashMap::from([(SHA_A.to_string(), SHA_B.to_string())]),
        metadata: ManifestMetadata {
            compiler: Some("@zeroship/vite-plugin@0.1".into()),
            built_at: "2026-04-29T12:34:56Z".into(),
        },
    };

    let json = serde_json::to_string(&m).unwrap();
    let d: Manifest = serde_json::from_str(&json).unwrap();
    assert_eq!(d.version, 2);
    assert_eq!(d.deploy_hash.as_deref(), Some(SHA_A));
    assert_eq!(
        d.worker,
        Some(WorkerCode {
            entry: "index.js".into(),
            modules: HashMap::from([("index.js".to_string(), SHA_B.to_string())]),
        })
    );
    assert_eq!(d.rules.len(), 1);
    assert_eq!(d.assets["/about.html"].hash, SHA_A);
    assert_eq!(d.sourcemaps.get(SHA_A).map(String::as_str), Some(SHA_B));
    assert_eq!(d.metadata.compiler.as_deref(), Some("@zeroship/vite-plugin@0.1"));
    assert_eq!(d.metadata.built_at, "2026-04-29T12:34:56Z");
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
fn manifest_validate_rejects_unsupported_version() {
    let m = Manifest { version: 99, ..Manifest::default() };
    let err = m.validate().unwrap_err();
    assert!(err.contains("99"), "error mentions bad version: {err}");
}

#[test]
fn manifest_passthrough_v2_validates() {
    let p = Manifest::passthrough();
    assert_eq!(p.version, 2);
    assert!(p.worker.is_none(), "passthrough has no worker");
    assert_eq!(p.metadata.built_at, "1970-01-01T00:00:00Z");
    assert!(
        p.metadata.compiler
            .as_deref()
            .is_some_and(|s| s.starts_with("zeroship-passthrough@")),
        "passthrough compiler tag should be set: {:?}",
        p.metadata.compiler,
    );
    p.validate().expect("passthrough must validate");
}

#[test]
fn manifest_v2_minimal_round_trips() {
    // Minimal v2 wire format: omit sourcemaps/metadata; the serde
    // defaults fill them in. version defaults to 2.
    let json = r#"{
        "rules": [],
        "assets": {},
        "runtime_assets": {},
        "asset_version": 0
    }"#;
    let m: Manifest = serde_json::from_str(json).unwrap();
    assert_eq!(m.version, 2);
    assert!(m.assets.is_empty());
    assert!(m.sourcemaps.is_empty());
    assert!(m.worker.is_none());
    assert_eq!(m.metadata.built_at, "");
    m.validate().expect("minimal v2 manifest must validate");
}

#[test]
fn manifest_validate_rejects_v1() {
    // Hard cut: v1 is no longer accepted. Production data is v2 only.
    let m = Manifest { version: 1, ..Manifest::default() };
    let err = m.validate().unwrap_err();
    assert!(err.contains("1"), "error mentions bad version: {err}");
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
    assert!(
        err.contains("worker"),
        "error mentions worker: {err}"
    );
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
    // Phase 4b: workers get the manifest inline on every /internal/versions
    // poll so they can resolve worker.modules[entry] without an extra hop.
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
    };
    let json = serde_json::to_string(&info).unwrap();
    assert!(json.contains("\"manifest\""), "manifest is on the wire: {json}");
    assert!(json.contains(SHA_B), "worker module hash present: {json}");

    let decoded: AppVersionInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.env_version, 7);
    assert_eq!(decoded.deploy_hash.as_deref(), Some(SHA_A));
    let worker = decoded.manifest.unwrap().worker.unwrap();
    assert_eq!(worker.entry, "index.js");
    assert_eq!(worker.modules.get("index.js").map(String::as_str), Some(SHA_B));
}

#[test]
fn app_version_info_omits_missing_manifest() {
    // Apps that have not deployed yet have no manifest. The field uses
    // `skip_serializing_if = Option::is_none`, so the wire payload stays
    // compact and forward-compatible.
    let info = AppVersionInfo {
        deploy_hash: None,
        plan_id: "free".into(),
        runtime: AppRuntimeLimits::default(),
        env_version: 0,
        manifest: None,
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
}

// -- CORS schema -----------------------------------------------------------

fn cors_rule_with(cors: Cors) -> Rule {
    Rule {
        r#match: Match::Prefix {
            method: None,
            path: "/api/".into(),
        },
        action: Action::Worker {
            mode: WorkerMode::Ssr,
            cache: None,
            rate_limit: None,
        },
        cors: Some(cors),
    }
}

#[test]
fn cors_round_trips_through_json() {
    let cors = Cors {
        allow_origins: vec!["https://example.com".into(), "https://app.example.com".into()],
        allow_methods: vec![HttpMethod::Get, HttpMethod::Post, HttpMethod::Options],
        allow_headers: vec!["content-type".into(), "authorization".into()],
        expose_headers: vec!["x-request-id".into()],
        allow_credentials: true,
        max_age_seconds: Some(600),
    };
    let m = Manifest {
        rules: vec![cors_rule_with(cors.clone())],
        ..Manifest::default()
    };
    let json = serde_json::to_string(&m).unwrap();
    let decoded: Manifest = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.rules.len(), 1);
    assert_eq!(decoded.rules[0].cors.as_ref(), Some(&cors));
}

#[test]
fn cors_credentials_with_wildcard_rejected() {
    // Per the browser CORS spec, allow_credentials=true MUST NOT pair
    // with allow_origins=["*"]. validate() must catch this.
    let m = Manifest {
        rules: vec![cors_rule_with(Cors {
            allow_origins: vec!["*".into()],
            allow_methods: vec![HttpMethod::Get],
            allow_headers: vec![],
            expose_headers: vec![],
            allow_credentials: true,
            max_age_seconds: None,
        })],
        ..Manifest::default()
    };
    let err = m.validate().unwrap_err();
    assert!(
        err.to_lowercase().contains("credentials") || err.contains("*"),
        "error must mention the credentials/* combination: {err}"
    );
}

#[test]
fn cors_empty_allow_origins_rejected() {
    let m = Manifest {
        rules: vec![cors_rule_with(Cors {
            allow_origins: vec![],
            allow_methods: vec![HttpMethod::Get],
            allow_headers: vec![],
            expose_headers: vec![],
            allow_credentials: false,
            max_age_seconds: None,
        })],
        ..Manifest::default()
    };
    let err = m.validate().unwrap_err();
    assert!(
        err.to_lowercase().contains("allow_origins"),
        "error must mention allow_origins: {err}"
    );
}

#[test]
fn cors_omitted_round_trips() {
    // A rule without `cors` must serialize without the field present
    // and deserialize as cors: None, preserving back-compat with
    // existing manifests.
    let m = Manifest {
        rules: vec![Rule {
            r#match: Match::Any,
            action: Action::Worker {
                mode: WorkerMode::Ssr,
                cache: None,
                rate_limit: None,
            },
            cors: None,
        }],
        ..Manifest::default()
    };
    let json = serde_json::to_string(&m).unwrap();
    assert!(!json.contains("\"cors\""), "no cors field when None: {json}");
    let decoded: Manifest = serde_json::from_str(&json).unwrap();
    assert!(decoded.rules[0].cors.is_none());

    // Older manifest payloads (no cors field at all) still parse.
    let legacy = r#"{
        "rules": [
            {"match": {"kind": "any"},
             "action": {"kind": "worker", "mode": "ssr"}}
        ],
        "assets": {},
        "runtime_assets": {},
        "asset_version": 0
    }"#;
    let m2: Manifest = serde_json::from_str(legacy).unwrap();
    assert!(m2.rules[0].cors.is_none());
}

#[test]
fn cors_credentials_with_specific_origin_validates() {
    let m = Manifest {
        rules: vec![cors_rule_with(Cors {
            allow_origins: vec!["https://example.com".into()],
            allow_methods: vec![HttpMethod::Post],
            allow_headers: vec!["content-type".into()],
            expose_headers: vec![],
            allow_credentials: true,
            max_age_seconds: Some(86_400),
        })],
        ..Manifest::default()
    };
    m.validate().expect("credentials with specific origin must validate");
}
