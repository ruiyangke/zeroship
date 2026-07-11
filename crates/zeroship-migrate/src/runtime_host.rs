//! The V8-host seam for the migrate engine — extraction Phase A.
//!
//! This module captures the *entire* coupling of the migrate engine to
//! `zeroship-runtime` behind three engine-owned traits — [`AuthoringHost`],
//! [`RecorderPlatform`], and [`JsDriverHost`] — plus the small vendored types the
//! trait signatures name ([`ModuleEntry`], [`GlobalsProfile`], [`NetPolicy`],
//! [`HostPort`], [`HostEvalError`], [`JsDriverTimeout`]). It is the exact analogue
//! of the shipped [`PgSession`](crate::postgres) seam.
//!
//! **The load-bearing shape decision (design §2.4): the traits name NO
//! `zeroship-runtime` / `v8` type.** Every symbol these signatures mention is
//! engine-owned or `std`. That is what makes `cargo tree -p zeroship-migrate`
//! carry no `zeroship-runtime` / `v8` node — the traits compile *unconditionally,
//! with zero runtime dep*, and the in-Rust-V8 impls live only in the monorepo
//! adapter crate `zeroship-migrate-runtime`.
//!
//! The traits move *transport to V8* (evaluate a module graph + read back string
//! globals; open/command/close a long-lived JS-driver isolate), NOT *migrate logic
//! to the runtime*. The module-graph content, the `__zs*` global protocol, the
//! envelope/owner-stamp parsing, the command protocol/serde, all SQL, the TLS/
//! net-policy security logic, and the entire kernel sandbox stay engine-owned.

use std::collections::HashMap;
use std::time::Duration;

// The reviewed-allowlist security type is vendored into the engine (extraction
// Phase B — `crate::net_policy`, a byte-identical copy of `zeroship_core::
// net_policy`), so the engine carries no `zeroship-core` normal-graph dep. The
// engine's `NetPolicy` construction + TLS-pin validation stay engine-owned; the
// adapter maps `NetPolicy` → `zeroship_runtime::NetPolicy` at `open_js_driver`.
pub use crate::net_policy::{HostPort, ReviewedAllowlist};

/// An in-memory JS module (specifier → source). Vendored into the engine so the
/// trait signatures carry no `zeroship_runtime::ModuleEntry`. The adapter maps
/// `migrate::ModuleEntry` ↔ `zeroship_runtime::ModuleEntry` (identical fields).
#[derive(Debug, Clone)]
pub struct ModuleEntry {
    /// The module specifier (import string / graph key).
    pub specifier: String,
    /// The module source text.
    pub source: String,
}

/// The globals profile a front-end operation installs (engine-owned enum; the peer
/// of `embedding.rs`'s `FrontendGlobals`). Passed to [`AuthoringHost::eval_frontend`]
/// so the adapter installs the right `setup_globals` + `install_*` set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalsProfile {
    /// The record / determinism-lint profile (`install_text_encoding_streams`).
    Migration,
    /// The schema-eval profile (headers + native streams + blob + text-encoding +
    /// DOM).
    Schema,
}

/// Host-eval failure — STRUCTURED so the engine reconstructs its own
/// `RecordError::{V8, NoIr}` faithfully (byte-identical error fidelity, design
/// §2.2).
#[derive(Debug)]
pub enum HostEvalError {
    /// The globals-profile install (`setup_globals` + `install_*`) failed, OR
    /// `load_modules` failed. The engine maps this → `RecordError::V8`. Carries the
    /// runtime's error string.
    Install(String),
    /// A requested global could not be handled as a JS string — either an **input**
    /// `v8::String::new` alloc failed, or a requested **output** global was absent
    /// or non-string. The engine maps this → `RecordError::NoIr`. Carries the
    /// offending global name.
    MissingOutput(String),
}

impl std::fmt::Display for HostEvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostEvalError::Install(msg) => write!(f, "host eval install/load failed: {msg}"),
            HostEvalError::MissingOutput(name) => {
                write!(f, "host eval produced no `{name}` string global")
            }
        }
    }
}

impl std::error::Error for HostEvalError {}

/// The authoring capability: evaluate a migrate front-end module graph in a fresh
/// isolate/context, install a globals profile, inject string globals, run the graph
/// + drain microtasks, and read back named string globals.
///
/// This single method subsumes record / schema-eval / determinism-lint: they differ
/// only in `modules`, `inputs`, and `outputs`. Isolate construction, `with_scope`,
/// `load_modules`, `setup_globals` + `install_*`, and every raw `v8::` call live in
/// the adapter, never in the engine.
///
/// `#[allow(async_fn_in_trait)]` is not needed (the method is sync — V8 eval is
/// synchronous and microtasks drain in-scope), but the stack is `!Send` by design
/// (single-thread V8 runtime), mirroring `MigrationBackend`/`PgSession`.
pub trait AuthoringHost {
    /// Evaluate `modules` (first entry is the eager entrypoint) with `profile`
    /// installed. Before eval, set each `(name, value)` in `inputs` as a string
    /// global. After eval + microtask drain, read back each name in `outputs`,
    /// returning a `name → value` map.
    ///
    /// Install/load failure ⇒ `Err(HostEvalError::Install)`; a missing/non-string
    /// output global (or a failed input-string alloc) ⇒
    /// `Err(HostEvalError::MissingOutput(name))` — so the engine can map back to
    /// `RecordError::{V8, NoIr}` faithfully (design §2.2).
    ///
    /// `heap_limit_mb: None` = default heap (the record/lint path); `Some(mb)` = the
    /// recorder-child's bounded heap.
    fn eval_frontend(
        &self,
        modules: &[ModuleEntry],
        profile: GlobalsProfile,
        inputs: &[(&str, &str)],
        outputs: &[&str],
        heap_limit_mb: Option<u32>,
    ) -> Result<HashMap<String, String>, HostEvalError>;
}

/// The single-threaded platform init the recorder child needs BEFORE lockdown, plus
/// the fail-closed flavor read. Split from [`AuthoringHost`] because it must run
/// before the seccomp/landlock ruleset is applied, and the fail-closed assertion
/// depends on the process-global platform `Once` the runtime commits.
///
/// **CONTRACT INVARIANT (security-critical).** [`platform_flavor`](Self::platform_flavor)
/// MUST return the flavor committed by *this same impl's*
/// [`init_single_threaded`](Self::init_single_threaded) — the two methods read/write
/// the one process-global platform `Once`. The engine keeps the go/no-go
/// `flavor ∈ {0,1,2}` decision (`assert_recorder_single_threaded`); the adapter only
/// supplies the raw `u8` read.
pub trait RecorderPlatform {
    /// Commit the single-threaded V8 platform (`init_v8_single_threaded`).
    fn init_single_threaded(&self);
    /// Read the committed platform flavor (raw u8: 0=uninit, 1=multi, 2=single).
    /// MUST reflect the flavor this impl's `init_single_threaded()` committed.
    fn platform_flavor(&self) -> u8;
}

/// Reply-timeout marker (engine-owned). Signals ONLY "the driver reply was not
/// received within the session's `command_timeout`" — it deliberately carries **no
/// `command_id`**. The `command_id` is engine-owned state (`next_id`), so on a
/// `JsDriverTimeout` the engine reconstructs the full
/// `JsDriverError::TimedOut { command_id, timeout }` from its own `id` + the
/// session's timeout (design §2.3).
#[derive(Debug)]
pub struct JsDriverTimeout {
    /// The elapsed wait the host measured before giving up (== the session's
    /// `command_timeout`). The engine already owns `command_id`, so it is not here.
    pub elapsed: Duration,
}

/// Host a long-lived JS SQL-driver isolate (mysql2 over node:net) and exchange JSON
/// commands with it. Separate from [`AuthoringHost`]: an embedder that only authors
/// needn't provide a driver isolate.
///
/// The [`Session`](Self::Session) associated type is an opaque driver-isolate
/// handle. The engine NEVER sees `runtime.state()` / `js_driver` / `result_senders`
/// / `command_queue` — those V8-shaped internals are fully absorbed by the impl.
///
/// `#[allow(async_fn_in_trait)]` — `!Send` by design on the single-thread V8 runtime
/// (mirrors `MigrationBackend`/`PgSession`).
#[allow(async_fn_in_trait)]
pub trait JsDriverHost {
    /// Opaque driver-isolate handle (wraps the runtime + its pump in the adapter).
    type Session;

    /// Spin up a Trusted JS-driver isolate from `modules` (entry + vendored mysql2
    /// bundle) seeded with `dsn_json` + `net_policy`, start its pump, and initialize
    /// it (empty env). `command_timeout` bounds each reply wait.
    fn open_js_driver(
        &self,
        modules: &[ModuleEntry],
        net_policy: NetPolicy,
        dsn_json: String,
        command_timeout: Duration,
    ) -> Result<Self::Session, HostEvalError>;

    /// Register `id`'s reply sender, enqueue `payload`, wake the pump, and wait
    /// (under the session's command_timeout) for the JSON reply. `id` is the
    /// engine's allocated command id — the sole engine-owned value the host needs
    /// besides the opaque payload, because the adapter keys
    /// `result_senders.insert(id, tx)` with exactly the id the engine baked into
    /// `payload`'s `DriverCommand` JSON (so the adapter never re-parses the engine
    /// wire shape). On timeout the adapter tears the isolate down via
    /// [`close_js_driver`](Self::close_js_driver) and returns `Err(JsDriverTimeout)`;
    /// the engine then reconstructs `JsDriverError::TimedOut { command_id: id, .. }`.
    async fn js_driver_command(
        &self,
        session: &mut Self::Session,
        id: u64,
        payload: String,
    ) -> Result<String, JsDriverTimeout>;

    /// Tear down the isolate (drop the runtime). Called by the adapter internally on
    /// a command timeout AND by the engine's poison path (`poison_transport` /
    /// `poison_timeout`) — the engine keeps setting its own `poisoned` flag;
    /// `close_js_driver` only drops the host-owned isolate.
    fn close_js_driver(&self, session: &mut Self::Session);
}

/// Engine-owned outbound raw-TCP policy for the MySQL JS-driver isolate.
///
/// This is the portable peer of `zeroship_runtime::NetPolicy` — it names only
/// the engine-vendored reviewed-allowlist security type, so the migrate engine's
/// public MySQL surface + its TLS-pin/net-policy construction stay
/// runtime-`NetPolicy`-free (design §2.5). The adapter maps this to
/// `zeroship_runtime::NetPolicy` at `open_js_driver`. Only the subset migrate
/// constructs/queries crosses the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetPolicy {
    /// Trusted: skip host matching (but the runtime still enforces SSRF + socket +
    /// egress caps).
    Trusted {
        /// The per-isolate socket cap.
        max_sockets: u32,
        /// The hard egress ceiling in bytes.
        egress_ceiling_bytes: u64,
    },
    /// Allowlist: narrow connect targets to the operator-reviewed host:port entries.
    Allowlist {
        /// The operator-reviewed host:port allowlist.
        entries: ReviewedAllowlist,
        /// The per-isolate socket cap.
        max_sockets: u32,
        /// The hard egress ceiling in bytes.
        egress_ceiling_bytes: u64,
    },
}

impl NetPolicy {
    /// Build a `Trusted` policy (host matching skipped; caps still enforced).
    pub fn trusted(max_sockets: u32, egress_ceiling_bytes: u64) -> Self {
        Self::Trusted {
            max_sockets,
            egress_ceiling_bytes,
        }
    }

    /// Build an `Allowlist` policy over operator-reviewed entries. Rejects broad /
    /// fronting wildcards at construction (via `ReviewedAllowlist::operator_reviewed`).
    pub fn allowlist(
        entries: Vec<HostPort>,
        max_sockets: u32,
        egress_ceiling_bytes: u64,
    ) -> Result<Self, String> {
        Ok(Self::Allowlist {
            entries: ReviewedAllowlist::operator_reviewed(entries)?,
            max_sockets,
            egress_ceiling_bytes,
        })
    }

    /// The per-isolate socket cap.
    pub fn max_sockets(&self) -> u32 {
        match self {
            Self::Trusted { max_sockets, .. } | Self::Allowlist { max_sockets, .. } => *max_sockets,
        }
    }

    /// The hard egress ceiling in bytes.
    pub fn egress_ceiling_bytes(&self) -> Option<u64> {
        match self {
            Self::Trusted {
                egress_ceiling_bytes,
                ..
            }
            | Self::Allowlist {
                egress_ceiling_bytes,
                ..
            } => Some(*egress_ceiling_bytes),
        }
    }

    /// Whether the policy admits a connect to `host:port`.
    pub fn allows_host_port(&self, host: &str, port: u16) -> bool {
        match self {
            Self::Trusted { .. } => true,
            Self::Allowlist { entries, .. } => entries.iter().any(|e| e.matches(host, port)),
        }
    }
}
