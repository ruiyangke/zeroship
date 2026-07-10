#![cfg(feature = "zsv8")]
//! Eval a sample `schema.js` (with encrypted/vector/mask/ref facets) in the V8
//! sandbox and assert the produced descriptor IR matches the expected
//! `CollectionDescriptor`s. No DB required — this exercises the front-end's
//! eval + lowering in isolation.

use zeroship_migrate::frontend::eval_schema_to_ir;
use zeroship_migrate::render::declarative::{CollectionDescriptor, FieldDescriptor};

const SAMPLE: &str = include_str!("fixtures/sample_schema.js");

fn collection<'a>(ir: &'a [CollectionDescriptor], name: &str) -> &'a CollectionDescriptor {
    ir.iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("collection {name} not in IR"))
}

fn field<'a>(c: &'a CollectionDescriptor, name: &str) -> &'a FieldDescriptor {
    c.fields
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("field {name} not in collection {}", c.name))
}

#[test]
fn evals_sample_schema_to_descriptor_ir() {
    let ir = eval_schema_to_ir(SAMPLE, "app_test").expect("eval should succeed");

    // Two collections, both stamped with the owner app.
    assert_eq!(ir.len(), 2, "two collections");
    for c in &ir {
        assert_eq!(c.owner_app, "app_test", "owner stamped");
    }

    let users = collection(&ir, "users");
    let docs = collection(&ir, "docs");

    // --- users.email: required + unique string ---
    let email = field(users, "email");
    assert_eq!(email.ty, "string");
    assert!(email.required);
    assert!(email.unique);

    // --- users.ssn: encrypted (deterministic, pii_key) + auto-mask ---
    let ssn = field(users, "ssn");
    // Encrypted strings carry the logical `string` type token.
    assert_eq!(ssn.ty, "string");
    let enc = ssn.encrypted.as_ref().expect("ssn carries encrypted facet");
    assert_eq!(enc["mode"], "deterministic");
    assert_eq!(enc["keyId"], "pii_key");
    // t.encrypted auto-populates a default mask.
    let mask = ssn.mask.as_ref().expect("encrypted auto-masks");
    assert_eq!(mask["kind"], "full");
    assert_eq!(mask["classification"], "pii");

    // --- users.phone: explicit partial mask ---
    let phone = field(users, "phone");
    let pmask = phone.mask.as_ref().expect("phone carries explicit mask");
    assert_eq!(pmask["kind"], "last4");

    // --- docs.embedding: vector(1536, cosine) ---
    let emb = field(docs, "embedding");
    assert_eq!(emb.ty, "vector");
    assert_eq!(emb.vector_dims, Some(1536));
    assert_eq!(emb.vector_metric.as_deref(), Some("cosine"));

    // --- docs.authorId: ref(users) ON DELETE cascade ---
    let author = field(docs, "authorId");
    assert_eq!(author.ty, "ref");
    assert_eq!(author.references.as_deref(), Some("users"));
    assert_eq!(author.on_delete.as_deref(), Some("cascade"));
}

#[test]
fn named_schema_export_also_supported() {
    let src = r#"
        import { t } from "@zeroship/db";
        export const schema = {
            items: { name: t.string().required() },
        };
    "#;
    let ir = eval_schema_to_ir(src, "app_x").expect("named export eval");
    assert_eq!(ir.len(), 1);
    assert_eq!(ir[0].name, "items");
    assert_eq!(ir[0].fields[0].name, "name");
}

#[test]
fn schema_js_cannot_forge_owner_app() {
    let src = r#"
        import { t } from "@zeroship/db";
        globalThis.__zsOwnerApp = "app_victim";
        export const schema = {
            items: { name: t.string().required() },
        };
    "#;
    let ir = eval_schema_to_ir(src, "app_trusted").expect("schema eval");
    assert_eq!(ir.len(), 1);
    assert_eq!(
        ir[0].owner_app, "app_trusted",
        "owner_app must be stamped by Rust, not read from creator-mutated JS globals"
    );
}

#[test]
fn missing_schema_export_is_lowering_error() {
    let src = r#"export default { fetch() {} };"#;
    let err = eval_schema_to_ir(src, "app_x").unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("neither") || msg.to_lowercase().contains("schema"),
        "expected a schema-missing lowering error, got: {msg}"
    );
}

#[test]
fn syntax_error_is_v8_error() {
    let src = r#"this is not valid javascript {{{"#;
    let err = eval_schema_to_ir(src, "app_x").unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("evaluation failed") || msg.to_lowercase().contains("compile"),
        "expected a V8 eval error, got: {msg}"
    );
}
