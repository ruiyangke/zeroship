/// Construct native records and arrays. Expression values use the native Serde
/// adapter; Rust model mapping uses `EncodeRecord` to move owned fields.
#[macro_export]
macro_rules! value {
    (@items $out:ident;) => {};
    (@items $out:ident; $key:tt : null $(, $($rest:tt)*)?) => {
        $out.insert(($key).into(), $crate::value::Value::Null);
        $crate::value!(@items $out; $($($rest)*)?);
    };
    (@items $out:ident; $key:tt : {$($v:tt)*} $(, $($rest:tt)*)?) => {
        $out.insert(($key).into(), $crate::value!({$($v)*}));
        $crate::value!(@items $out; $($($rest)*)?);
    };
    (@items $out:ident; $key:tt : [$($v:tt)*] $(, $($rest:tt)*)?) => {
        $out.insert(($key).into(), $crate::value!([$($v)*]));
        $crate::value!(@items $out; $($($rest)*)?);
    };
    (@items $out:ident; $key:tt : $v:expr $(, $($rest:tt)*)?) => {
        $out.insert(($key).into(), $crate::value!($v));
        $crate::value!(@items $out; $($($rest)*)?);
    };
    (@array [$($values:expr,)*];) => { vec![$($values,)*] };
    (@array [$($values:expr,)*]; null $(, $($rest:tt)*)?) => {
        $crate::value!(@array [$($values,)* $crate::value::Value::Null,]; $($($rest)*)?)
    };
    (@array [$($values:expr,)*]; {$($v:tt)*} $(, $($rest:tt)*)?) => {
        $crate::value!(@array [$($values,)* $crate::value!({$($v)*}),]; $($($rest)*)?)
    };
    (@array [$($values:expr,)*]; [$($v:tt)*] $(, $($rest:tt)*)?) => {
        $crate::value!(@array [$($values,)* $crate::value!([$($v)*]),]; $($($rest)*)?)
    };
    (@array [$($values:expr,)*]; $v:expr $(, $($rest:tt)*)?) => {
        $crate::value!(@array [$($values,)* $crate::value!($v),]; $($($rest)*)?)
    };
    (null) => { $crate::value::Value::Null };
    ({}) => { $crate::value::Value::Object($crate::value::Record::new()) };
    ({$($v:tt)+}) => {{
        let mut fields = $crate::value::Record::new();
        $crate::value!(@items fields; $($v)+);
        $crate::value::Value::Object(fields)
    }};
    ([$($v:tt)*]) => { $crate::value::Value::Array($crate::value!(@array []; $($v)*)) };
    ($v:expr) => { $crate::value::to_value(&$v).expect("native value encoding") };
}
