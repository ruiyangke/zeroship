//! V8 global bindings — console, setTimeout, clearTimeout, setInterval, clearInterval.
//!
//! Timer and console callbacks remain hand-written (they take V8 Function args
//! or are variadic). All other native APIs use `#[appbase_op]` in their own modules.

use std::cmp::Reverse;
use std::time::Duration;

use crate::event_loop::SharedState;
use crate::timers::{TimerCallback, TimerHeapEntry};

// ---------------------------------------------------------------------------
// Console polyfill (variadic — stays manual)
// ---------------------------------------------------------------------------

fn console_log_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let mut parts = Vec::new();
    for i in 0..args.length() {
        let arg = args.get(i);
        let s = arg.to_rust_string_lossy(scope);
        parts.push(s);
    }
    let line = parts.join(" ");
    println!("{}", line);

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
        .clone();
    let mut s = state.borrow_mut();
    s.log_buffer.push(line);
    if s.log_buffer.len() > 1000 {
        let drain = s.log_buffer.len() - 1000;
        s.log_buffer.drain(..drain);
    }
}

// ---------------------------------------------------------------------------
// Timer callbacks (take v8::Function args — stays manual)
// ---------------------------------------------------------------------------

fn set_timeout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
        .clone();

    let callback = match v8::Local::<v8::Function>::try_from(args.get(0)) {
        Ok(f) => f,
        Err(_) => {
            let msg = v8::String::new(scope, "setTimeout: first argument must be a function")
                .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let ms = if args.length() > 1 {
        args.get(1).uint32_value(scope).unwrap_or(0)
    } else {
        0
    };

    let global_cb = v8::Global::new(scope, callback);
    let mut s = state.borrow_mut();
    let id = s.timers.next_id;
    s.timers.next_id += 1;

    let delay = Duration::from_millis(u64::from(ms));

    s.timers.heap.push(Reverse(TimerHeapEntry {
        fire_at: std::time::Instant::now() + delay,
        id,
    }));
    s.timers.callbacks.insert(
        id,
        TimerCallback {
            callback: global_cb,
            interval: None,
        },
    );

    rv.set(v8::Integer::new(scope, id as i32).into());
}

fn clear_timeout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
        .clone();

    let id = if args.length() > 0 {
        args.get(0).uint32_value(scope).unwrap_or(0)
    } else {
        return;
    };

    state.borrow_mut().timers.callbacks.remove(&id);
}

fn set_interval_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
        .clone();

    let callback = match v8::Local::<v8::Function>::try_from(args.get(0)) {
        Ok(f) => f,
        Err(_) => {
            let msg = v8::String::new(scope, "setInterval: first argument must be a function")
                .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let ms = if args.length() > 1 {
        args.get(1).uint32_value(scope).unwrap_or(0)
    } else {
        0
    };
    let dur = Duration::from_millis(u64::from(ms));

    let global_cb = v8::Global::new(scope, callback);
    let mut s = state.borrow_mut();
    let id = s.timers.next_id;
    s.timers.next_id += 1;

    s.timers.heap.push(Reverse(TimerHeapEntry {
        fire_at: std::time::Instant::now() + dur,
        id,
    }));
    s.timers.callbacks.insert(
        id,
        TimerCallback {
            callback: global_cb,
            interval: Some(dur),
        },
    );

    rv.set(v8::Integer::new(scope, id as i32).into());
}

// ---------------------------------------------------------------------------
// Setup all globals on a V8 context
// ---------------------------------------------------------------------------

/// Install console, timers, fetch, URL, KV, crypto, env on the global object.
///
/// Callbacks from `#[appbase_op]` modules are referenced as `crate::{mod}::{fn}_callback`.
pub(crate) fn setup_globals(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);

    // console.log/warn/error/info
    {
        let console = v8::Object::new(scope);
        let log_fn = v8::Function::new(scope, console_log_callback).unwrap();
        let log_key = v8::String::new(scope, "log").unwrap();
        console.set(scope, log_key.into(), log_fn.into());

        let warn_key = v8::String::new(scope, "warn").unwrap();
        console.set(scope, warn_key.into(), log_fn.into());
        let error_key = v8::String::new(scope, "error").unwrap();
        console.set(scope, error_key.into(), log_fn.into());
        let info_key = v8::String::new(scope, "info").unwrap();
        console.set(scope, info_key.into(), log_fn.into());

        let console_key = v8::String::new(scope, "console").unwrap();
        global.set(scope, console_key.into(), console.into());
    }

    // setTimeout
    {
        let f = v8::Function::new(scope, set_timeout_callback).unwrap();
        let key = v8::String::new(scope, "setTimeout").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // clearTimeout
    {
        let f = v8::Function::new(scope, clear_timeout_callback).unwrap();
        let key = v8::String::new(scope, "clearTimeout").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // setInterval
    {
        let f = v8::Function::new(scope, set_interval_callback).unwrap();
        let key = v8::String::new(scope, "setInterval").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // clearInterval (same implementation as clearTimeout)
    {
        let f = v8::Function::new(scope, clear_timeout_callback).unwrap();
        let key = v8::String::new(scope, "clearInterval").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // __rawFetch (native HTTP fetch)
    {
        let f = v8::Function::new(scope, crate::fetch::raw_fetch_callback).unwrap();
        let key = v8::String::new(scope, "__rawFetch").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // __urlParse / __urlCanParse (native URL parser via ada-url)
    {
        let f = v8::Function::new(scope, crate::url::url_parse_callback).unwrap();
        let key = v8::String::new(scope, "__urlParse").unwrap();
        global.set(scope, key.into(), f.into());

        let f = v8::Function::new(scope, crate::url::url_can_parse_callback).unwrap();
        let key = v8::String::new(scope, "__urlCanParse").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // kv namespace
    {
        let kv = v8::Object::new(scope);

        let get_fn = v8::Function::new(scope, crate::kv::kv_get_callback).unwrap();
        let get_key = v8::String::new(scope, "get").unwrap();
        kv.set(scope, get_key.into(), get_fn.into());

        let set_fn = v8::Function::new(scope, crate::kv::kv_set_callback).unwrap();
        let set_key = v8::String::new(scope, "set").unwrap();
        kv.set(scope, set_key.into(), set_fn.into());

        let del_fn = v8::Function::new(scope, crate::kv::kv_delete_callback).unwrap();
        let del_key = v8::String::new(scope, "delete").unwrap();
        kv.set(scope, del_key.into(), del_fn.into());

        let list_fn = v8::Function::new(scope, crate::kv::kv_list_callback).unwrap();
        let list_key = v8::String::new(scope, "list").unwrap();
        kv.set(scope, list_key.into(), list_fn.into());

        let kv_key = v8::String::new(scope, "kv").unwrap();
        global.set(scope, kv_key.into(), kv.into());
    }

    // crypto namespace (randomUUID + native helpers for SubtleCrypto)
    {
        let crypto = v8::Object::new(scope);

        let uuid_fn = v8::Function::new(scope, crate::crypto::crypto_random_uuid_callback).unwrap();
        let uuid_key = v8::String::new(scope, "randomUUID").unwrap();
        crypto.set(scope, uuid_key.into(), uuid_fn.into());

        let grv_fn = v8::Function::new(scope, crate::crypto::crypto_get_random_values_callback).unwrap();
        let grv_key = v8::String::new(scope, "__cryptoGetRandomValues").unwrap();
        crypto.set(scope, grv_key.into(), grv_fn.into());

        let digest_fn = v8::Function::new(scope, crate::crypto::crypto_digest_callback).unwrap();
        let digest_key = v8::String::new(scope, "__cryptoDigest").unwrap();
        crypto.set(scope, digest_key.into(), digest_fn.into());

        let import_fn = v8::Function::new(scope, crate::crypto::crypto_import_key_callback).unwrap();
        let import_key = v8::String::new(scope, "__cryptoImportKey").unwrap();
        crypto.set(scope, import_key.into(), import_fn.into());

        let export_fn = v8::Function::new(scope, crate::crypto::crypto_export_key_callback).unwrap();
        let export_key = v8::String::new(scope, "__cryptoExportKey").unwrap();
        crypto.set(scope, export_key.into(), export_fn.into());

        let gen_fn = v8::Function::new(scope, crate::crypto::crypto_generate_key_callback).unwrap();
        let gen_key = v8::String::new(scope, "__cryptoGenerateKey").unwrap();
        crypto.set(scope, gen_key.into(), gen_fn.into());

        let sign_fn = v8::Function::new(scope, crate::crypto::crypto_sign_callback).unwrap();
        let sign_key = v8::String::new(scope, "__cryptoSign").unwrap();
        crypto.set(scope, sign_key.into(), sign_fn.into());

        let verify_fn = v8::Function::new(scope, crate::crypto::crypto_verify_callback).unwrap();
        let verify_key = v8::String::new(scope, "__cryptoVerify").unwrap();
        crypto.set(scope, verify_key.into(), verify_fn.into());

        let encrypt_fn = v8::Function::new(scope, crate::crypto::crypto_encrypt_callback).unwrap();
        let encrypt_key = v8::String::new(scope, "__cryptoEncrypt").unwrap();
        crypto.set(scope, encrypt_key.into(), encrypt_fn.into());

        let decrypt_fn = v8::Function::new(scope, crate::crypto::crypto_decrypt_callback).unwrap();
        let decrypt_key = v8::String::new(scope, "__cryptoDecrypt").unwrap();
        crypto.set(scope, decrypt_key.into(), decrypt_fn.into());

        let derive_bits_fn = v8::Function::new(scope, crate::crypto::crypto_derive_bits_callback).unwrap();
        let derive_bits_key = v8::String::new(scope, "__cryptoDeriveBits").unwrap();
        crypto.set(scope, derive_bits_key.into(), derive_bits_fn.into());

        let derive_key_fn = v8::Function::new(scope, crate::crypto::crypto_derive_key_callback).unwrap();
        let derive_key_key = v8::String::new(scope, "__cryptoDeriveKey").unwrap();
        crypto.set(scope, derive_key_key.into(), derive_key_fn.into());

        let crypto_key = v8::String::new(scope, "crypto").unwrap();
        global.set(scope, crypto_key.into(), crypto.into());
    }

    // env namespace
    {
        let env = v8::Object::new(scope);

        let get_fn = v8::Function::new(scope, crate::env::env_get_callback).unwrap();
        let get_key = v8::String::new(scope, "get").unwrap();
        env.set(scope, get_key.into(), get_fn.into());

        let env_key = v8::String::new(scope, "env").unwrap();
        global.set(scope, env_key.into(), env.into());
    }
}
