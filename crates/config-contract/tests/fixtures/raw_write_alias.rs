// The write-side twin of `raw_read_alias.rs`: a process-environment MUTATION
// that no compiler pass in this workspace will ever see, because the cfg is
// false in every build. `tests/clippy_gate.sh` therefore cannot report it, and
// a scan of tracked SOURCE is the only thing that can.
//
// Two violations, by construction: the import binds `std::env::set_var` under
// another name, and the call site uses that name. `PLANTED_VIOLATIONS` in
// `crates/config-contract/src/main.rs` counts both.
#[cfg(any())]
fn hidden_behind_disabled_cfg(key: &str, value: &str) {
    use std::env::set_var as hidden_write;

    hidden_write(key, value);
}
