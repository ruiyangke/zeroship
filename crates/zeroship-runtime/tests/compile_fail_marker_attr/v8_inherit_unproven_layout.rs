//! Compile-fail: `#[v8_inherit(Base)]` whose derived state cannot be read
//! as the base's.
//!
//! `__zs_brand` brands a derived wrapper for the base state type too, so the
//! base class's callbacks read the derived box as a base box. The macro proves
//! that layout at compile time: `Plain` inherits a base with a non-zero-sized
//! state without naming a field that holds it, `Misplaced` names a field that
//! is not at offset zero, and `Boxed` and `Referenced` name a field at offset
//! zero that only points at a base state (a `Box`, a `&'static`), which deref
//! coercion would let a reference-typed check accept. All four are rejected.
#![expect(unused_imports, reason = "the fixture imports the inner v8 attribute macros that `#[v8_class]` consumes during expansion, so rustc cannot see them used; the stderr snapshot pins only the intended rejection")]

use zeroship_runtime_macros::{v8_class, v8_constructor, v8_inherit};

pub struct Base {
    pub count: u64,
}

#[v8_class]
impl Base {
    #[v8_constructor]
    fn new() -> Base {
        Base { count: 0 }
    }
}

pub struct Plain;

#[v8_class]
#[v8_inherit(Base)]
impl Plain {
    #[v8_constructor]
    fn new() -> Plain {
        Plain
    }
}

#[repr(C)]
pub struct Misplaced {
    pub tag: u64,
    pub base: Base,
}

#[v8_class]
#[v8_inherit(Base, state_field = base)]
impl Misplaced {
    #[v8_constructor]
    fn new() -> Misplaced {
        Misplaced { tag: 0, base: Base { count: 0 } }
    }
}

pub struct Boxed {
    pub base: Box<Base>,
}

#[v8_class]
#[v8_inherit(Base, state_field = base)]
impl Boxed {
    #[v8_constructor]
    fn new() -> Boxed {
        Boxed { base: Box::new(Base { count: 0 }) }
    }
}

pub struct Referenced {
    pub base: &'static Base,
}

#[v8_class]
#[v8_inherit(Base, state_field = base)]
impl Referenced {
    #[v8_constructor]
    fn new() -> Referenced {
        Referenced { base: Box::leak(Box::new(Base { count: 0 })) }
    }
}

fn main() {}
