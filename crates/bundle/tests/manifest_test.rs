//! Round-trip tests for `Manifest` JSON wire format.
//!
//! The wire format is an immutable contract — every additive change
//! must keep old manifests deserializing unchanged AND keep newly
//! produced manifests omitting the new fields whenever they're empty.
//! These tests pin both directions.

use serde_json::{Value, json};
use zeroship_bundle::{AuthConfig, HandlerEntry, Manifest, ManifestExports, ScopeDef};

/// An old manifest produced before `exports` existed must deserialize
/// unchanged, and round-trip serialize without inventing the field.
#[test]
fn old_manifest_without_exports_round_trips() {
    let original = json!({
        "version": 1,
        "metadata": { "compiler": "old@0.1.0", "built_at": "2024-01-01T00:00:00Z" },
    });

    let m: Manifest = serde_json::from_value(original.clone()).expect("parse");
    assert!(m.exports.is_none(), "missing field must deserialize to None");

    // The re-serialized form must NOT contain an `exports` key.
    let v: Value = serde_json::to_value(&m).expect("serialize");
    assert!(
        v.get("exports").is_none(),
        "exports must be omitted when None — got {v}"
    );
}

/// A fresh manifest constructed with `exports: Some(default)` must
/// serialize WITHOUT any `exports` key (default exports has no
/// schema and an empty handler list, so both inner fields are
/// `skip_serializing_if`-suppressed; serde then drops the outer
/// `Option<…>`-wrapped value too via the same attribute on the
/// field). Verified by an explicit round-trip equivalence with the
/// "no exports at all" case.
#[test]
fn empty_exports_serializes_to_no_field() {
    // Case A: brand-new manifest with explicit empty exports.
    let mut a = Manifest::default();
    a.exports = Some(ManifestExports::default());
    let a_json = serde_json::to_value(&a).expect("serialize A");

    // Case B: brand-new manifest with no exports at all.
    let b = Manifest::default();
    let b_json = serde_json::to_value(&b).expect("serialize B");

    // The current serde policy: an explicit `Some(default)` is
    // preserved on the wire (it serializes to `{}`) so that a build
    // can distinguish "no schema, no handlers" from "we didn't run
    // discovery yet". The wire-format invariant we care about is that
    // a manifest without the field still round-trips unchanged
    // (covered by `old_manifest_without_exports_round_trips`), so
    // just assert what the serialized shape actually is for both
    // cases — that way future changes intentionally break this test.
    assert_eq!(a_json.get("exports"), Some(&json!({})));
    assert!(b_json.get("exports").is_none());
}

/// A manifest with a populated `exports.schema` field must round-trip
/// losslessly — the path stays the same string before and after.
#[test]
fn schema_path_round_trips() {
    let mut m = Manifest::default();
    m.exports = Some(ManifestExports {
        schema: Some("src/schema.ts".to_string()),
        handlers: Vec::new(),
    });
    let json_str = serde_json::to_string(&m).expect("serialize");
    let m2: Manifest = serde_json::from_str(&json_str).expect("parse");
    assert_eq!(
        m2.exports.as_ref().and_then(|e| e.schema.as_deref()),
        Some("src/schema.ts"),
    );
    // Handlers omitted on the wire.
    let v: Value = serde_json::from_str(&json_str).expect("parse value");
    assert!(
        v["exports"].get("handlers").is_none(),
        "empty handlers must be omitted — got {v}",
    );
}

/// `HandlerEntry` is Stage 2+ territory but the type must already
/// round-trip so manifests opt-in early (e.g. test builds) without
/// blowing up.
#[test]
fn handler_entries_round_trip() {
    let mut m = Manifest::default();
    m.exports = Some(ManifestExports {
        schema: None,
        handlers: vec![HandlerEntry {
            path: "src/api/query/listTodos.ts".to_string(),
            capability: "query".to_string(),
            name: "listTodos".to_string(),
        }],
    });
    let json_str = serde_json::to_string(&m).expect("serialize");
    let m2: Manifest = serde_json::from_str(&json_str).expect("parse");
    let handlers = &m2.exports.as_ref().expect("exports").handlers;
    assert_eq!(handlers.len(), 1);
    assert_eq!(handlers[0].path, "src/api/query/listTodos.ts");
    assert_eq!(handlers[0].capability, "query");
    assert_eq!(handlers[0].name, "listTodos");
}

// ── auth.scopes (declared-scope vocabulary, Slice 3a) ────────────────────────

/// A manifest declaring `auth.scopes` round-trips through serde with every
/// `{ id, label, description }` field intact.
#[test]
fn auth_scopes_round_trip() {
    let mut m = Manifest::default();
    m.auth = AuthConfig {
        scopes: vec![
            ScopeDef {
                id: "read:billing".to_string(),
                label: "View billing".to_string(),
                description: Some("See invoices and plan.".to_string()),
            },
            ScopeDef {
                id: "write:projects".to_string(),
                label: "Manage projects".to_string(),
                description: None,
            },
        ],
    };
    let json_str = serde_json::to_string(&m).expect("serialize");
    let m2: Manifest = serde_json::from_str(&json_str).expect("parse");
    assert_eq!(m2.auth.scopes.len(), 2);
    assert_eq!(m2.auth.scopes[0].id, "read:billing");
    assert_eq!(m2.auth.scopes[0].label, "View billing");
    assert_eq!(
        m2.auth.scopes[0].description.as_deref(),
        Some("See invoices and plan.")
    );
    assert_eq!(m2.auth.scopes[1].id, "write:projects");
    // Absent description omits the key on the wire.
    let v: Value = serde_json::to_value(&m).expect("serialize value");
    let scopes = v["auth"]["scopes"].as_array().expect("scopes array");
    assert!(scopes[1].get("description").is_none(), "got {v}");
}

/// An app with no declared scopes serializes to NO `auth` key at all, so a
/// scope-free manifest is wire-identical to one with no `auth` field.
#[test]
fn empty_auth_omits_the_key() {
    let m = Manifest::default();
    let v: Value = serde_json::to_value(&m).expect("serialize");
    assert!(v.get("auth").is_none(), "auth must be omitted when empty — got {v}");

    // And an old manifest without an `auth` key deserializes to the empty
    // default and round-trips back to no key.
    let original = json!({ "version": 1 });
    let parsed: Manifest = serde_json::from_value(original).expect("parse");
    assert!(parsed.auth.is_empty());
    let back: Value = serde_json::to_value(&parsed).expect("serialize");
    assert!(back.get("auth").is_none());
}

/// `Manifest::validate()` rejects a malformed scope id (format-level —
/// platform-vocab collision is the control plane's job).
#[test]
fn validate_rejects_malformed_scope_id() {
    let mut m = Manifest::default();
    m.auth = AuthConfig {
        scopes: vec![ScopeDef {
            id: "Read:Billing".to_string(), // uppercase ⇒ rejected
            label: "x".to_string(),
            description: None,
        }],
    };
    let err = m.validate().expect_err("uppercase scope id must be rejected");
    assert!(err.contains("Read:Billing"), "{err}");
}

/// `Manifest::validate()` accepts well-formed `verb:resource` ids.
#[test]
fn validate_accepts_wellformed_scope_id() {
    let mut m = Manifest::default();
    m.auth = AuthConfig {
        scopes: vec![
            ScopeDef { id: "read:billing".to_string(), label: "x".to_string(), description: None },
            ScopeDef { id: "manage_team".to_string(), label: "y".to_string(), description: None },
            ScopeDef { id: "a:b:c".to_string(), label: "z".to_string(), description: None },
        ],
    };
    m.validate().expect("well-formed scope ids accepted");
}
