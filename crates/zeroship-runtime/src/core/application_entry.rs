//! Native entry snapshots retain callable identity and HTTP receivers.

use crate::rpc::dispatch::ProcedureRegistry;

#[derive(Clone)]
pub(crate) struct HttpHandler {
    function: v8::Global<v8::Function>,
    receiver: v8::Global<v8::Value>,
}

impl HttpHandler {
    pub(crate) fn locals<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> (v8::Local<'s, v8::Function>, v8::Local<'s, v8::Value>) {
        (
            v8::Local::new(scope, &self.function),
            v8::Local::new(scope, &self.receiver),
        )
    }
}

pub(crate) struct ApplicationEntry {
    pub fetch: Option<HttpHandler>,
    pub fetch_fast: Option<HttpHandler>,
    pub rpc: Option<ProcedureRegistry>,
}

impl ApplicationEntry {
    /// Capture all fields before publication. A getter or malformed procedure
    /// can reject a replacement without changing an existing snapshot.
    pub(crate) fn capture(
        scope: &mut v8::PinScope,
        value: v8::Local<v8::Value>,
        receiver: Option<v8::Local<v8::Value>>,
    ) -> Result<Self, String> {
        if value.is_null_or_undefined() {
            return Ok(Self {
                fetch: None,
                fetch_fast: None,
                rpc: None,
            });
        }
        if !value.is_object() || value.is_array() || value.is_function() {
            return Err("application entry must be an object".into());
        }
        let object = v8::Local::<v8::Object>::try_from(value).unwrap();
        let receiver = receiver.unwrap_or(value);
        let fetch = capture_http_handler(scope, object, "fetch", receiver)?;
        let fetch_fast = capture_http_handler(scope, object, "fetchFast", receiver)?;
        let rpc = read_field(scope, object, "rpc")?;
        let rpc = if rpc.is_null_or_undefined() {
            None
        } else {
            Some(ProcedureRegistry::snapshot(scope, rpc).map_err(|error| error.describe(scope))?)
        };
        Ok(Self {
            fetch,
            fetch_fast,
            rpc,
        })
    }
}

fn capture_http_handler(
    scope: &mut v8::PinScope,
    object: v8::Local<v8::Object>,
    name: &str,
    receiver: v8::Local<v8::Value>,
) -> Result<Option<HttpHandler>, String> {
    let value = read_field(scope, object, name)?;
    if value.is_null_or_undefined() {
        return Ok(None);
    }
    let function = v8::Local::<v8::Function>::try_from(value)
        .map_err(|_| format!("application entry {name} must be a function"))?;
    Ok(Some(HttpHandler {
        function: v8::Global::new(scope, function),
        receiver: v8::Global::new(scope, receiver),
    }))
}

pub(crate) fn read_field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    object: v8::Local<v8::Object>,
    name: &str,
) -> Result<v8::Local<'s, v8::Value>, String> {
    v8::tc_scope!(let tc, scope);
    let key = v8::String::new(tc, name).ok_or("could not allocate entry field name")?;
    object.get(tc, key.into()).ok_or_else(|| {
        tc.exception().map_or_else(
            || format!("could not read application entry {name}"),
            |error| crate::core::modules::error_detail(tc, error),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Runtime;

    fn expression<'s>(scope: &mut v8::PinScope<'s, '_>, source: &str) -> v8::Local<'s, v8::Value> {
        let source = v8::String::new(scope, source).unwrap();
        v8::Script::compile(scope, source, None)
            .unwrap()
            .run(scope)
            .unwrap()
    }

    #[test]
    fn snapshot_keeps_original_functions_and_an_explicit_receiver() {
        let runtime = Runtime::builder().build();
        let entry = runtime.with_scope(|scope| {
            let value = expression(
                scope,
                r#"
                globalThis.owner = {tag: 'original'};
                globalThis.targets = {
                    async fetch() { await Promise.resolve(); return this.tag; },
                    fetchFast() { return this.tag; },
                    rpc: {'named procedure': () => 'original'},
                };
                targets
            "#,
            );
            let receiver = expression(scope, "owner");
            ApplicationEntry::capture(scope, value, Some(receiver)).unwrap()
        });
        runtime.with_scope(|scope| {
            expression(
                scope,
                r#"
                owner = {tag: 'replacement'};
                targets.fetch = () => 'replacement';
                targets.fetchFast = () => 'replacement';
                targets.rpc['named procedure'] = () => 'replacement';
            "#,
            );
        });
        let promise = runtime.with_scope(|scope| {
            let (function, receiver) = entry.fetch.as_ref().unwrap().locals(scope);
            let promise = function.call(scope, receiver, &[]).unwrap();
            v8::Global::new(scope, v8::Local::<v8::Promise>::try_from(promise).unwrap())
        });
        runtime.with_scope(|scope| {
            let promise = v8::Local::new(scope, &promise);
            assert_eq!(promise.state(), v8::PromiseState::Fulfilled);
            assert_eq!(
                promise.result(scope).to_rust_string_lossy(scope),
                "original"
            );
            let (function, receiver) = entry.fetch_fast.as_ref().unwrap().locals(scope);
            assert_eq!(
                function
                    .call(scope, receiver, &[])
                    .unwrap()
                    .to_rust_string_lossy(scope),
                "original"
            );
        });
        runtime.with_scope(|scope| {
            let undefined = v8::undefined(scope).into();
            let mut call = crate::rpc::dispatch::RpcCall::new(
                scope,
                entry.rpc.as_ref().unwrap().clone(),
                "named procedure".into(),
                undefined,
                undefined,
            );
            match call.poll(scope) {
                Ok(crate::rpc::dispatch::CallProgress::Complete { value, .. }) => {
                    let value = v8::Local::new(scope, value);
                    assert_eq!(value.to_rust_string_lossy(scope), "original");
                }
                _ => panic!("captured procedure must complete synchronously"),
            }
        });
    }

    #[test]
    fn malformed_replacements_fail_without_changing_the_captured_entry() {
        let runtime = Runtime::builder().build();
        let entry = runtime.with_scope(|scope| {
            let value = expression(
                scope,
                "({tag: 'valid', fetch() {return this.tag;}, rpc: {valid() {}}})",
            );
            ApplicationEntry::capture(scope, value, None).unwrap()
        });
        runtime.with_scope(|scope| {
            for source in [
                "[]",
                "42",
                "({fetch: true})",
                "({fetchFast: 'invalid'})",
                "({rpc() {}})",
            ] {
                let value = expression(scope, source);
                assert!(
                    ApplicationEntry::capture(scope, value, None).is_err(),
                    "{source}"
                );
            }
            let value = expression(
                scope,
                "({get rpc() {throw new Error('replacement getter failed');}})",
            );
            let error = ApplicationEntry::capture(scope, value, None).err().unwrap();
            assert!(error.contains("replacement getter failed"), "{error}");
            let (function, receiver) = entry.fetch.as_ref().unwrap().locals(scope);
            assert_eq!(
                function
                    .call(scope, receiver, &[])
                    .unwrap()
                    .to_rust_string_lossy(scope),
                "valid"
            );
            let value = expression(scope, "({rpc: {replacement() {}}})");
            assert!(ApplicationEntry::capture(scope, value, None).is_ok());
        });
    }
}
