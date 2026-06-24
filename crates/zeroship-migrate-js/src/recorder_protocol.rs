//! The stdin/stdout wire protocol between the recorder service (parent) and the
//! kernel-sandboxed recorder child (`src/bin/recorder-child.rs`).
//!
//! The parent writes a [`ChildRequest`] JSON line on the child's stdin and reads a
//! [`ChildResponse`] JSON line from its stdout. The migration `.ts` source travels
//! IN-MEMORY over this pipe (never via a file the child must read), so landlock can
//! deny ALL filesystem reads outside the explicit allow-list without starving the
//! recorder.

use serde::{Deserialize, Serialize};

use crate::sandbox::SandboxReport;

/// The request the parent pipes to the child's stdin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildRequest {
    /// The untrusted migration `.ts`/`.js` source (bundled, self-contained). Travels
    /// in-memory; never written to disk.
    pub ts_source: String,
    /// The owner-app HINT stamped on the recorded IR (the engine server-stamps the
    /// authoritative `owner_app` at deploy, §8.6).
    pub owner_app: String,
    /// The filename-derived migration name used when the module omits an explicit one.
    pub name: String,
    /// HOSTED multi-tenant posture (kernel sandbox mandatory, refuse-to-run floor)
    /// vs LOCAL single-tenant (userland floor, kernel layers opportunistic).
    pub hosted: bool,
    /// Whether the parent's `pre_exec` requested + the netns engaged (the child
    /// re-confirms via `/proc` where it can).
    pub netns_engaged: bool,
    /// Whether the parent's `pre_exec` applied the rlimit budget.
    pub rlimit_engaged: bool,
    /// The V8 heap cap (MiB) the child installs on its runtime — the authoritative
    /// memory bound for an alloc bomb (RLIMIT_AS is too coarse for V8, see
    /// `sandbox::ResourceBudget`).
    pub heap_limit_mb: u32,
    /// The read-only filesystem allow-list paths for the landlock ruleset (the
    /// migration dir + the schema-types blob path, if any). Empty = deny all fs
    /// reads (landlock) — fine because the source is piped in-memory.
    pub allow_read_paths: Vec<String>,
}

/// The result the child writes to its stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildResponse {
    /// `true` iff `ir_json` is present and recording succeeded.
    pub ok: bool,
    /// The recorded `.ir.json` ENVELOPE string (the `ir_version`+`name`+`ops` shape
    /// `op_recorder.js` emits), present iff `ok`.
    pub ir_json: Option<String>,
    /// The structured error (eval failure / sandbox refusal), present iff `!ok`.
    pub error: Option<ChildError>,
    /// Which sandbox layers engaged — the honest baseline-vs-degraded-floor record.
    pub report: SandboxReport,
}

/// The kind of failure the child reports (mapped to the §8.8 structured error by the
/// parent service).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "message")]
pub enum ChildError {
    /// The migration module evaluation failed (syntax error, throw, op-function
    /// outside a recorder, …). Carries the V8/recorder error string.
    EvalError(String),
    /// The hosted recorder refused to run because the kernel sandbox floor was not
    /// met (neither seccomp nor netns engaged). NOT an authoring error — an
    /// environment refusal.
    SandboxRefused(String),
}

impl ChildResponse {
    pub fn ok(ir_json: String, report: SandboxReport) -> Self {
        ChildResponse {
            ok: true,
            ir_json: Some(ir_json),
            error: None,
            report,
        }
    }

    pub fn eval_error(message: String, report: SandboxReport) -> Self {
        ChildResponse {
            ok: false,
            ir_json: None,
            error: Some(ChildError::EvalError(message)),
            report,
        }
    }

    pub fn refused(message: String, report: SandboxReport) -> Self {
        ChildResponse {
            ok: false,
            ir_json: None,
            error: Some(ChildError::SandboxRefused(message)),
            report,
        }
    }
}
