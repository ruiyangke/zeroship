//! Compile-fail: `post_init = ident` (not a string literal).
//!
//! Expected error: the `extract_post_init` parser rejects non-string
//! values via `syn::Error::to_compile_error()` with the message:
//!   "post_init must be a string literal naming a function on this
//!    impl, e.g. post_init = \"after_install\""
//! (design §5.7 case 3). Strict by design — silent no-op would be
//! a debugging nightmare for a semantically load-bearing attribute.
#![allow(unused_imports)]

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::{v8_class, v8_constructor};

pub struct Bare;

#[v8_class]
impl Bare {
    // Wrong: post_init must take a string literal value.
    #[v8_constructor(post_init = after_install)]
    fn new() -> Bare {
        Bare
    }

    pub(crate) fn after_install(
        _scope: &mut v8::PinScope,
        _this: v8::Local<v8::Object>,
    ) -> Result<(), OpError> {
        Ok(())
    }
}

fn main() {}
