//! Native URL implementation using ada-url (same parser as Node.js).
//!
//! Exposes `__urlParse(input, base?)` and `__urlCanParse(input, base?)` to JS.
//! The JS `URL` class wraps these native calls for spec-compliant URL parsing.

use appbase_runtime_macros::appbase_op;

/// V8 callback: `__urlParse(input, base?) → Object | null`
///
/// Returns a V8 Object with all URL components directly — no JSON serialization.
pub fn url_parse_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let input = args.get(0).to_rust_string_lossy(scope);

    let base = if args.length() > 1 && !args.get(1).is_null_or_undefined() {
        Some(args.get(1).to_rust_string_lossy(scope))
    } else {
        None
    };

    let result = ada_url::Url::parse(&input, base.as_deref());

    match result {
        Ok(url) => {
            let obj = v8::Object::new(scope);

            macro_rules! set_prop {
                ($key:expr, $val:expr) => {
                    let k = v8::String::new(scope, $key).unwrap();
                    let v = v8::String::new(scope, &$val).unwrap();
                    obj.set(scope, k.into(), v.into());
                };
            }

            set_prop!("href", url.href());
            set_prop!("protocol", url.protocol());
            set_prop!("username", url.username());
            set_prop!("password", url.password());
            set_prop!("hostname", url.hostname());
            set_prop!("port", url.port());
            set_prop!("host", url.host());
            set_prop!("pathname", url.pathname());
            set_prop!("search", url.search());
            set_prop!("hash", url.hash());
            set_prop!("origin", url.origin());

            rv.set(obj.into());
        }
        Err(_) => {
            rv.set(v8::null(scope).into());
        }
    }
}

/// `__urlCanParse(input, base?) → boolean`
#[appbase_op]
fn url_can_parse(input: String, base: Option<String>) -> bool {
    ada_url::Url::can_parse(&input, base.as_deref())
}
