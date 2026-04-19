//! Plugin system tests.

use std::sync::Arc;

use zeroship_runtime::{ModuleEntry, NativePlugin, NativeRegistrar, Runtime, init_v8};

/// A minimal plugin that claims a namespace and registers nothing.
struct NoopPlugin {
    ns: &'static str,
    display_name: &'static str,
}

impl NativePlugin for NoopPlugin {
    fn namespace(&self) -> &str {
        self.ns
    }
    fn name(&self) -> &str {
        self.display_name
    }
    fn register(&self, _r: &mut NativeRegistrar) {}
}

fn noop_modules() -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: "export function hello() {}".into(),
    }]
}

#[test]
#[should_panic(expected = "plugin namespace collision")]
fn duplicate_namespace_panics() {
    init_v8();
    let plugins: Vec<Arc<dyn NativePlugin>> = vec![
        Arc::new(NoopPlugin { ns: "db", display_name: "first" }),
        Arc::new(NoopPlugin { ns: "db", display_name: "second" }),
    ];
    // Plugins are registered during lazy isolate init (first dispatch).
    // Invoke `has_http_handler` which forces `ensure_initialized` through
    // nothing — we instead trigger via a dispatch_rpc which definitely
    // initializes and then panics inside `register_plugins`.
    let runtime = Runtime::builder()
        .modules(noop_modules())
        .plugins(plugins)
        .build();
    let _ = runtime.dispatch_rpc(r#"{"id":1,"method":"hello","params":[]}"#);
}

#[test]
fn unique_namespaces_ok() {
    init_v8();
    let plugins: Vec<Arc<dyn NativePlugin>> = vec![
        Arc::new(NoopPlugin { ns: "db", display_name: "db" }),
        Arc::new(NoopPlugin { ns: "kv", display_name: "kv" }),
    ];
    let runtime = Runtime::builder()
        .modules(noop_modules())
        .plugins(plugins)
        .build();
    // Force isolate initialization — this runs register_plugins, which must
    // not panic with unique namespaces.
    let _ = runtime.dispatch_rpc(r#"{"id":1,"method":"hello","params":[]}"#);
}
