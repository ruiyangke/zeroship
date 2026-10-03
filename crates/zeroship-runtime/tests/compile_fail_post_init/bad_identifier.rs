//! Compile-fail: `post_init = "1bad"` — the value parses as a string
//! literal but isn't a valid Rust identifier.
//!
//! Expected error: the `extract_post_init` parser rejects via
//! `syn::Error::to_compile_error()` with the message:
//!   "post_init = \"1bad\" is not a valid Rust identifier"
//! (design §5.7 case 4). Span lands on the string literal.
#![expect(unused_imports, reason = "the fixture imports the inner v8 attribute macros that `#[v8_class]` consumes during expansion, so rustc cannot see them used; the stderr snapshot pins only the intended rejection")]

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::{v8_class, v8_constructor};

pub struct Bare;

#[v8_class]
impl Bare {
    #[v8_constructor(post_init = "1bad")]
    fn new() -> Bare {
        Bare
    }
}

fn main() {}
