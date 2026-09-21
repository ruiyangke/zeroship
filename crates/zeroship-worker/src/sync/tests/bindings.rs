//! Decoding control's binding response into the `ResolvedBinding` set an
//! isolate's session role is derived from, and addressing control's own routes
//! to ask for it.
//!
//! # Where the fixture shape comes from
//!
//! `zeroship_control::internal::get_app_bindings` selects
//! `b.id AS binding_id, b.database_id, d.schema_epoch`, narrows the epoch out
//! of its `i32` column with `u32::try_from`, and serves one JSON object per row
//! under a `bindings` array. [`control_entry`] mirrors that object key for key
//! and [`control_body`] mirrors the envelope.
//!
//! # Where the route comes from
//!
//! The paths these calls address are read from
//! `zeroship_core::service_identity::endpoints`, the same declarations control
//! authorizes each call against, and never restated here: a path spelled in
//! this file would be one more copy of the thing the worker is being bound to.
//!
//! # What these do not catch
//!
//! The fixtures are hand-built, so they exercise the CONSUMER alone. No shared
//! type, golden body or round trip joins the two sides: control composes its
//! object with an inline `serde_json::json!` and the worker reads it with
//! string keys, so a renamed or retyped field on the producer leaves this file
//! green until someone edits it too.
//!
//! The route half is bound to the DECLARATION, which is not the whole of the
//! producer either: control registers its routes in
//! `zeroship_control::main` with `web::resource` over a written-out path, so a
//! declaration and a registration that disagree are invisible from here.

use super::*;
use zeroship_core::{BindingId, DatabaseId};
use zeroship_data_orm::resolved_bindings::ResolvedBinding;

/// One entry exactly as control builds it: two typed ids as JSON strings and
/// the schema epoch as a JSON number.
fn control_entry(binding: &BindingId, database: &DatabaseId, epoch: u32) -> serde_json::Value {
    serde_json::json!({
        "binding_id": binding.as_str(),
        "database_id": database.as_str(),
        "schema_epoch": epoch,
    })
}

/// Control's envelope: the entries under a `bindings` array, serialized as the
/// HTTP body the worker hands to the parse.
fn control_body(entries: &[serde_json::Value]) -> String {
    serde_json::json!({ "bindings": entries }).to_string()
}

/// One entry with a single key replaced, so every rejection below differs from
/// its acceptance control in exactly one variable.
fn with_field(entry: &serde_json::Value, name: &str, value: serde_json::Value) -> String {
    let mut entry = entry.clone();
    entry
        .as_object_mut()
        .expect("the fixture entry is a JSON object")
        .insert(name.to_owned(), value);
    control_body(&[entry])
}

/// One entry with a single key removed.
fn without_field(entry: &serde_json::Value, name: &str) -> String {
    let mut entry = entry.clone();
    assert!(
        entry
            .as_object_mut()
            .expect("the fixture entry is a JSON object")
            .remove(name)
            .is_some(),
        "the fixture must carry {name} for its removal to be the variable under test"
    );
    control_body(&[entry])
}

#[test]
fn every_entry_is_decoded_field_for_field_at_a_non_zero_epoch() {
    let first = (BindingId::mint(), DatabaseId::mint(), 7_u32);
    let second = (BindingId::mint(), DatabaseId::mint(), 19_u32);
    let body = control_body(&[
        control_entry(&first.0, &first.1, first.2),
        control_entry(&second.0, &second.1, second.2),
    ]);

    let resolved = parse_resolved_bindings(&body).expect("control's own shape decodes");

    // Value equality over the WHOLE set, in order: an app binds many databases
    // and each entry composes its own role, so a parse that dropped one, or
    // carried one entry's epoch onto another, must not pass here.
    assert_eq!(
        resolved,
        vec![
            ResolvedBinding {
                database: first.1.clone(),
                binding: first.0.clone(),
                epoch: first.2,
            },
            ResolvedBinding {
                database: second.1,
                binding: second.0,
                epoch: second.2,
            },
        ]
    );

    // The control: one malformed entry refuses the WHOLE response rather than
    // installing the entries that happened to parse.
    let partial = format!(
        r#"{{"bindings":[{},{{"binding_id":"{}"}}]}}"#,
        control_entry(&first.0, &first.1, first.2),
        first.0.as_str(),
    );
    parse_resolved_bindings(&partial)
        .expect_err("one malformed entry refuses every entry beside it");
}

#[test]
fn a_zero_epoch_is_a_value_not_an_absence() {
    let binding = BindingId::mint();
    let database = DatabaseId::mint();
    let entry = control_entry(&binding, &database, 0);

    let resolved =
        parse_resolved_bindings(&control_body(std::slice::from_ref(&entry))).expect("zero decodes");
    assert_eq!(
        resolved,
        vec![ResolvedBinding {
            database,
            binding,
            epoch: 0,
        }],
        "control mints a database at epoch zero, so zero is the first value this \
         carries and must not read as a missing field"
    );

    // The control differing in one variable: the same entry with the epoch
    // outside what a `u32` holds is refused, so the acceptance above is the
    // value being accepted rather than the check being absent.
    let out_of_range = with_field(&entry, "schema_epoch", serde_json::json!(-1));
    parse_resolved_bindings(&out_of_range).expect_err("a negative epoch is refused");
}

#[test]
fn the_accepted_epoch_range_ends_at_u32_max() {
    let binding = BindingId::mint();
    let database = DatabaseId::mint();
    let entry = control_entry(&binding, &database, 1);

    // Control narrows an `i32` column, so `i32::MAX` is the largest epoch it
    // can serve, and `u32::MAX` the largest this parse accepts.
    for accepted in [
        u32::try_from(i32::MAX).expect("i32::MAX is non-negative"),
        u32::MAX,
    ] {
        let body = with_field(&entry, "schema_epoch", serde_json::json!(accepted));
        let resolved = parse_resolved_bindings(&body).unwrap_or_else(|error| {
            panic!("{accepted} is inside the epoch range this parse accepts: {error}")
        });
        assert_eq!(
            resolved.first().expect("one entry in, one entry out").epoch,
            accepted
        );
    }

    let over = u64::from(u32::MAX) + 1;
    let body = with_field(&entry, "schema_epoch", serde_json::json!(over));
    let error = parse_resolved_bindings(&body)
        .expect_err("an epoch above u32::MAX composes no role and is refused");
    assert!(
        error.contains("schema_epoch"),
        "the refusal must name the field that failed: {error}"
    );
}

#[test]
fn a_schema_epoch_that_is_absent_or_not_a_number_is_refused() {
    let binding = BindingId::mint();
    let database = DatabaseId::mint();
    let entry = control_entry(&binding, &database, 3);

    // The acceptance control: this exact entry parses, so each rejection below
    // is caused by the one key it changes.
    parse_resolved_bindings(&control_body(std::slice::from_ref(&entry)))
        .expect("the unmutated entry is accepted");

    let refusals = [
        ("absent", without_field(&entry, "schema_epoch")),
        (
            "a string",
            with_field(&entry, "schema_epoch", serde_json::json!("3")),
        ),
        (
            "null",
            with_field(&entry, "schema_epoch", serde_json::Value::Null),
        ),
        (
            "negative",
            with_field(&entry, "schema_epoch", serde_json::json!(-1)),
        ),
    ];
    for (label, body) in refusals {
        let Err(error) = parse_resolved_bindings(&body) else {
            panic!("an epoch that is {label} must refuse, not compose a role name");
        };
        assert!(
            error.contains("schema_epoch"),
            "an epoch that is {label} must be refused by name: {error}"
        );
    }
}

#[test]
fn a_bindings_envelope_that_is_absent_or_not_an_array_is_refused() {
    let entry = control_entry(&BindingId::mint(), &DatabaseId::mint(), 5);

    // The acceptance control: the envelope control serves.
    assert_eq!(
        parse_resolved_bindings(&control_body(std::slice::from_ref(&entry)))
            .expect("control's envelope decodes")
            .len(),
        1
    );

    for body in [
        serde_json::json!({ "error": "no live database binding" }).to_string(),
        serde_json::json!({ "bindings": entry }).to_string(),
        serde_json::json!({ "bindings": serde_json::Value::Null }).to_string(),
    ] {
        let error = parse_resolved_bindings(&body)
            .expect_err("a response carrying no bindings ARRAY resolves nothing");
        assert!(
            error.contains("bindings"),
            "the refusal must name the missing envelope: {error}"
        );
    }

    // A body that is not JSON at all is a different refusal, so the envelope
    // message above is not simply what every bad body produces.
    let error = parse_resolved_bindings("not json at all").expect_err("a non-JSON body is refused");
    assert!(
        !error.contains("bindings"),
        "an undecodable body is refused before any field is looked for: {error}"
    );
}

#[test]
fn an_id_that_is_absent_or_unparseable_is_refused() {
    let binding = BindingId::mint();
    let database = DatabaseId::mint();
    let entry = control_entry(&binding, &database, 11);

    parse_resolved_bindings(&control_body(std::slice::from_ref(&entry)))
        .expect("the unmutated entry is accepted");

    for name in ["binding_id", "database_id"] {
        let error = parse_resolved_bindings(&without_field(&entry, name))
            .expect_err("an absent id composes no binding");
        assert!(
            error.contains(name),
            "the refusal must name the absent field: {error}"
        );

        let error = parse_resolved_bindings(&with_field(&entry, name, serde_json::json!(7)))
            .expect_err("an id that is not text composes no binding");
        assert!(
            error.contains(name),
            "the refusal must name the field that was not text: {error}"
        );

        parse_resolved_bindings(&with_field(&entry, name, serde_json::json!("not-an-id")))
            .expect_err("an id outside the typed-id grammar composes no binding");
    }
}

#[test]
fn the_two_ids_are_held_apart_by_their_prefixes() {
    let binding = BindingId::mint();
    let database = DatabaseId::mint();

    // Swapped, not corrupted: both values are canonical typed ids, and only the
    // prefix says which slot each belongs in. The role name is derived from the
    // binding id, so a parse that took them interchangeably would name a role
    // nothing created.
    let swapped = serde_json::json!({
        "binding_id": database.as_str(),
        "database_id": binding.as_str(),
        "schema_epoch": 2,
    });
    let error = parse_resolved_bindings(&control_body(&[swapped]))
        .expect_err("a database id in the binding slot is refused");
    assert!(
        error.contains(DatabaseId::PREFIX) && error.contains(BindingId::PREFIX),
        "the refusal must name the prefix the slot requires and the one it got: {error}"
    );

    // The control: the same two ids in their own slots decode.
    let resolved = parse_resolved_bindings(&control_body(&[control_entry(&binding, &database, 2)]))
        .expect("the same ids in their own slots decode");
    assert_eq!(
        resolved,
        vec![ResolvedBinding {
            database,
            binding,
            epoch: 2,
        }]
    );
}

#[test]
fn an_empty_bindings_array_resolves_no_binding_without_refusing() {
    // Control answers 404 rather than an empty array when an app binds nothing,
    // so this pins the consumer's own tolerance: an empty SET is not a
    // malformed response.
    assert_eq!(
        parse_resolved_bindings(&control_body(&[])).expect("an empty set is not a refusal"),
        Vec::new()
    );

    // Paired with a nonempty input through the same call, so the assertion
    // above cannot be the whole of what this test observes.
    let binding = BindingId::mint();
    let database = DatabaseId::mint();
    assert_eq!(
        parse_resolved_bindings(&control_body(&[control_entry(&binding, &database, 13)]))
            .expect("one entry decodes"),
        vec![ResolvedBinding {
            database,
            binding,
            epoch: 13,
        }]
    );
}

/// The refusal one body produces, so two conditions can be COMPARED rather
/// than each checked against a phrase this file would then own a copy of.
fn refusal(body: &str) -> String {
    parse_resolved_bindings(body).expect_err("this body composes no binding")
}

/// One refusal with the offending value's rendering removed, leaving the part
/// of the message that names the CONDITION. Two inputs that failed the same
/// way share this; two that failed differently must not.
fn condition(message: &str, value: &str) -> String {
    message.replace(value, "")
}

#[test]
fn a_refused_schema_epoch_names_which_condition_failed_rather_than_absence() {
    let binding = BindingId::mint();
    let database = DatabaseId::mint();
    let entry = control_entry(&binding, &database, 3);

    // The acceptance control: this exact entry decodes, so every refusal below
    // is caused by the one key it changes.
    parse_resolved_bindings(&control_body(std::slice::from_ref(&entry)))
        .expect("the unmutated entry is accepted");

    let absent = refusal(&without_field(&entry, "schema_epoch"));
    let text = refusal(&with_field(&entry, "schema_epoch", serde_json::json!("3")));
    let null = refusal(&with_field(&entry, "schema_epoch", serde_json::Value::Null));
    let negative = refusal(&with_field(&entry, "schema_epoch", serde_json::json!(-1)));
    let over = refusal(&with_field(
        &entry,
        "schema_epoch",
        serde_json::json!(u64::from(u32::MAX) + 1),
    ));
    let fractional = refusal(&with_field(&entry, "schema_epoch", serde_json::json!(3.5)));

    // In four of these five the field is PRESENT, so a refusal reporting an
    // absence sends the operator after a field control served.
    for (label, present) in [
        ("text", &text),
        ("null", &null),
        ("negative", &negative),
        ("above the range", &over),
        ("fractional", &fractional),
    ] {
        assert_ne!(
            present, &absent,
            "a schema_epoch that is {label} is present, so its refusal must not \
             be the one an absent field produces"
        );
        assert!(
            present.contains("schema_epoch"),
            "a schema_epoch that is {label} must be refused by name: {present}"
        );
    }
    assert!(
        absent.contains("schema_epoch"),
        "an absent schema_epoch must be refused by name: {absent}"
    );

    // A number the range cannot hold and a number that is not whole are
    // different repairs, so their conditions differ with the value stripped
    // out - value inequality alone would leave a single message wearing
    // whatever it was handed.
    assert_ne!(
        condition(&negative, "-1"),
        condition(&fractional, "3.5"),
        "a value outside the range and a value that is not whole are different \
         conditions"
    );
    assert_ne!(
        condition(&text, "3"),
        condition(&negative, "-1"),
        "a field that is not a number at all and a number outside the range are \
         different conditions"
    );
    assert_ne!(
        text, null,
        "a refusal must name the type it found, so text and null differ"
    );

    // The control on that stripping: two inputs that failed the SAME way share
    // one condition, so the inequalities above are the wording differing and
    // not merely the value appearing inside it.
    assert_eq!(
        condition(&negative, "-1"),
        condition(&over, "4294967296"),
        "both of these are epochs outside the range, and report it the same way"
    );

    // And the value is carried, so an operator reads what control served
    // rather than only that it was wrong.
    assert!(
        negative.contains("-1") && over.contains("4294967296") && fractional.contains("3.5"),
        "a numeric refusal must carry the value it refused: {negative} / {over} / {fractional}"
    );
}

#[test]
fn a_refused_id_says_whether_it_was_absent_or_present_and_not_text() {
    let binding = BindingId::mint();
    let database = DatabaseId::mint();
    let entry = control_entry(&binding, &database, 17);

    parse_resolved_bindings(&control_body(std::slice::from_ref(&entry)))
        .expect("the unmutated entry is accepted");

    for name in ["binding_id", "database_id"] {
        let absent = refusal(&without_field(&entry, name));
        let number = refusal(&with_field(&entry, name, serde_json::json!(7)));
        let null = refusal(&with_field(&entry, name, serde_json::Value::Null));
        assert_ne!(
            absent, number,
            "a {name} control served as a number is present, so its refusal must \
             not be the one an absent field produces"
        );
        assert_ne!(
            number, null,
            "a refusal must name the type it found in {name}"
        );
        for message in [&absent, &number, &null] {
            assert!(
                message.contains(name),
                "every refusal must name the field that failed: {message}"
            );
        }
    }
}

/// Every control route this module addresses by app id.
///
/// A hand-written list of the ENDPOINTS, never of their paths: every path is
/// read from `zeroship_core::service_identity::endpoints`, so a route that
/// moves moves in one place.
const APP_ADDRESSED: [ServiceEndpoint; 4] = [
    endpoints::CONTROL_APP,
    endpoints::CONTROL_APP_ENV,
    endpoints::CONTROL_APP_DATA_KEY,
    endpoints::CONTROL_APP_BINDINGS,
];

/// The path one app-addressed route declares, with its parameter filled.
///
/// Derived from the SHAPE of the template - its one `{...}` span - rather than
/// from the parameter's name, so this and `control_app_url` reach the same
/// string by different routes and a renamed parameter separates them.
fn declared_path(endpoint: ServiceEndpoint, app_id: &AppId) -> String {
    let template = endpoint.path_template();
    let open = template
        .find('{')
        .unwrap_or_else(|| panic!("{template} declares a path parameter"));
    let close = open
        + template[open..]
            .find('}')
            .unwrap_or_else(|| panic!("{template} closes its path parameter"));
    format!(
        "{}{}{}",
        &template[..open],
        app_id.as_str(),
        &template[close + 1..]
    )
}

/// A control plane that records what the worker ADDRESSED and serves nothing.
///
/// It accepts `expected` connections, reads the request line of each and drops
/// the connection, so the caller's own error path ends the call. The address
/// is what these assertions read, and a refused request carries it exactly as
/// a served one does.
fn recording_control(expected: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
    use std::io::Read;

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a control plane binds");
    let base = format!("http://{}", listener.local_addr().expect("its address"));
    listener
        .set_nonblocking(true)
        .expect("poll for connections so a call that never arrives ends the wait");
    let handle = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut paths = Vec::new();
        while paths.len() < expected && std::time::Instant::now() < deadline {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("accept a control request: {error}"),
            };
            stream
                .set_nonblocking(false)
                .expect("read the request head as it arrives");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .expect("bound that read");
            let mut head = Vec::new();
            let mut chunk = [0_u8; 256];
            while !head.windows(2).any(|pair| pair == b"\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => head.extend_from_slice(&chunk[..read]),
                }
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            let request_line = head.lines().next().unwrap_or_default();
            paths.push(
                request_line
                    .split(' ')
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned(),
            );
        }
        paths
    });
    (base, handle)
}

#[test]
fn every_app_addressed_control_url_is_the_declared_route_with_its_parameter_filled() {
    let app = AppId::mint();
    let base = "http://control.invalid:8080";
    assert!(
        !APP_ADDRESSED.is_empty(),
        "a route table with nothing in it would pass every check below"
    );

    let mut built = Vec::new();
    for endpoint in APP_ADDRESSED {
        let url = control_app_url(base, endpoint, app.as_str());
        assert_eq!(
            url,
            format!("{base}{}", declared_path(endpoint, &app)),
            "the URL the worker sends is the route control declares"
        );
        assert!(
            !url.contains('{') && !url.contains('}'),
            "the declared parameter must be filled rather than travelling to \
             control as a literal: {url}"
        );
        // The rejection control: the template with nothing filled in is a
        // different string, so the equality above is the fill having happened.
        assert_ne!(
            url,
            format!("{base}{}", endpoint.path_template()),
            "an unfilled template addresses no app"
        );
        built.push(url);
    }

    // And each endpoint addresses its OWN route, so a call site reaching for
    // the wrong declaration is not covered by this test passing.
    built.sort();
    built.dedup();
    assert_eq!(
        built.len(),
        APP_ADDRESSED.len(),
        "two endpoints must not build one URL"
    );
}

#[compio::test]
async fn the_control_reads_address_the_routes_control_declares() {
    let app = AppId::mint();
    let auth = crate::identity_fixture::service_auth();

    // One recorder per call, so a path is attributed to the call that sent it
    // rather than to the order two calls happened to interleave in.
    let (base, recorder) = recording_control(1);
    let refused = fetch_app_version(&base, &auth, &app).await;
    assert_eq!(
        recorder.join().expect("the recording control plane"),
        vec![declared_path(endpoints::CONTROL_APP, &app)],
        "the version read addresses the route control declares"
    );
    // The control on what this measures: nothing was served, so the call
    // failed. What is under test is the address, not a decoded body.
    refused.expect_err("a control plane that answers nothing resolves no version");

    let (base, recorder) = recording_control(2);
    let supplied = zeroship_data_orm::resolved_bindings::SuppliedAppBindings::new();
    let published = fetch_app_env_supplying(&base, &auth, &app, None, Some(&supplied)).await;
    assert_eq!(
        recorder.join().expect("the recording control plane"),
        vec![
            declared_path(endpoints::CONTROL_APP_BINDINGS, &app),
            declared_path(endpoints::CONTROL_APP_ENV, &app),
        ],
        "the binding is resolved at its declared route before the environment \
         is read at its own"
    );
    published.expect_err("a control plane that answers nothing publishes no environment");

    let (base, recorder) = recording_control(1);
    let keys = zeroship_data_orm::encryption::SuppliedProjectKeys::new();
    let refused = fetch_app_env_supplying(&base, &auth, &app, Some(&keys), None).await;
    assert_eq!(
        recorder.join().expect("the recording control plane"),
        vec![declared_path(endpoints::CONTROL_APP_DATA_KEY, &app)],
        "the project key read addresses the route control declares"
    );
    refused.expect_err("a control plane that answers nothing supplies no project key");
}
