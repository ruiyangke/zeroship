//! Native-state brands: which Rust type a wrapper's internal field holds.
//!
//! Every native class keeps its Rust state behind a raw pointer in internal
//! field 0 of its JS wrappers, and casting that pointer is sound only when the
//! wrapper really is one of that class's own. Script decides what reaches a
//! callback: any object can be a method's receiver (`Foo.prototype.m.call(x)`)
//! or a method's argument, and `Object.setPrototypeOf` rewires any prototype
//! chain. So neither an object's shape ("field 0 holds an External") nor its
//! prototype says what the field points at, and a check built on either reads
//! one class's memory as another's.
//!
//! A brand says it. Each state type `T` has one private symbol per isolate,
//! minted here and reachable only through this module, and every wrapper whose
//! field 0 holds a `T` carries that symbol with the same `External` as its
//! value. Script can neither read, write nor copy a private symbol, so
//! [`state`] yields a pointer only for an object this runtime branded as a `T`
//! wrapper, and only the pointer the brand was minted with: a later write of
//! another class's state into the same field does not match the brand.
//!
//! A class that inherits another's layout (the base state as its first field,
//! at offset zero, or a zero-sized base state) brands its wrappers for the base
//! state type too, which is what lets the base class's callbacks accept them.
//! `#[v8_inherit]` proves that layout at compile time through [`ClassState`].

use std::marker::PhantomData;
use std::ptr::NonNull;

/// The native state type the wrappers of a class hold.
///
/// Every `#[v8_class]` implements it, and so does a hand-rolled class a
/// `#[v8_class]` inherits from: `#[v8_inherit(Base)]` reads the base's state
/// type through it to prove, at compile time, that a derived state can be read
/// as the base's.
pub trait ClassState {
    /// The state type in the class's internal field 0.
    type State: 'static;

    /// The internal fields every wrapper of the class has: 1, or 2 when the
    /// class or any class it inherits from has a fastcall method. A derived
    /// wrapper passes its base's `v8::Signature`, so a base's fastcall shim
    /// reads slot 1 of it, and the derived class has the slot whether or not
    /// it declares a fastcall method itself.
    const FIELD_COUNT: usize;
}

/// The isolate's brand key for state type `T`. Generic over `T` so every state
/// type gets its own slot, and so its own key.
struct BrandKey<T>(v8::Eternal<v8::Private>, PhantomData<fn() -> T>);

/// The private symbol that brands `T` wrappers in this isolate, minted on
/// first use. `Private::new` (not the name-keyed `Private::for_api`) so no
/// other native code can reach the key by guessing its name either.
fn key<'s, T: 'static>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Private> {
    if let Some(slot) = scope.get_slot::<BrandKey<T>>()
        && let Some(key) = slot.0.get(scope)
    {
        return key;
    }
    let key = v8::Private::new(scope, None);
    let eternal = v8::Eternal::empty();
    eternal.set(scope, key);
    scope.set_slot(BrandKey::<T>(eternal, PhantomData));
    key
}

/// Brand `obj` as a wrapper of `T` whose internal field 0 holds `state`. Call
/// it right after storing `state` in the field, once per state type the
/// wrapper may be read as.
pub fn mark<T: 'static>(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    state: v8::Local<v8::External>,
) {
    let key = key::<T>(scope);
    obj.set_private(scope, key, state.into());
}

/// Box `state` into a wrapper and brand it as a `T` wrapper.
///
/// The box goes into internal field 0 of `obj` and is dropped when V8
/// collects `obj` (or disposes the isolate). Returns the field's
/// `External`, for callers that brand the wrapper for a base state type too.
pub fn wrap<'s, T: 'static>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    state: T,
) -> v8::Local<'s, v8::External> {
    let raw = Box::into_raw(Box::new(state));
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw.cast());
    obj.set_internal_field(0, ext.into());
    mark::<T>(scope, obj, ext);
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || {
            // SAFETY: `raw_addr` came from `Box::into_raw` of a `Box<T>`
            // above, and this finalizer is the only code that frees it,
            // once, when V8 reclaims `obj`.
            drop(unsafe { Box::from_raw(raw_addr as *mut T) });
        }),
    );
    // Dropping the handle would cancel the finalizer; the guaranteed
    // variant fires on collection or isolate disposal either way.
    std::mem::forget(weak);
    ext
}

/// The `T` state behind `obj`, or `None` when `obj` is not a wrapper this
/// runtime branded as holding a `T`.
pub fn state<T: 'static>(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> Option<NonNull<T>> {
    let field = obj.get_internal_field(scope, 0)?;
    let field = v8::Local::<v8::External>::try_from(field).ok()?;
    let ptr = NonNull::new(field.value().cast::<T>())?;
    let key = key::<T>(scope);
    let brand = obj.get_private(scope, key)?;
    let brand = v8::Local::<v8::External>::try_from(brand).ok()?;
    (brand.value() == field.value()).then_some(ptr)
}

/// Whether `obj` is a wrapper this runtime branded as holding a `T`.
pub fn is<T: 'static>(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    state::<T>(scope, obj).is_some()
}

/// [`state`] for a value that may not be an object at all.
pub fn value_state<T: 'static>(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Option<NonNull<T>> {
    let obj = v8::Local::<v8::Object>::try_from(value).ok()?;
    state::<T>(scope, obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two state types, and one value of each for an `External` to point at.
    struct First;
    struct Second;
    static FIRST: u8 = 1;
    static SECOND: u8 = 2;

    fn external<'s>(scope: &v8::PinScope<'s, '_>, target: &'static u8) -> v8::Local<'s, v8::External> {
        v8::External::new(scope, std::ptr::from_ref(target).cast_mut().cast())
    }

    /// The brand is what makes a wrapper readable as a state type: an
    /// `External` in field 0 alone is not enough, a brand for one type does
    /// not admit another, and once field 0 is overwritten (what a constructor
    /// run on an existing wrapper would do) the old brand does not match it.
    #[test]
    fn a_brand_names_its_type_and_the_pointer_it_was_minted_with() {
        crate::init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handles, &mut isolate);
        let context = v8::Context::new(handles, v8::ContextOptions::default());
        let scope = &mut v8::ContextScope::new(handles, context);
        let template = v8::ObjectTemplate::new(scope);
        template.set_internal_field_count(1);
        let wrapper = template.new_instance(scope).expect("wrapper allocates");
        let first = external(scope, &FIRST);
        let second = external(scope, &SECOND);

        wrapper.set_internal_field(0, first.into());
        assert!(state::<First>(scope, wrapper).is_none(), "an unbranded field is not a First");

        mark::<First>(scope, wrapper, first);
        let found = state::<First>(scope, wrapper).expect("a branded First reads back");
        assert_eq!(found.as_ptr().cast::<u8>().cast_const(), std::ptr::from_ref(&FIRST));
        assert!(state::<Second>(scope, wrapper).is_none(), "a First brand does not admit a Second");

        wrapper.set_internal_field(0, second.into());
        assert!(
            state::<First>(scope, wrapper).is_none(),
            "a First brand does not vouch for another pointer stored over it",
        );

        let plain = v8::Object::new(scope);
        mark::<First>(scope, plain, first);
        assert!(state::<First>(scope, plain).is_none(), "an object without internal fields is no wrapper");
    }
}
