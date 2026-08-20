//! The Web Platform Tests target for `zeroship-runtime`.
//!
//! The 23 modules below were 23 separate integration executables, each
//! statically linking V8. They are merged here for the same reason as
//! `tests/main.rs` (see its header for the measurement), and kept OUT of
//! `main.rs` for one reason:
//!
//! every module here `include_str!`s files under `tests/wpt/`, which is not
//! tracked in git. `tests/setup-wpt.sh` shallow-clones a pinned WPT commit
//! (~930 MB working tree) on demand and `.gitignore` keeps it out of the
//! index. On a checkout that has not run that script these modules do not
//! COMPILE - `include_str!` fails at macro expansion, not at run time. Folding
//! them into `main.rs` would mean a fresh clone could not build a single
//! runtime integration test. Here it means it cannot build this one target.
//!
//! WHAT THIS DOES NOT PROTECT AGAINST
//! ----------------------------------
//! The 23 modules now share one process, one V8 initialisation and one
//! environment. `wpt_fetch_abort`, `wpt_fetch_basic_network` and
//! `wpt_fetch_redirect` each set `ZEROSHIP_DEV=1` and never unset it, so from
//! the first of them to run, every module here sees dev mode - which, in
//! `transport/ssrf.rs`, means loopback stops being blocked. Nothing here
//! restores the variable, so the value cannot flip back mid-test; a module
//! added later that does restore it would break the ones above, and belongs
//! in its own target instead (`Cargo.toml` has five such entries already).
//!
//! Each module builds its own isolate per WPT file, so isolate state is not
//! shared - but the V8 flags and the platform are, and the first `init_v8()`
//! in the process fixes both for all of them.
//!
//! `Cargo.toml` sets `autotests = false`: a new `tests/wpt_*.rs` is compiled
//! by nothing until it is listed below.

mod wpt_abort;
mod wpt_blob;
mod wpt_event_target;
mod wpt_fetch_abort;
mod wpt_fetch_basic_network;
mod wpt_fetch_basic;
mod wpt_fetch_body;
mod wpt_fetch_redirect;
mod wpt_fetch_request;
mod wpt_fetch_response;
mod wpt_form_data;
mod wpt_headers;
mod wpt_streams_async_iter;
mod wpt_streams_byob;
mod wpt_streams_piping;
mod wpt_streams_readable;
mod wpt_streams_tee;
mod wpt_streams_transform;
mod wpt_streams_writable;
mod wpt_text_encoding;
mod wpt_text_encoding_streams;
mod wpt_url;
mod wpt_webcrypto;
