// zeroship native C++ extensions
// ==================================
//
// These functions move hot-path work (Request construction, Response
// inspection) from rusty_v8 into a single C++ call per request. The
// motivation: `obj.set(scope, key, val)` from Rust costs ~50-80ns per
// call via the rusty_v8 FFI wrapper, while the same work in C++ inlines
// into a handful of cycles. For a Request-shaped object with ~10 field
// sets plus the Headers._map build, that's 500-1500ns per request we
// shift off the hot path.
//
// Conventions:
//   - All functions are `extern "C"` so Rust can call them directly.
//   - V8 objects cross the boundary as raw `void*`. We use the same
//     reinterpret_cast trick rusty_v8 uses internally (support.h
//     `ptr_to_local` / `local_to_ptr`): `v8::Local<T>` is layout-identical
//     to `T*`, so a reinterpret_cast is ABI-safe.
//   - The caller must hold a v8::HandleScope; we re-enter one for safety
//     but don't rely on creating any cross-scope Locals.
//   - String data comes in as raw (ptr, len) pairs pointing at Rust-owned
//     buffers that outlive the call.

#include "v8-isolate.h"
#include "v8-context.h"
#include "v8-local-handle.h"
#include "v8-object.h"
#include "v8-primitive.h"
#include "v8-container.h"
#include <cstring>
#include <cstdint>

// ---------------------------------------------------------------------------
// Local<->ptr helpers (equivalent to rusty_v8's support.h internals)
// ---------------------------------------------------------------------------

template <typename T>
static inline v8::Local<T> ptr_to_local(void* ptr) {
    static_assert(sizeof(v8::Local<T>) == sizeof(T*),
                  "v8::Local<T> must be layout-compatible with T*");
    T* typed = static_cast<T*>(ptr);
    return *reinterpret_cast<v8::Local<T>*>(&typed);
}

// ---------------------------------------------------------------------------
// POC functions (kept from the build-plumbing validation step)
// ---------------------------------------------------------------------------

extern "C" int zs_ext_poc_answer() {
    return 42;
}

extern "C" bool zs_ext_has_current_context(v8::Isolate* isolate) {
    if (isolate == nullptr) return false;
    v8::HandleScope scope(isolate);
    auto ctx = isolate->GetCurrentContext();
    return !ctx.IsEmpty();
}

// ---------------------------------------------------------------------------
// zs_build_request
// ---------------------------------------------------------------------------
//
// Builds a Request-shaped JS object directly in C++. Rust passes:
//   - `request_proto_ptr` / `headers_proto_ptr`: cached prototype Locals
//     (from globalThis.Request.prototype / globalThis.Headers.prototype)
//   - method / url / body as (ptr, len) pairs
//   - headers as 4 parallel arrays: name_ptrs, name_lens, value_ptrs, value_lens
//
// Returns the built Request as a raw `v8::Object*`. Rust converts back to
// `v8::Local<Object>` via `v8::Local::new(scope, global)` pattern — see
// the Rust wrapper.
//
// Steps inlined here that the rusty_v8 path does via separate FFI crossings:
//   1. v8::Object::New — Request
//   2. SetPrototype(Request.prototype)
//   3. v8::Object::New — Headers
//   4. SetPrototype(Headers.prototype)
//   5. v8::Object::New — _map (null prototype)
//   6. Per header: lowercase name, new String, new Array, set_index, set on _map
//   7. Set _map on Headers
//   8. Set url, method, headers, [_bodyText] on Request

extern "C" v8::Object* zs_build_request(
    void* request_proto_ptr,
    void* headers_proto_ptr,
    const char* method_ptr, size_t method_len,
    const char* url_ptr, size_t url_len,
    const char* const* header_name_ptrs,
    const size_t* header_name_lens,
    const char* const* header_value_ptrs,
    const size_t* header_value_lens,
    size_t header_count,
    const char* body_ptr, size_t body_len
) {
    // Caller (Rust) already holds an entered HandleScope on this thread.
    // We don't open a new one — any handles we create live in the caller's
    // scope, and the returned Local is valid there directly without Escape.
    v8::Isolate* isolate = v8::Isolate::GetCurrent();
    v8::Local<v8::Context> ctx = isolate->GetCurrentContext();

    v8::Local<v8::Object> request_proto = ptr_to_local<v8::Object>(request_proto_ptr);
    v8::Local<v8::Object> headers_proto = ptr_to_local<v8::Object>(headers_proto_ptr);

    // Pre-intern the property-name strings. kInternalized lets V8 compare
    // by identity rather than hashing + byte-comparing each time — cheap
    // once, wins on every property set/get.
    auto s = [&](const char* literal) -> v8::Local<v8::String> {
        return v8::String::NewFromUtf8(
            isolate, literal, v8::NewStringType::kInternalized
        ).ToLocalChecked();
    };
    v8::Local<v8::String> k_url = s("url");
    v8::Local<v8::String> k_method = s("method");
    v8::Local<v8::String> k_headers = s("headers");
    v8::Local<v8::String> k_map = s("_map");
    v8::Local<v8::String> k_bodyText = s("_bodyText");

    // --- Build _map (null-prototype object, name -> [value, ...]) ---
    v8::Local<v8::Object> map_obj = v8::Object::New(
        isolate, v8::Null(isolate),
        /* names */ nullptr, /* values */ nullptr, 0
    );

    for (size_t i = 0; i < header_count; i++) {
        // Lowercase the name bytes in-place in a small stack buffer. HTTP
        // header names are limited by our parser to 64 bytes; larger ones
        // fall back to a heap allocation.
        size_t n_len = header_name_lens[i];
        const char* n_ptr = header_name_ptrs[i];
        char stack_buf[64];
        char* lower_buf;
        bool heap_alloc = n_len > sizeof(stack_buf);
        if (heap_alloc) {
            lower_buf = new char[n_len];
        } else {
            lower_buf = stack_buf;
        }
        for (size_t j = 0; j < n_len; j++) {
            char c = n_ptr[j];
            lower_buf[j] = (c >= 'A' && c <= 'Z') ? (c + 32) : c;
        }

        v8::Local<v8::String> name_str = v8::String::NewFromUtf8(
            isolate, lower_buf, v8::NewStringType::kInternalized, n_len
        ).ToLocalChecked();
        if (heap_alloc) delete[] lower_buf;

        v8::Local<v8::String> value_str = v8::String::NewFromUtf8(
            isolate, header_value_ptrs[i], v8::NewStringType::kNormal, header_value_lens[i]
        ).ToLocalChecked();

        // Check if this name already has an entry (duplicate headers).
        // Common case: first occurrence → set fresh array. Rare: push
        // onto the existing array.
        v8::Local<v8::Value> existing;
        if (map_obj->Get(ctx, name_str).ToLocal(&existing) && existing->IsArray()) {
            v8::Local<v8::Array> arr = v8::Local<v8::Array>::Cast(existing);
            uint32_t len = arr->Length();
            arr->Set(ctx, len, value_str).Check();
        } else {
            v8::Local<v8::Array> arr = v8::Array::New(isolate, 1);
            arr->Set(ctx, 0, value_str).Check();
            map_obj->Set(ctx, name_str, arr).Check();
        }
    }

    // --- Build Headers ---
    v8::Local<v8::Name> headers_names[1] = { k_map };
    v8::Local<v8::Value> headers_values[1] = { map_obj };
    v8::Local<v8::Object> headers_obj = v8::Object::New(
        isolate, headers_proto, headers_names, headers_values, 1
    );

    // --- Build Request ---
    v8::Local<v8::String> url_val = v8::String::NewFromUtf8(
        isolate, url_ptr, v8::NewStringType::kNormal, url_len
    ).ToLocalChecked();
    v8::Local<v8::String> method_val = v8::String::NewFromUtf8(
        isolate, method_ptr, v8::NewStringType::kInternalized, method_len
    ).ToLocalChecked();

    // Does the HTTP method permit a body? Matches the JS helper.
    bool has_body = body_len > 0
        && !(method_len == 3 && memcmp(method_ptr, "GET", 3) == 0)
        && !(method_len == 4 && memcmp(method_ptr, "HEAD", 4) == 0);

    v8::Local<v8::Object> req;
    if (has_body) {
        v8::Local<v8::String> body_val = v8::String::NewFromUtf8(
            isolate, body_ptr, v8::NewStringType::kNormal, body_len
        ).ToLocalChecked();
        v8::Local<v8::Name> req_names[4] = { k_url, k_method, k_headers, k_bodyText };
        v8::Local<v8::Value> req_values[4] = { url_val, method_val, headers_obj, body_val };
        req = v8::Object::New(isolate, request_proto, req_names, req_values, 4);
    } else {
        v8::Local<v8::Name> req_names[3] = { k_url, k_method, k_headers };
        v8::Local<v8::Value> req_values[3] = { url_val, method_val, headers_obj };
        req = v8::Object::New(isolate, request_proto, req_names, req_values, 3);
    }
    // The Local lives in the caller's HandleScope — safe to return the
    // underlying pointer directly, no Escape needed.
    return *req;
}
