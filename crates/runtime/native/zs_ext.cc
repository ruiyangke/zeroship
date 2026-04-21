// POC — C++ functions called from Rust.
//
// Phase 1: trivial int return (verify cc-crate plumbing)                   — OK
// Phase 2: include V8 headers (verify V8 compile environment)              — OK
// Phase 3: call V8 API (verify linkage against librusty_v8.a)              — testing

#include "v8-isolate.h"
#include "v8-context.h"
#include "v8-local-handle.h"

extern "C" int zs_ext_poc_answer() {
    return 42;
}

// Calls a real V8 method — forces the linker to resolve V8 symbols from the
// rusty_v8 static lib that the `v8` crate brings in. If the binary links,
// our build pipeline is complete.
extern "C" bool zs_ext_has_current_context(v8::Isolate* isolate) {
    if (isolate == nullptr) return false;
    // Caller must already be on the isolate thread with an entered context
    // (the zeroship runtime always is when dispatching a request).
    v8::HandleScope scope(isolate);
    auto ctx = isolate->GetCurrentContext();
    return !ctx.IsEmpty();
}
