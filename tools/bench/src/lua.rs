use mlua::prelude::*;

use crate::config::Config;
use crate::stats::Summary;

/// wrk-compatible Lua scripting bridge.
///
/// Supports the standard wrk callback API:
/// - `request()` → return custom HTTP request bytes, or nil to use default
/// - `response(status, headers, body)` → process each response
/// - `done(summary)` → called once after benchmark completes
///
/// Also sets up the `wrk` global table with method/path/headers/body fields
/// and a `wrk.format()` helper that builds the raw HTTP request from the
/// current wrk state (mirrors wrk behavior).
pub struct LuaScript {
    lua: Lua,
    has_request: bool,
    has_response: bool,
    has_done: bool,
}

impl LuaScript {
    /// Load and execute a Lua script file, returning the configured bridge.
    pub fn load(path: &str, config: &Config) -> Result<Self, String> {
        let lua = Lua::new();

        // Set up the `wrk` global table to match wrk's API.
        setup_wrk_global(&lua, config)
            .map_err(|e| format!("failed to set up wrk global: {e}"))?;

        // Execute the script file.
        let source = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read script {path}: {e}"))?;
        lua.load(&source)
            .exec()
            .map_err(|e| format!("failed to execute script {path}: {e}"))?;

        let globals = lua.globals();
        let has_request = globals.get::<LuaFunction>("request").is_ok();
        let has_response = globals.get::<LuaFunction>("response").is_ok();
        let has_done = globals.get::<LuaFunction>("done").is_ok();

        Ok(Self { lua, has_request, has_response, has_done })
    }

    /// Call `request()` → returns custom HTTP request bytes, or None to use default.
    pub fn call_request(&self) -> Option<Vec<u8>> {
        if !self.has_request {
            return None;
        }
        let globals = self.lua.globals();
        let func: LuaFunction = globals.get("request").ok()?;
        match func.call::<LuaValue>(()) {
            Ok(LuaValue::String(s)) => Some(s.as_bytes().to_vec()),
            Ok(LuaValue::Nil) => None,
            Ok(_) => None,
            Err(e) => {
                eprintln!("[lua] request() error: {e}");
                None
            }
        }
    }

    /// Call `response(status, headers, body)` — fires after each response.
    pub fn call_response(&self, status: u16, headers: &str, body: &str) {
        if !self.has_response {
            return;
        }
        let globals = self.lua.globals();
        if let Ok(func) = globals.get::<LuaFunction>("response") {
            if let Err(e) = func.call::<()>((status, headers, body)) {
                eprintln!("[lua] response() error: {e}");
            }
        }
    }

    /// Call `done(summary)` — fires once after the benchmark completes.
    /// Passes a Lua table with the key summary fields.
    pub fn call_done(&self, summary: &Summary) {
        if !self.has_done {
            return;
        }
        let globals = self.lua.globals();
        let Ok(func) = globals.get::<LuaFunction>("done") else { return };

        let table = match self.lua.create_table() {
            Ok(t) => t,
            Err(e) => { eprintln!("[lua] done() table error: {e}"); return; }
        };

        let secs = summary.duration.as_secs_f64();
        let _ = table.set("requests", summary.requests);
        let _ = table.set("bytes", summary.bytes);
        let _ = table.set("duration_ms", summary.duration.as_millis() as u64);
        let _ = table.set("requests_per_sec", if secs > 0.0 { summary.requests as f64 / secs } else { 0.0 });
        let _ = table.set("bytes_per_sec", if secs > 0.0 { summary.bytes as f64 / secs } else { 0.0 });
        let _ = table.set("latency_avg_us", summary.latency.mean());
        let _ = table.set("latency_max_us", summary.latency.max());
        let _ = table.set("latency_p50_us", summary.latency.value_at_percentile(50.0));
        let _ = table.set("latency_p99_us", summary.latency.value_at_percentile(99.0));
        let _ = table.set("errors_connect", summary.errors_connect);
        let _ = table.set("errors_read", summary.errors_read);
        let _ = table.set("errors_write", summary.errors_write);
        let _ = table.set("errors_timeout", summary.errors_timeout);
        let _ = table.set("errors_status", summary.errors_status);
        let _ = table.set("errors_total", summary.total_errors());

        if let Err(e) = func.call::<()>(table) {
            eprintln!("[lua] done() error: {e}");
        }
    }

    pub fn has_request(&self) -> bool { self.has_request }
    pub fn has_response(&self) -> bool { self.has_response }

    /// Build the default HTTP request from the `wrk` global table.
    ///
    /// After the script runs, `wrk.method`/`wrk.body`/`wrk.headers` may have
    /// been mutated (e.g. `wrk.method = "POST"` in the common wrk idiom).
    /// Read them back and produce the final request bytes — otherwise these
    /// assignments are silently ignored and the benchmark sends the config's
    /// original method/path, which is almost never what the user intended.
    pub fn build_request_from_wrk(&self, config: &Config) -> Vec<u8> {
        let globals = self.lua.globals();
        let Ok(wrk): LuaResult<LuaTable> = globals.get("wrk") else {
            return crate::http::build_request(config);
        };
        let method: String = wrk.get("method").unwrap_or_else(|_| config.method.clone());
        let path: String = wrk.get("path").unwrap_or_else(|_| config.path.clone());
        let host: String = wrk.get("host").unwrap_or_else(|_| config.host.clone());
        let port: u16 = wrk.get("port").unwrap_or(config.port);
        let body: String = wrk.get("body").unwrap_or_default();

        let mut req = format!("{method} {path} HTTP/1.1\r\n");
        req.push_str(&format!("Host: {host}:{port}\r\n"));
        req.push_str("Connection: keep-alive\r\n");

        if let Ok(headers) = wrk.get::<LuaTable>("headers") {
            for pair in headers.pairs::<String, String>() {
                if let Ok((k, v)) = pair {
                    req.push_str(&format!("{k}: {v}\r\n"));
                }
            }
        }

        if !body.is_empty() {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
            req.push_str("\r\n");
            req.push_str(&body);
        } else {
            req.push_str("\r\n");
        }

        req.into_bytes()
    }
}

/// Set up the `wrk` global table in the Lua state, matching wrk's API surface.
fn setup_wrk_global(lua: &Lua, config: &Config) -> LuaResult<()> {
    let wrk = lua.create_table()?;

    wrk.set("method", config.method.as_str())?;
    wrk.set("path", config.path.as_str())?;
    wrk.set("body", config.body.as_deref().unwrap_or(""))?;
    wrk.set("scheme", "http")?;
    wrk.set("host", config.host.as_str())?;
    wrk.set("port", config.port)?;

    // headers table — { ["Name"] = "Value", ... }
    let headers_tbl = lua.create_table()?;
    for (k, v) in &config.headers {
        headers_tbl.set(k.as_str(), v.as_str())?;
    }
    wrk.set("headers", headers_tbl)?;

    // wrk.thread — basic thread info table (thread id set to 0 for main context)
    let thread_tbl = lua.create_table()?;
    thread_tbl.set("id", 0u32)?;
    wrk.set("thread", thread_tbl)?;

    // wrk.format() — build an HTTP/1.1 request string from current wrk state.
    // This mirrors wrk's built-in helper that scripts can call to get the default
    // request bytes, then modify headers/body before returning.
    let wrk_ref = wrk.clone();
    let format_fn = lua.create_function(move |lua_ctx, ()| {
        let method: String = wrk_ref.get("method").unwrap_or_else(|_| "GET".into());
        let path: String = wrk_ref.get("path").unwrap_or_else(|_| "/".into());
        let host: String = wrk_ref.get("host").unwrap_or_default();
        let port: u16 = wrk_ref.get("port").unwrap_or(80);
        let body: String = wrk_ref.get("body").unwrap_or_default();
        let headers: LuaTable = wrk_ref.get("headers").unwrap_or_else(|_| {
            lua_ctx.create_table().unwrap()
        });

        let mut req = format!("{method} {path} HTTP/1.1\r\n");
        req.push_str(&format!("Host: {host}:{port}\r\n"));
        req.push_str("Connection: keep-alive\r\n");

        for pair in headers.pairs::<String, String>() {
            if let Ok((k, v)) = pair {
                req.push_str(&format!("{k}: {v}\r\n"));
            }
        }

        if !body.is_empty() {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
            req.push_str("\r\n");
            req.push_str(&body);
        } else {
            req.push_str("\r\n");
        }

        Ok(req)
    })?;
    wrk.set("format", format_fn)?;

    lua.globals().set("wrk", wrk)?;
    Ok(())
}
