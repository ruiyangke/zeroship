//! Internal unit tests for the native WebSocket impl. Most user-facing
//! behaviour is tested via the integration tests in
//! `crates/runtime/tests/websocket_*.rs`; this module is for things
//! that benefit from in-crate access (private helpers).

#[test]
fn clamp_unsigned_short_examples() {
    use crate::dom;
    use crate::init::init_v8;

    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);
    dom::install_globals(scope, global);

    use super::algorithms::clamp_unsigned_short;

    // Build V8 numbers and run them through clamp_unsigned_short.
    let cases: &[(f64, u16)] = &[
        (0.0, 0),
        (1000.0, 1000),
        (65535.0, 65535),
        (65536.0, 65535), // clamp to top
        (-1.0, 0),        // clamp to bottom
        (f64::NAN, 0),    // NaN → 0
        (f64::INFINITY, 65535),
        (f64::NEG_INFINITY, 0),
        (1.5, 2),  // tie → round to even (1.5 → 2 since 1 is odd, +1 = 2)
        (2.5, 2),  // tie → round to even (2.5 → 2 since 2 is even)
        (3.5, 4),  // tie → round to even
        (1.4, 1),  // non-tie below
        (1.6, 2),  // non-tie above
        (-0.5, 0), // -0.5 → after clamp → 0.0 → 0
    ];
    for (input, expected) in cases {
        let v = v8::Number::new(scope, *input);
        let got = clamp_unsigned_short(scope, v.into());
        assert_eq!(
            got, *expected,
            "clamp_unsigned_short({}) expected {} got {}",
            input, expected, got
        );
    }
}
