//! Plugin interface for the zeroship runtime.
//!
//! Plugins extend the runtime with new capabilities (db, kv, auth, storage, etc.)
//! Each plugin provides:
//! - **JS bridge**: JavaScript code defining the user-facing API (e.g., `globalThis.db`)
//! - **Meter resources**: Declared metered resources for billing/enforcement
//! - **State init**: Per-isolate state injection (e.g., database connections)
//!
//! # Lifecycle
//!
//! 1. Plugin is constructed with its config (e.g., `DbPlugin::new("path.db")`)
//! 2. `configure()` is called to validate the config
//! 3. When an isolate is created:
//!    a. `js_bridge()` is injected into the global scope
//!    b. `init()` is called with a `PluginContext` to set up per-isolate state
//! 4. User code runs, calling the plugin's JS API

use std::path::Path;

/// How values combine across events / billing periods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregation {
    Sum,
    Max,
    Latest,
    Gauge,
}

/// Describes a metered resource declared by a plugin.
#[derive(Debug, Clone)]
pub struct MeterResource {
    pub name: String,
    pub unit: String,
    pub aggregation: Aggregation,
    pub category: String,
}

/// Trait for recording usage from plugins. Implemented by CounterRegistry in the metering crate.
///
/// Uses name-based lookup (HashMap, ~30ns overhead) which is fine for plugin ops
/// that do I/O (DB queries, KV operations). The core runtime uses `ResourceHandle`
/// directly for hot-path resources (requests, cpu_us).
pub trait PluginMeter: Send + Sync {
    fn increment(&self, resource_name: &str, delta: u64);
}

/// No-op meter for testing or when metering is disabled.
pub struct NoopMeter;

impl PluginMeter for NoopMeter {
    fn increment(&self, _resource_name: &str, _delta: u64) {}
}

/// Quota check result — returned when a resource is over quota.
#[derive(Debug, Clone)]
pub struct QuotaDenied {
    pub resource: String,
    pub used: u64,
    pub limit: u64,
    pub message: String,
}

impl std::fmt::Display for QuotaDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for QuotaDenied {}

/// Quota checker for plugin ops. Check BEFORE consuming a resource.
pub trait PluginQuota: Send + Sync {
    /// Check if a resource can be consumed. Returns Err if over quota.
    fn check(&self, resource: &str) -> Result<(), QuotaDenied>;
}

/// No-op quota checker — always allows. For testing / when quotas are disabled.
pub struct NoopQuota;
impl PluginQuota for NoopQuota {
    fn check(&self, _resource: &str) -> Result<(), QuotaDenied> {
        Ok(())
    }
}

/// Context provided to plugins during isolate initialization.
///
/// Gives plugins access to the app identity, data directory,
/// and metering/quota interfaces.
#[allow(clippy::missing_debug_implementations)]
pub struct PluginContext<'a> {
    /// Meter for recording plugin resource usage (db ops, kv ops, etc.).
    pub meter: &'a dyn PluginMeter,
    /// Quota checker for point-of-use enforcement (db ops, kv ops, etc.).
    pub quota: &'a dyn PluginQuota,
    /// Base directory for this app's data (e.g., databases, files).
    pub data_dir: &'a Path,
}

/// The core plugin interface.
///
/// All platform primitives (db, kv, auth, storage, etc.) implement this trait.
/// Plugins must be `Send + Sync` because:
/// - `Send`: passed across thread boundaries (isolates run on dedicated threads)
/// - `Sync`: the plugin factory may be shared across threads
pub trait Plugin: Send + Sync {
    /// Unique name for this plugin (e.g., "db", "kv", "auth").
    /// Used in config files and logging.
    fn name(&self) -> &str;

    /// JavaScript code injected into the V8 global scope.
    /// Typically defines a global object that wraps the raw ops
    /// into a user-friendly API.
    ///
    /// Must be valid 7-bit ASCII (V8 extension requirement).
    fn js_bridge(&self) -> &str;

    /// Initialize per-isolate state.
    /// Called once per isolate, before any user code runs.
    ///
    /// Use `ctx.data_dir` for file-based storage paths.
    fn init(&self, ctx: &mut PluginContext<'_>);

    /// Declare metered resources this plugin tracks.
    /// Called once at startup during registry building.
    fn meter_resources(&self) -> Vec<MeterResource> {
        vec![]
    }
}

/// Factory that creates plugin instances for a given app.
///
/// Called each time a new isolate is spawned. The factory receives the app ID
/// and returns a fresh set of plugins configured for that app.
///
/// This is `Arc`-shared across threads because multiple isolate threads
/// may need to create plugins concurrently.
pub type PluginFactory = std::sync::Arc<dyn Fn(&str) -> Vec<Box<dyn Plugin>> + Send + Sync>;

