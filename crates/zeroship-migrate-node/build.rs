// napi-rs build shim. `napi_build::setup()` emits the cdylib link arguments that
// let the addon reference the Node/Bun N-API symbols (resolved at `.node` load
// time), so the same object loads under Node and Bun. It emits only linker args —
// it needs no Node headers, so it is a no-op-safe call for a plain `cargo test`
// build of the crate's pure-Rust integration suite as well.
fn main() {
    napi_build::setup();
}
