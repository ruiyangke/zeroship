use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::{JavaScriptModule, NativePlugin, NativeRegistrar};
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx};

struct AdapterPlugin(&'static [JavaScriptModule]);

struct HostAdapterPlugin(&'static [JavaScriptModule]);

struct MixedAdapterPlugin;

fn value_callback(
    _scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set_int32(42);
}

impl NativePlugin for AdapterPlugin {
    fn namespace(&self) -> &str {
        "fixture"
    }
    fn register(&self, registrar: &mut NativeRegistrar) {
        registrar.add("value", value_callback);
    }
    fn javascript_modules(&self) -> &'static [JavaScriptModule] {
        self.0
    }
}

impl NativePlugin for HostAdapterPlugin {
    fn namespace(&self) -> &str {
        "fixture"
    }
    fn register(&self, registrar: &mut NativeRegistrar) {
        registrar.add("value", value_callback);
    }
    fn host_javascript_modules(&self) -> &'static [JavaScriptModule] {
        self.0
    }
}

impl NativePlugin for MixedAdapterPlugin {
    fn namespace(&self) -> &str {
        "fixture"
    }
    fn register(&self, registrar: &mut NativeRegistrar) {
        registrar.add("value", value_callback);
    }
    fn javascript_modules(&self) -> &'static [JavaScriptModule] {
        &[JavaScriptModule {
            specifier: "zeroship:fixture/public",
            source: r#"
                export async function readPrivate() {
                    return (await import("zeroship:fixture/private")).secret;
                }
            "#,
        }]
    }
    fn host_javascript_modules(&self) -> &'static [JavaScriptModule] {
        &[JavaScriptModule {
            specifier: "zeroship:fixture/private",
            source: "export const secret = 'private';",
        }]
    }
}

const ADAPTERS: &[JavaScriptModule] = &[
    JavaScriptModule {
        specifier: "zeroship:fixture/adapter",
        source: r#"
            import { env } from "zeroship";
            import { AsyncLocalStorage } from "node:async_hooks";
            import { token } from "zeroship:fixture/value";
            export { token };
            export function read() {
                const local = new AsyncLocalStorage();
                return local.run(env.fixture.value(), () => local.getStore());
            }
        "#,
    },
    JavaScriptModule {
        specifier: "zeroship:fixture/value",
        source: "export const token = {};",
    },
];

fn call(runtime: &Runtime) -> (u16, String) {
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://localhost/",
        &[],
        "",
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8(body.to_vec()).unwrap())
        }
        _ => panic!("adapter fixture should settle without external I/O"),
    }
}

fn runtime(source: &str, modules: &'static [JavaScriptModule], extra: Vec<ModuleEntry>) -> Runtime {
    zeroship_runtime::init_v8();
    let mut entries = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: source.into(),
    }];
    entries.extend(extra);
    Runtime::builder()
        .modules(entries)
        .plugin(AdapterPlugin(modules))
        .build()
}

fn host_runtime(source: &str, modules: &'static [JavaScriptModule]) -> Runtime {
    zeroship_runtime::init_v8();
    Runtime::builder()
        .modules(vec![ModuleEntry {
            specifier: "index.js".into(),
            source: source.into(),
        }])
        .plugin(HostAdapterPlugin(modules))
        .build()
}

fn mixed_runtime(source: &str) -> Runtime {
    zeroship_runtime::init_v8();
    Runtime::builder()
        .modules(vec![ModuleEntry {
            specifier: "index.js".into(),
            source: source.into(),
        }])
        .plugin(MixedAdapterPlugin)
        .build()
}

#[test]
fn creator_static_import_cannot_resolve_host_only_adapter() {
    let runtime = host_runtime(
        r#"
        import "zeroship:fixture/adapter";
        export default { fetch() { return new Response("unexpected import"); } };
    "#,
        ADAPTERS,
    );
    let (status, body) = call(&runtime);
    assert_eq!(status, 500, "{body}");
    assert!(body.contains("cannot import host-only module"), "{body}");
}

#[test]
fn creator_dynamic_import_cannot_resolve_host_only_adapter() {
    let runtime = host_runtime(
        r#"
        export default { async fetch() {
            try { await import("zeroship:fixture/adapter"); }
            catch (error) { return new Response(error.message); }
            return new Response("unexpected import");
        } };
    "#,
        ADAPTERS,
    );
    let (status, body) = call(&runtime);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "Cannot find module 'zeroship:fixture/adapter'");
}

#[test]
fn creator_importable_adapter_cannot_reexport_host_only_module() {
    let runtime = mixed_runtime(
        r#"
        import { readPrivate } from "zeroship:fixture/public";
        export default { async fetch() {
            try { await readPrivate(); }
            catch (error) { return new Response(error.message); }
            return new Response("unexpected import");
        } };
    "#,
    );
    let (status, body) = call(&runtime);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "Cannot find module 'zeroship:fixture/private'");
}

#[test]
fn static_and_dynamic_adapter_imports_share_native_environment_and_module_identity() {
    let runtime = runtime(
        r#"
        import { token, read } from "zeroship:fixture/adapter";
        export default { async fetch() {
            const dynamic = await import("zeroship:fixture/adapter");
            return Response.json({ same: token === dynamic.token, value: read(), dynamic: dynamic.read() });
        } };
    "#,
        ADAPTERS,
        vec![],
    );
    let (status, body) = call(&runtime);
    assert_eq!(status, 200, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"same":true,"value":42,"dynamic":42})
    );
}

#[test]
fn dynamic_only_adapter_import_instantiates_its_dependency_graph() {
    let runtime = runtime(
        r#"
        export default { async fetch() {
            const a = await import("zeroship:fixture/adapter");
            const b = await import("zeroship:fixture/adapter");
            return Response.json({ same: a === b, value: a.read() });
        } };
    "#,
        ADAPTERS,
        vec![],
    );
    let (status, body) = call(&runtime);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"same":true,"value":42})
    );
}

#[test]
fn nested_lazy_bundle_and_adapter_imports_share_evaluation_and_identity() {
    let runtime = runtime(
        r#"
        const initial = await import('./app/chunks/lazy.js');
        export default { async fetch() {
            const repeated = await import('./app/chunks/lazy.js');
            const adapter = await import('zeroship:fixture/adapter');
            return Response.json({
                same: initial === repeated,
                token: initial.token === adapter.token,
                value: await initial.read(),
                evaluations: globalThis.lazyEvaluations,
            });
        } };
    "#,
        &[
            JavaScriptModule {
                specifier: "zeroship:fixture/adapter",
                source: r#"
                    import { token } from './value';
                    export { token };
                    export async function read() {
                        const value = await import('./value');
                        const { env } = await import('zeroship');
                        const { AsyncLocalStorage } = await import('node:async_hooks');
                        const local = new AsyncLocalStorage();
                        return local.run(value.token === token, () => ({
                            same: local.getStore(), value: env.fixture.value(),
                        }));
                    }
                "#,
            },
            JavaScriptModule {
                specifier: "zeroship:fixture/value",
                source: "await Promise.resolve(); export const token = {};",
            },
        ],
        vec![
            ModuleEntry {
                specifier: "app/chunks/lazy.js".into(),
                source: r#"
                    import { token, read as readAdapter } from 'zeroship:fixture/adapter';
                    import label from '../label.js';
                    await Promise.resolve().then(() => {});
                    globalThis.lazyEvaluations = (globalThis.lazyEvaluations ?? 0) + 1;
                    export { token };
                    export async function read() { return { label, adapter: await readAdapter() }; }
                "#
                .into(),
            },
            ModuleEntry {
                specifier: "app/label.js".into(),
                source: "export default 'nested';".into(),
            },
            ModuleEntry {
                specifier: "label.js".into(),
                source: "throw new Error('root fallback must never execute');".into(),
            },
            ModuleEntry {
                specifier: "unused.js".into(),
                source: "this source is deliberately invalid JavaScript".into(),
            },
        ],
    );
    let (status, body) = call(&runtime);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
            "same":true,
            "token":true,
            "value":{"label":"nested","adapter":{"same":true,"value":42}},
            "evaluations":1,
        })
    );
}

#[test]
fn dynamic_core_facade_import_resolves_without_plugin_adapters() {
    let runtime = runtime(
        r#"
        export default { async fetch() {
            const { env } = await import("zeroship");
            return Response.json({ value: env.fixture.value() });
        } };
    "#,
        &[],
        vec![],
    );
    let (status, body) = call(&runtime);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"value":42})
    );
}

#[test]
fn runtime_without_db_plugin_does_not_supply_the_db_adapter() {
    let runtime = runtime(
        r#"
        export default { async fetch() {
            try { await import("zeroship:db/adapter"); }
            catch (error) { return new Response(error.message); }
            return new Response('unexpected DB adapter');
        } };
    "#,
        &[],
        vec![],
    );
    let (status, body) = call(&runtime);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "Cannot find module 'zeroship:db/adapter'");
}

#[test]
fn dynamic_adapter_import_waits_for_top_level_await_and_reuses_evaluation() {
    let runtime = runtime(
        r#"
        export default { async fetch() {
            const [a, b] = await Promise.all([
                import("zeroship:fixture/adapter"),
                import("zeroship:fixture/adapter"),
            ]);
            return Response.json({ same: a === b, ready: a.ready });
        } };
    "#,
        &[JavaScriptModule {
            specifier: "zeroship:fixture/adapter",
            source: "await Promise.resolve().then(() => {}).then(() => {}); export const ready = true;",
        }],
        vec![],
    );
    let (status, body) = call(&runtime);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"same":true,"ready":true})
    );
}

#[test]
fn dynamic_adapter_import_preserves_async_evaluation_failure() {
    let runtime = runtime(
        r#"
        export default { async fetch() {
            const errors = [];
            for (let i = 0; i < 2; i++) {
                try { await import("zeroship:fixture/adapter"); }
                catch (error) { errors.push(error); }
            }
            return Response.json({ same: errors[0] === errors[1], messages: errors.map(e => e.message) });
        } };
    "#,
        &[JavaScriptModule {
            specifier: "zeroship:fixture/adapter",
            source: "await Promise.resolve().then(() => {}).then(() => { throw new Error('adapter-failed'); });",
        }],
        vec![],
    );
    let (status, body) = call(&runtime);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"same":true,"messages":["adapter-failed","adapter-failed"]})
    );
}

#[test]
fn dynamic_adapter_import_rejects_linking_errors() {
    let runtime = runtime(
        r#"
        export default { async fetch() {
            try { await import("zeroship:fixture/adapter"); }
            catch (error) { return Response.json({ name: error.name, message: error.message }); }
            return new Response('unexpected success');
        } };
    "#,
        &[
            JavaScriptModule {
                specifier: "zeroship:fixture/adapter",
                source: "import { missing } from 'zeroship:fixture/value'; export { missing };",
            },
            JavaScriptModule {
                specifier: "zeroship:fixture/value",
                source: "export {};",
            },
        ],
        vec![],
    );
    let (status, body) = call(&runtime);
    assert_eq!(status, 200, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["name"], "SyntaxError");
    assert!(
        value["message"].as_str().unwrap().contains("missing"),
        "{body}"
    );
}

fn assert_rejected(modules: &'static [JavaScriptModule], extra: Vec<ModuleEntry>, expected: &str) {
    let runtime = runtime(
        "export default { fetch() { return new Response('creator-ran'); } };",
        modules,
        extra,
    );
    let first = call(&runtime);
    assert_eq!(first.0, 500, "{}", first.1);
    assert!(first.1.contains(expected), "{}", first.1);
    assert_eq!(
        call(&runtime),
        first,
        "initialization failure must remain cached"
    );
}

#[test]
fn creator_artifacts_cannot_replace_host_sources() {
    for specifier in [
        "zeroship:fixture/adapter",
        "./zeroship:fixture/adapter",
        "zeroship",
        "./zeroship.js",
    ] {
        assert_rejected(
            ADAPTERS,
            vec![ModuleEntry {
                specifier: specifier.into(),
                source: "export const token = 'forged';".into(),
            }],
            "reserved host specifier",
        );
    }
}

#[test]
fn adapter_dependencies_cannot_be_supplied_by_the_creator_artifact() {
    assert_rejected(
        &[JavaScriptModule {
            specifier: "zeroship:fixture/adapter",
            source: "export { token } from 'app-helper.js';",
        }],
        vec![ModuleEntry {
            specifier: "app-helper.js".into(),
            source: "export const token = {};".into(),
        }],
        "cannot import creator module",
    );
}

#[test]
fn dynamic_adapter_dependencies_cannot_be_supplied_by_the_creator_artifact() {
    let runtime = runtime(
        r#"
        import 'app-helper.js';
        export default { async fetch() {
            const adapter = await import('zeroship:fixture/adapter');
            try { await adapter.read(); }
            catch (error) { return Response.json({ name: error.name, message: error.message }); }
            return new Response('unexpected creator dependency');
        } };
    "#,
        &[JavaScriptModule {
            specifier: "zeroship:fixture/adapter",
            source: "export async function read() { return import('app-helper.js'); }",
        }],
        vec![ModuleEntry {
            specifier: "app-helper.js".into(),
            source: "export const token = {};".into(),
        }],
    );
    let (status, body) = call(&runtime);
    assert_eq!(status, 200, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["name"], "TypeError");
    assert!(
        value["message"]
            .as_str()
            .unwrap()
            .contains("cannot import creator module"),
        "{body}"
    );
}

#[test]
fn module_registration_rejects_missing_empty_duplicate_and_foreign_sources() {
    assert_rejected(
        &[JavaScriptModule {
            specifier: "zeroship:fixture/adapter",
            source: "import 'zeroship:fixture/missing';",
        }],
        vec![],
        "Cannot resolve import",
    );
    assert_rejected(
        &[JavaScriptModule {
            specifier: "zeroship:fixture/adapter",
            source: " ",
        }],
        vec![],
        "empty plugin module",
    );
    assert_rejected(
        &[
            JavaScriptModule {
                specifier: "zeroship:fixture/adapter",
                source: "export {};",
            },
            JavaScriptModule {
                specifier: "zeroship:fixture/adapter",
                source: "export {};",
            },
        ],
        vec![],
        "duplicate plugin module",
    );
    assert_rejected(
        &[JavaScriptModule {
            specifier: "zeroship:other/adapter",
            source: "export {};",
        }],
        vec![],
        "must provide modules under",
    );
    assert_rejected(
        &[JavaScriptModule {
            specifier: "zeroship:fixture/../other",
            source: "export {};",
        }],
        vec![],
        "invalid plugin module specifier",
    );
}
