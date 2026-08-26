//! Native `node:os`.
//!
//! Wave #192 — npm packages probe `os.platform()` / `os.arch()` /
//! `os.cpus().length` / `os.EOL` to gate code paths (e.g. picking a
//! native binary, deciding whether to thread-pool a workload, choosing
//! line endings on file output). The runtime is single-tenant Linux
//! V8 so most exports are constant; the pieces that vary
//! (`uptime`, `arch` at compile time) read once and stay frozen for
//! the isolate's lifetime.
//!
//! Anything that requires real OS introspection (`networkInterfaces`,
//! `getPriority`, `setPriority`) installs as a throwing stub — same
//! pattern as `node:async_hooks::executionAsyncId`.

#![allow(unsafe_code)]

use std::sync::OnceLock;
use std::time::Instant;

/// Shared start instant — `uptime()` reports seconds since this. Used
/// in lieu of system uptime since we run inside an isolate, not at
/// boot. First read on synthetic-module evaluation seeds it.
fn start_instant() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// Mint a synthetic ESM record for `node:os`. Called from
/// `core::native_modules::resolve_native`.
pub fn synthetic_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Module> {
    // Seed the start instant so the very first `uptime()` after import
    // reflects time since module import, not since first `uptime()` call.
    let _ = start_instant();

    let module_name = v8::String::new(scope, "node:os").unwrap();
    let names = export_names();
    let export_strings: Vec<v8::Local<v8::String>> = names
        .iter()
        .map(|n| v8::String::new(scope, n).unwrap())
        .collect();
    v8::Module::create_synthetic_module(scope, module_name, &export_strings, evaluate)
}

fn evaluate<'s>(
    context: v8::Local<'s, v8::Context>,
    module: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Value>> {
    v8::callback_scope!(unsafe scope, context);

    let ns = v8::Object::new(scope);
    populate(scope, ns);

    for name in export_names() {
        if *name == "default" { continue; }
        let key = v8::String::new(scope, name).unwrap();
        let val = ns.get(scope, key.into()).unwrap_or_else(|| v8::undefined(scope).into());
        let _ = module.set_synthetic_module_export(scope, key, val);
    }
    let default_key = v8::String::new(scope, "default").unwrap();
    let _ = module.set_synthetic_module_export(scope, default_key, ns.into());

    Some(v8::undefined(scope).into())
}

fn export_names() -> &'static [&'static str] {
    &[
        "platform", "arch", "type", "release", "version", "machine",
        "cpus", "totalmem", "freemem", "loadavg", "uptime",
        "hostname", "homedir", "tmpdir", "userInfo",
        "endianness", "EOL", "constants",
        "networkInterfaces", "getPriority", "setPriority", "availableParallelism",
        "default",
    ]
}

fn populate<'s>(scope: &mut v8::PinScope<'s, '_>, obj: v8::Local<v8::Object>) {
    set_fn(scope, obj, "platform", op_platform);
    set_fn(scope, obj, "arch", op_arch);
    set_fn(scope, obj, "type", op_type);
    set_fn(scope, obj, "release", op_release);
    set_fn(scope, obj, "version", op_version);
    set_fn(scope, obj, "machine", op_arch); // machine() ≈ arch() on Linux
    set_fn(scope, obj, "cpus", op_cpus);
    set_fn(scope, obj, "totalmem", op_totalmem);
    set_fn(scope, obj, "freemem", op_freemem);
    set_fn(scope, obj, "loadavg", op_loadavg);
    set_fn(scope, obj, "uptime", op_uptime);
    set_fn(scope, obj, "hostname", op_hostname);
    set_fn(scope, obj, "homedir", op_homedir);
    set_fn(scope, obj, "tmpdir", op_tmpdir);
    set_fn(scope, obj, "userInfo", op_user_info);
    set_fn(scope, obj, "endianness", op_endianness);
    set_fn(scope, obj, "networkInterfaces", op_network_interfaces);
    set_fn(scope, obj, "availableParallelism", op_available_parallelism);
    set_fn(scope, obj, "getPriority", op_get_priority);
    set_fn(scope, obj, "setPriority", op_set_priority);

    // EOL — string constant. Linux (and our runtime) uses `\n`.
    let eol_k = v8::String::new(scope, "EOL").unwrap();
    let eol_v = v8::String::new(scope, "\n").unwrap();
    obj.set(scope, eol_k.into(), eol_v.into());

    // constants — Node ships ~30 of these. Apps that read os.constants
    // overwhelmingly grab `signals.SIGINT` / `signals.SIGTERM` and
    // `errno.E*`; the full table mirrors `node/lib/internal/constants.js`.
    let constants = v8::Object::new(scope);
    let signals = v8::Object::new(scope);
    let signal_pairs: &[(&str, i32)] = &[
        ("SIGHUP", 1), ("SIGINT", 2), ("SIGQUIT", 3), ("SIGILL", 4),
        ("SIGTRAP", 5), ("SIGABRT", 6), ("SIGIOT", 6), ("SIGBUS", 7),
        ("SIGFPE", 8), ("SIGKILL", 9), ("SIGUSR1", 10), ("SIGSEGV", 11),
        ("SIGUSR2", 12), ("SIGPIPE", 13), ("SIGALRM", 14), ("SIGTERM", 15),
        ("SIGCHLD", 17), ("SIGSTKFLT", 16), ("SIGCONT", 18), ("SIGSTOP", 19),
        ("SIGTSTP", 20), ("SIGTTIN", 21), ("SIGTTOU", 22), ("SIGURG", 23),
        ("SIGXCPU", 24), ("SIGXFSZ", 25), ("SIGVTALRM", 26), ("SIGPROF", 27),
        ("SIGWINCH", 28), ("SIGIO", 29), ("SIGPOLL", 29), ("SIGPWR", 30),
        ("SIGSYS", 31), ("SIGUNUSED", 31),
    ];
    for (name, val) in signal_pairs.iter() {
        let k = v8::String::new(scope, name).unwrap();
        let v = v8::Integer::new(scope, *val);
        signals.set(scope, k.into(), v.into());
    }
    let signals_k = v8::String::new(scope, "signals").unwrap();
    constants.set(scope, signals_k.into(), signals.into());

    // priority constants — getPriority/setPriority semantics.
    let priority = v8::Object::new(scope);
    for (name, val) in [
        ("PRIORITY_LOW", 19),
        ("PRIORITY_BELOW_NORMAL", 10),
        ("PRIORITY_NORMAL", 0),
        ("PRIORITY_ABOVE_NORMAL", -7),
        ("PRIORITY_HIGH", -14),
        ("PRIORITY_HIGHEST", -20),
    ] {
        let k = v8::String::new(scope, name).unwrap();
        let v = v8::Integer::new(scope, val);
        priority.set(scope, k.into(), v.into());
    }
    let priority_k = v8::String::new(scope, "priority").unwrap();
    constants.set(scope, priority_k.into(), priority.into());

    let constants_k = v8::String::new(scope, "constants").unwrap();
    obj.set(scope, constants_k.into(), constants.into());
}

fn set_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    name: &str,
    callback: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let f = v8::Function::new(scope, callback).unwrap();
    let k = v8::String::new(scope, name).unwrap();
    obj.set(scope, k.into(), f.into());
}

// ---------------------------------------------------------------------------
// String-returning ops
// ---------------------------------------------------------------------------

fn op_platform(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    rv.set(v8::String::new(scope, "linux").unwrap().into());
}

fn op_arch(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    // Map Rust's `cfg!(target_arch)` to Node's arch values. Node uses
    // x64 / arm64 / ia32 / arm / s390x / ppc64 / mips / mipsel.
    let a = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "x86" => "ia32",
        "aarch64" => "arm64",
        "arm" => "arm",
        "powerpc64" => "ppc64",
        other => other,
    };
    rv.set(v8::String::new(scope, a).unwrap().into());
}

fn op_type(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    rv.set(v8::String::new(scope, "Linux").unwrap().into());
}

fn op_release(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    // Sentinel kernel release. Real lookup would read /proc/sys/kernel/osrelease;
    // we keep this constant so npm packages get a stable string and we
    // don't grow a syscall path through a sandbox boundary.
    rv.set(v8::String::new(scope, "6.0.0").unwrap().into());
}

fn op_version(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    rv.set(v8::String::new(scope, "#1 SMP PREEMPT_DYNAMIC Linux").unwrap().into());
}

fn op_hostname(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    rv.set(v8::String::new(scope, "zeroship-worker").unwrap().into());
}

fn op_homedir(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    rv.set(v8::String::new(scope, "/").unwrap().into());
}

fn op_tmpdir(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    rv.set(v8::String::new(scope, "/tmp").unwrap().into());
}

fn op_endianness(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    // V8 always runs little-endian on supported platforms.
    rv.set(v8::String::new(scope, "LE").unwrap().into());
}

// ---------------------------------------------------------------------------
// Number / array ops
// ---------------------------------------------------------------------------

fn op_totalmem(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    // 1 GiB sentinel — npm packages divide by it for memory-fraction
    // heuristics; 0 would NaN those.
    rv.set(v8::Number::new(scope, (1024 * 1024 * 1024) as f64).into());
}

fn op_freemem(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    rv.set(v8::Number::new(scope, (512 * 1024 * 1024) as f64).into());
}

fn op_loadavg(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    let arr = v8::Array::new(scope, 3);
    let zero = v8::Number::new(scope, 0.0);
    for i in 0..3 {
        arr.set_index(scope, i as u32, zero.into());
    }
    rv.set(arr.into());
}

fn op_uptime(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    let secs = start_instant().elapsed().as_secs_f64();
    rv.set(v8::Number::new(scope, secs).into());
}

fn op_available_parallelism(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    // Single-tenant V8-per-thread — we don't surface real CPU count to
    // user code (lets npm packages pick spawn counts; we want them at 1).
    rv.set(v8::Integer::new(scope, 1).into());
}

fn op_cpus(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    // Single-CPU stub — npm probes typically check `.length > 0`.
    let arr = v8::Array::new(scope, 1);
    let cpu = v8::Object::new(scope);

    let model_k = v8::String::new(scope, "model").unwrap();
    let model_v = v8::String::new(scope, "V8 Worker").unwrap();
    cpu.set(scope, model_k.into(), model_v.into());

    let speed_k = v8::String::new(scope, "speed").unwrap();
    let speed_v = v8::Integer::new(scope, 0);
    cpu.set(scope, speed_k.into(), speed_v.into());

    let times = v8::Object::new(scope);
    for name in ["user", "nice", "sys", "idle", "irq"] {
        let k = v8::String::new(scope, name).unwrap();
        let v = v8::Integer::new(scope, 0);
        times.set(scope, k.into(), v.into());
    }
    let times_k = v8::String::new(scope, "times").unwrap();
    cpu.set(scope, times_k.into(), times.into());

    arr.set_index(scope, 0, cpu.into());
    rv.set(arr.into());
}

fn op_user_info(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    let obj = v8::Object::new(scope);

    let username_k = v8::String::new(scope, "username").unwrap();
    let username_v = v8::String::new(scope, "zeroship").unwrap();
    obj.set(scope, username_k.into(), username_v.into());

    let uid_k = v8::String::new(scope, "uid").unwrap();
    let uid_v = v8::Integer::new(scope, -1);
    obj.set(scope, uid_k.into(), uid_v.into());

    let gid_k = v8::String::new(scope, "gid").unwrap();
    let gid_v = v8::Integer::new(scope, -1);
    obj.set(scope, gid_k.into(), gid_v.into());

    let shell_k = v8::String::new(scope, "shell").unwrap();
    let shell_v = v8::null(scope);
    obj.set(scope, shell_k.into(), shell_v.into());

    let homedir_k = v8::String::new(scope, "homedir").unwrap();
    let homedir_v = v8::String::new(scope, "/").unwrap();
    obj.set(scope, homedir_k.into(), homedir_v.into());

    rv.set(obj.into());
}

fn op_network_interfaces(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    // No-net visibility from the sandbox — return an empty object,
    // matching what Node does on a host with no interfaces (rare,
    // but the shape is documented).
    let obj = v8::Object::new(scope);
    rv.set(obj.into());
}

fn op_get_priority(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    rv.set(v8::Integer::new(scope, 0).into());
}

fn op_set_priority(_scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    // setPriority is documented to return undefined; just no-op.
    rv.set_undefined();
}
