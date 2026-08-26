//! V8 authoring — run the STANDALONE `zero-migrate` v1 recorder inside
//! zeroship-runtime's V8 isolate to author a `.ts` migration into its
//! `ir_version:1` IR envelope. This is the production port of the S2
//! `author_v1_envelope` test mechanism (`tests/author_and_apply_pg.rs`): identical
//! module graph, identical recorder seam, no Node in the loop.

use zeroship_runtime::{ModuleEntry, Runtime};

/// The authoring glue (imports the migration + the recorder seam, runs `up()`,
/// emits the v1 envelope on `globalThis.__zsPlatformIR`). Shared verbatim with the
/// S2 test glue.
const RECORDER_GLUE_JS: &str = include_str!("recorder_glue.js");

/// The `zero-migrate` engine's OWN recorder bundle — the CURRENT v1 DSL + recorder
/// (`table()`/`t.*`/`role`/`grant`/`createFunction`/… → `__begin`/`__drain`).
/// Mapping `@zeroship/migrate` to THIS file is what makes the authored envelope v1.
///
/// This is the ENGINE's recorder (vendored in the submodule), NOT the monorepo's
/// `sdks/migrate` copy. Recorder and IR parser ship from the same package, so a
/// lexicon the monorepo bundle has not yet followed cannot produce IR the engine
/// rejects at parse time. It is also fully self-contained (no
/// `@zeroship/db`/`zeroship` external imports), so the authoring module graph
/// needs no extra stubs.
///
/// PROVENANCE, not a guarantee. This said "guaranteed in lockstep", which is
/// stronger than anything checked here. What is actually measured:
///
///   * this file is tracked in the submodule at 136 KB and `dist/` is not
///     gitignored there, so it is the real build output rather than a stub
///     (verified 2026-08-08);
///   * this repo's only exercise of the production `author_v1_envelope` path is
///     a single sample migration with two ops, in
///     `tests/author_and_apply_pg.rs`;
///   * the engine's own dedicated recorder coverage is FOUR tests
///     (`packages/zero-migrate/tests/recorder.test.ts`), reported by that
///     project on 2026-08-08 and NOT re-run here - this repo's CI does not
///     build the submodule's JS suites.
///
/// So the same-package argument is sound and the word "guaranteed" was not.
/// Drift between the recorder and the parser would be caught by four tests in a
/// suite we do not run, plus a two-op smoke test here.
const STANDALONE_RECORDER_JS: &str =
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packages/zero-migrate/dist/embedded-recorder.js"
    ));

/// The deserialized adapter result mirroring the JSON the glue emits.
#[derive(serde::Deserialize)]
struct AuthoredEnvelope {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    ir: Option<serde_json::Value>,
}

/// Author a `.ts` migration module source into its `ir_version:1` IR envelope JSON
/// by running the STANDALONE recorder in zeroship-runtime's V8 isolate.
///
/// `name` is the filename-derived fallback used when the module declares none.
/// Returns the envelope JSON string (`{ ir_version:1, name, ops }`) on success, or
/// a human message on any authoring failure (recorder error, missing `up()`, a V8
/// setup fault).
pub fn author_v1_envelope(migration_source: &str, name: &str) -> Result<String, String> {
    zeroship_runtime::init_v8();

    let modules = vec![
        ModuleEntry {
            specifier: "recorder_glue.js".to_string(),
            source: RECORDER_GLUE_JS.to_string(),
        },
        ModuleEntry {
            specifier: "./__migration__.js".to_string(),
            source: migration_source.to_string(),
        },
        ModuleEntry {
            specifier: "@zeroship/migrate".to_string(),
            source: STANDALONE_RECORDER_JS.to_string(),
        },
    ];

    let runtime = Runtime::builder().build();

    let authored: Result<String, String> = runtime.with_scope(|scope| {
        zeroship_runtime::init::setup_globals(scope)?;
        zeroship_runtime::init::install_text_encoding_streams(scope);

        {
            let global = scope.get_current_context().global(scope);
            let k = v8::String::new(scope, "__zsMigrationName")
                .ok_or("alloc __zsMigrationName key")?;
            let v = v8::String::new(scope, name).ok_or("alloc __zsMigrationName value")?;
            global.set(scope, k.into(), v.into());
        }

        zeroship_runtime::modules::load_modules(scope, &modules)?;
        scope.perform_microtask_checkpoint();

        let global = scope.get_current_context().global(scope);
        let k = v8::String::new(scope, "__zsPlatformIR").ok_or("alloc __zsPlatformIR key")?;
        let v = global
            .get(scope, k.into())
            .filter(|v| v.is_string())
            .ok_or("glue left no __zsPlatformIR string")?;
        Ok(v.to_rust_string_lossy(scope))
    });

    let ir_json = authored?;
    let envelope: AuthoredEnvelope = serde_json::from_str(&ir_json)
        .map_err(|e| format!("authored envelope JSON does not parse: {e}"))?;
    if !envelope.ok {
        return Err(envelope
            .error
            .unwrap_or_else(|| "recorder reported an authoring error".to_string()));
    }
    let ir = envelope
        .ir
        .ok_or("successful authoring carries no ir")?;
    serde_json::to_string(&ir).map_err(|e| format!("re-serialize authored ir: {e}"))
}
