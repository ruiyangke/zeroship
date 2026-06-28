//! `#[v8_class]` host object for `node:net.Socket`.
//!
//! The JS-facing `Socket` is an EventEmitter facade in `net.js`. This
//! native class is the thin kernel handle: it owns a socket id and
//! forwards connect/read/write control into [`super::state`].

use crate::state::{OpError, SharedState};
use super::connect::{authorize_connect, capability_violation, ConnectKind};
#[cfg(feature = "runtime_tls")]
use super::connect::{authorize_start_tls, validate_tls_policy};
#[allow(unused_imports)]
use zeroship_runtime_macros::{
    v8_class, v8_constructor, v8_getter, v8_method, v8_name, v8_to_string_tag,
};

pub struct NativeSocket {
    socket_id: u32,
    state: SharedState,
}

impl Drop for NativeSocket {
    fn drop(&mut self) {
        super::state::free_native_socket_state(&self.state, self.socket_id);
    }
}

#[v8_class]
#[v8_to_string_tag = "NativeSocket"]
impl NativeSocket {
    #[v8_constructor]
    fn new(scope: &mut v8::PinScope) -> Result<Self, OpError> {
        let state = scope
            .get_slot::<SharedState>()
            .ok_or_else(|| OpError::error("NativeSocket: runtime state missing"))?
            .clone();
        let socket_id = super::state::alloc_native_socket_id(&state)
            .map_err(|e| OpError::node("EMFILE", e))?;
        Ok(Self { socket_id, state })
    }

    #[v8_method]
    fn attach(
        &self,
        scope: &mut v8::PinScope,
        wrapper: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        let wrapper = v8::Local::<v8::Object>::try_from(wrapper)
            .map_err(|_| OpError::type_error("NativeSocket.attach expects an object"))?;
        super::state::attach_wrapper(
            &self.state,
            self.socket_id,
            v8::Global::new(scope, wrapper),
        );
        Ok(())
    }

    #[v8_method]
    fn connect(
        &self,
        scope: &mut v8::PinScope,
        host: String,
        port: u32,
    ) -> Result<(), OpError> {
        let target = authorize_connect(
            scope,
            &self.state,
            self.socket_id,
            host,
            port,
            ConnectKind::Net,
        )?;
        super::state::reserve_socket_slot(&self.state, self.socket_id).map_err(|e| {
            if e.contains("cap") {
                OpError::node("EMFILE", e)
            } else {
                capability_violation(e)
            }
        })?;
        super::connect::spawn_connect_task(self.state.clone(), self.socket_id, target);
        Ok(())
    }

    #[cfg(feature = "runtime_tls")]
    #[v8_method]
    #[v8_name = "validateTls"]
    fn validate_tls(&self, reject_unauthorized: bool) -> Result<(), OpError> {
        validate_tls_policy(reject_unauthorized)
    }

    #[cfg(feature = "runtime_tls")]
    #[v8_method]
    #[v8_name = "connectTls"]
    fn connect_tls(
        &self,
        scope: &mut v8::PinScope,
        host: String,
        port: u32,
        servername: String,
        reject_unauthorized: bool,
        ca_pem: Option<String>,
        verify_identity: bool,
    ) -> Result<(), OpError> {
        let target = authorize_connect(
            scope,
            &self.state,
            self.socket_id,
            host,
            port,
            ConnectKind::Tls {
                reject_unauthorized,
            },
        )?;
        super::state::reserve_socket_slot(&self.state, self.socket_id).map_err(|e| {
            if e.contains("cap") {
                OpError::node("EMFILE", e)
            } else {
                capability_violation(e)
            }
        })?;
        super::connect::spawn_tls_connect_task(
            self.state.clone(),
            self.socket_id,
            target,
            super::state::TlsOptions {
                servername,
                reject_unauthorized,
                ca_pem,
                verify_identity,
            },
        );
        Ok(())
    }

    #[cfg(feature = "runtime_tls")]
    #[v8_method]
    #[v8_name = "startTls"]
    fn start_tls(
        &self,
        scope: &mut v8::PinScope,
        servername: String,
        reject_unauthorized: bool,
        ca_pem: Option<String>,
        verify_identity: bool,
    ) -> Result<(), OpError> {
        authorize_start_tls(scope, reject_unauthorized)?;
        super::state::queue_start_tls(
            &self.state,
            self.socket_id,
            super::state::TlsOptions {
                servername,
                reject_unauthorized,
                ca_pem,
                verify_identity,
            },
        )
        .map_err(|e| OpError::node("ERR_TLS_HANDSHAKE", e))
    }

    #[v8_method]
    fn write(
        &self,
        scope: &mut v8::PinScope,
        data: v8::Local<v8::Value>,
        encoding: Option<String>,
    ) -> Result<bool, OpError> {
        let bytes = crate::node::buffer::extract_input(scope, data, encoding.as_deref())?;
        super::state::queue_write(&self.state, self.socket_id, bytes)
            .map_err(|e| OpError::node("ERR_SOCKET_CLOSED", e))
    }

    #[v8_method]
    fn end(&self) -> Result<(), OpError> {
        super::state::queue_end(&self.state, self.socket_id)
            .map_err(|e| OpError::node("ERR_SOCKET_CLOSED", e))
    }

    #[v8_method]
    fn destroy(&self) {
        super::state::destroy_socket(&self.state, self.socket_id);
    }

    #[v8_method]
    fn pause(&self) {
        super::state::pause_socket(&self.state, self.socket_id);
    }

    #[v8_method]
    fn resume(&self) {
        super::state::resume_socket(&self.state, self.socket_id);
    }

    #[v8_method]
    #[v8_name = "setNoDelay"]
    fn set_no_delay(
        &self,
        scope: &mut v8::PinScope,
        on: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        let on = if on.is_undefined() {
            true
        } else {
            on.boolean_value(scope)
        };
        super::state::set_no_delay(&self.state, self.socket_id, on)
            .map_err(|e| OpError::node("ERR_SOCKET_BAD_OPTION", e.to_string()))
    }

    #[v8_method]
    #[v8_name = "setKeepAlive"]
    fn set_keep_alive(
        &self,
        scope: &mut v8::PinScope,
        on: v8::Local<v8::Value>,
        initial_delay: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        let on = if on.is_undefined() {
            true
        } else {
            on.boolean_value(scope)
        };
        let initial_delay = if initial_delay.is_undefined() {
            0
        } else {
            initial_delay.uint32_value(scope).unwrap_or(0)
        };
        super::state::set_keep_alive(
            &self.state,
            self.socket_id,
            on,
            initial_delay as u64,
        )
        .map_err(|e| OpError::node("ERR_SOCKET_BAD_OPTION", e.to_string()))
    }

    #[v8_getter]
    #[v8_name = "remoteAddress"]
    fn remote_address(&self) -> Option<String> {
        self.state
            .borrow()
            .native_sockets
            .get(&self.socket_id)
            .and_then(|s| s.borrow().remote.map(|a| a.ip().to_string()))
    }

    #[v8_getter]
    #[v8_name = "remotePort"]
    fn remote_port(&self) -> Option<u32> {
        self.state
            .borrow()
            .native_sockets
            .get(&self.socket_id)
            .and_then(|s| s.borrow().remote.map(|a| a.port() as u32))
    }

    #[v8_getter]
    #[v8_name = "bytesRead"]
    fn bytes_read(&self) -> u32 {
        self.state
            .borrow()
            .native_sockets
            .get(&self.socket_id)
            .map(|s| s.borrow().bytes_read.min(u32::MAX as u64) as u32)
            .unwrap_or(0)
    }

    #[v8_getter]
    #[v8_name = "bytesWritten"]
    fn bytes_written(&self) -> u32 {
        self.state
            .borrow()
            .native_sockets
            .get(&self.socket_id)
            .map(|s| s.borrow().bytes_written.min(u32::MAX as u64) as u32)
            .unwrap_or(0)
    }

    #[v8_getter]
    fn encrypted(&self) -> bool {
        self.state
            .borrow()
            .native_sockets
            .get(&self.socket_id)
            .map(|s| s.borrow().encrypted)
            .unwrap_or(false)
    }
}

pub fn install_native<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Option<v8::Local<'s, v8::Function>> {
    let tmpl = NativeSocket::install(scope);
    let ctor = tmpl.get_function(scope)?;
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "__zsNativeSocket").unwrap();
    global.set(scope, key.into(), ctor.into());
    Some(ctor)
}
