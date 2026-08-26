#[cfg(any())]
fn hidden_behind_disabled_cfg(key: &str) -> Option<String> {
    use std::env::var as hidden_read;

    hidden_read(key).ok()
}
