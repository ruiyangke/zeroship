//! The PR4a §8.9.2 HOSTED recorder service — spawns a FRESH kernel-sandboxed child
//! per `record` invocation, enforces the resource budget + wall-clock watchdog, and
//! implements the §8.9.2 contract (`POST /v1/recorder/record`, PAT auth + server-side
//! `app_id` ownership cross-check, per-token + global concurrency limits).
//!
//! Layering:
//! - [`spawn_sandboxed_record`] is the SECURITY CORE: it spawns the
//!   `zero-migrate-recorder-child` binary with the §8.9 `pre_exec` lockdown
//!   (netns + rlimits + no_new_privs), pipes the request in-memory, applies the
//!   parent wall-clock watchdog (`SIGKILL` on overrun), and classifies the child's
//!   termination cause (SIGSYS / SIGXCPU / SIGKILL → the §8.8 structured error).
//! - [`RecorderService`] wraps it with the §8.9.2 service contract: auth via an
//!   injectable [`Authorizer`] seam (PAT → owned apps, the §8.6 ownership check),
//!   per-token concurrency + a global cap with a queue.
//!
//! The wall watchdog + termination-cause classification are why a fork+exec, an
//! infinite loop, and an allocation bomb all surface as a clean structured error
//! rather than a hang or a confusing exit code.

#![allow(unsafe_code)] // Command::pre_exec closure + waitpid/kill

use std::io::Write;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::recorder_protocol::{
    ChildError, ChildOperation, ChildRequest, ChildResponse, MAX_TS_SOURCE_BYTES,
};
use super::sandbox::{pre_exec_netns, pre_exec_rlimits, ResourceBudget, SandboxPosture, SandboxReport};

/// The §8.8 structured error the recorder surfaces (mapped from the child's
/// termination cause or its `ChildError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecorderError {
    /// The resource budget (RLIMIT_CPU / wall-watchdog / RLIMIT_AS) bounded the
    /// child — `BUILD_RECORDER_BUDGET_EXCEEDED` (§8.8).
    BudgetExceeded {
        /// Which budget tripped: "cpu" (SIGXCPU), "wall" (watchdog SIGKILL),
        /// "memory" (RLIMIT_AS / OOM), or "unknown-kill".
        which: String,
    },
    /// The kernel killed the child for a denied syscall — `SIGSYS` from the seccomp
    /// default-deny filter (a subprocess spawn / network socket attempt). Surfaced
    /// distinctly so the faithful e2e can assert the KERNEL fired.
    KilledBySeccomp,
    /// The migration `.ts` evaluation reported an authoring error (op outside
    /// recorder, throw, etc.).
    EvalError(String),
    /// A recorder input exceeded the shared source/blob size cap.
    RequestTooLarge {
        /// Which field was too large (`ts_source`, `schema_source`,
        /// `schema_types_blob`, or `request`).
        field: String,
        /// The accepted byte limit.
        limit: usize,
        /// The observed byte count.
        actual: usize,
    },
    /// The hosted recorder refused to run (kernel floor not met — neither seccomp
    /// nor netns). Maps to a 503/environment refusal, NOT an authoring reject.
    SandboxRefused(String),
    /// The child was killed by a BARE `SIGKILL` whose origin is AMBIGUOUS — it could
    /// be an ENVIRONMENT event (operator kill, an unrelated-cgroup OOM, node shutdown)
    /// rather than the migration's own resource budget. We do NOT assert an authoring
    /// reject for it: it maps to a RETRYABLE 503-class outcome (recorder-unreachable /
    /// environment → the client falls back to local recording), NOT a 422
    /// (PR4a code-critic LOW — bare SIGKILL is an environment event, not a reject).
    /// The genuinely migration-caused budget kills (cpu/wall/memory + SIGSEGV/
    /// SIGABRT/SIGBUS) stay [`RecorderError::BudgetExceeded`] (non-retryable 422).
    EnvironmentKilled(String),
    /// The token/app ownership check failed (§8.6) — fail-closed.
    Unauthorized(String),
    /// A burst exceeded the global concurrency cap (after the queue) — backpressure.
    Overloaded,
    /// The child could not be spawned / the protocol broke (recorder-unreachable
    /// class — the client retries / falls back to local).
    Spawn(String),
}

impl RecorderError {
    /// The §8.8 machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            RecorderError::BudgetExceeded { .. } => "BUILD_RECORDER_BUDGET_EXCEEDED",
            RecorderError::KilledBySeccomp => "BUILD_RECORDER_SANDBOX_VIOLATION",
            RecorderError::EvalError(_) => "RECORD_EVAL_ERROR",
            RecorderError::RequestTooLarge { .. } => "RECORDER_REQUEST_TOO_LARGE",
            RecorderError::SandboxRefused(_) => "RECORDER_SANDBOX_UNAVAILABLE",
            RecorderError::EnvironmentKilled(_) => "RECORDER_UNREACHABLE",
            RecorderError::Unauthorized(_) => "RECORDER_UNAUTHORIZED",
            RecorderError::Overloaded => "RECORDER_OVERLOADED",
            RecorderError::Spawn(_) => "RECORDER_UNREACHABLE",
        }
    }
}

impl std::fmt::Display for RecorderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecorderError::BudgetExceeded { which } => {
                write!(f, "BUILD_RECORDER_BUDGET_EXCEEDED ({which})")
            }
            RecorderError::KilledBySeccomp => {
                write!(f, "recorder child killed by seccomp (denied syscall, SIGSYS)")
            }
            RecorderError::EvalError(m) => write!(f, "migration evaluation failed: {m}"),
            RecorderError::RequestTooLarge {
                field,
                limit,
                actual,
            } => write!(
                f,
                "{field} is {actual} bytes; the recorder accepts at most {limit} bytes"
            ),
            RecorderError::SandboxRefused(m) => write!(f, "recorder refused to run: {m}"),
            RecorderError::EnvironmentKilled(m) => {
                write!(f, "recorder child killed by an environment event: {m}")
            }
            RecorderError::Unauthorized(m) => write!(f, "unauthorized: {m}"),
            RecorderError::Overloaded => write!(f, "recorder overloaded"),
            RecorderError::Spawn(m) => write!(f, "recorder unreachable: {m}"),
        }
    }
}
impl std::error::Error for RecorderError {}

/// The successful recording outcome (the §8.9.2 response body, pre-checksum). The
/// caller computes the `Checksum::of_ir` + the `.ts` provenance blob.
#[derive(Debug, Clone)]
pub struct RecordResult {
    /// The recorded `.ir.json` envelope string (the child's `ir_json`).
    pub ir_json: String,
    /// Which sandbox layers actually engaged (baseline-vs-degraded record).
    pub report: SandboxReport,
}

/// Inputs for one sandboxed recording (post-auth).
#[derive(Debug, Clone)]
pub struct RecordRequest {
    pub ts_source: String,
    pub owner_app: String,
    pub name: String,
    pub posture: SandboxPosture,
    pub budget: ResourceBudget,
    /// The landlock read-only allow-list (migration dir + schema-types blob path).
    pub allow_read_paths: Vec<PathBuf>,
    /// The optional §8.9.2 type-only schema-types blob (in-memory recorder context).
    pub schema_types_blob: Option<String>,
}

/// Inputs for one sandboxed schema evaluation.
#[derive(Debug, Clone)]
pub struct SchemaEvalRequest {
    pub schema_source: String,
    pub owner_app: String,
    pub posture: SandboxPosture,
    pub budget: ResourceBudget,
    /// The landlock read-only allow-list. Schema source is piped in-memory, so this
    /// is normally empty.
    pub allow_read_paths: Vec<PathBuf>,
}

/// Successful sandboxed schema evaluation output.
#[derive(Debug, Clone)]
pub struct SchemaEvalResult {
    /// The schema IR adapter envelope string (`{ ok, collections?, error? }`).
    pub schema_ir_json: String,
    /// Which sandbox layers actually engaged.
    pub report: SandboxReport,
}

/// Read the calling process's network-namespace inode
/// (`stat("/proc/self/ns/net").st_ino`). Used by the parent to capture its OWN netns
/// inode pre-spawn so the child can prove (by inode INEQUALITY) that its
/// `unshare(CLONE_NEWNET)` moved it into a FRESH namespace — robust where the
/// interface-count heuristic false-positives (a host whose only interface is `lo`,
/// PR4a code-critic LOW #1). Returns `None` if `/proc` is unreadable.
pub fn read_netns_inode() -> Option<u64> {
    std::fs::metadata("/proc/self/ns/net")
        .ok()
        .map(|m| std::os::unix::fs::MetadataExt::ino(&m))
}

/// Locate the recorder-child binary next to the current executable (the standard
/// cargo layout: sibling in `target/<profile>/`). Overridable via
/// `ZEROSHIP_RECORDER_CHILD` for packaged installs / tests.
pub fn recorder_child_path() -> PathBuf {
    if let Ok(p) = std::env::var("ZEROSHIP_RECORDER_CHILD") {
        return PathBuf::from(p);
    }
    let exe = std::env::current_exe().unwrap_or_default();
    let dir = exe.parent().map(PathBuf::from).unwrap_or_default();
    // Test binaries live in target/<profile>/deps/; the child is one level up.
    let candidate = dir.join("zero-migrate-recorder-child");
    if candidate.exists() {
        return candidate;
    }
    if let Some(parent) = dir.parent() {
        let up = parent.join("zero-migrate-recorder-child");
        if up.exists() {
            return up;
        }
    }
    candidate
}

/// Spawn a FRESH kernel-sandboxed child for ONE front-end evaluation, wait under
/// the wall-clock watchdog, and classify the result.
fn spawn_sandboxed_frontend(
    operation: ChildOperation,
    source: &str,
    owner_app: &str,
    name: &str,
    posture: SandboxPosture,
    budget: ResourceBudget,
    allow_read_paths: &[PathBuf],
    schema_types_blob: Option<&str>,
) -> Result<(String, SandboxReport), RecorderError> {
    let field = match operation {
        ChildOperation::RecordMigration => "ts_source",
        ChildOperation::EvalSchema => "schema_source",
    };
    reject_oversized_source(field, source, MAX_TS_SOURCE_BYTES)?;
    if let Some(blob) = schema_types_blob {
        reject_oversized_source("schema_types_blob", blob, MAX_TS_SOURCE_BYTES)?;
    }

    let child_bin = recorder_child_path();
    if !child_bin.exists() {
        return Err(RecorderError::Spawn(format!(
            "recorder child binary not found at {} (set ZEROSHIP_RECORDER_CHILD)",
            child_bin.display()
        )));
    }

    // We attempt netns + rlimits in pre_exec. netns can fail on a host with
    // unprivileged userns disabled; we still proceed (the child records what
    // engaged, and the hosted floor leans on seccomp). rlimits failing IS fatal for
    // the budget guarantee, so we abort the spawn if pre_exec returns Err for them.
    // The pre_exec closure runs in the forked child. netns first (at clone), then
    // rlimits + no_new_privs. We encode "did netns engage" by trying it and, on
    // failure, NOT failing the spawn (return Ok) — the child re-checks /proc. But
    // rlimits failing returns Err (aborts spawn) so we never run without the budget.
    let pre = move || -> std::io::Result<()> {
        // netns: best-effort. On failure we keep going (do not abort) — the child
        // detects it and the hosted floor requires seccomp instead.
        let _ = unsafe { pre_exec_netns() };
        // rlimits + no_new_privs: MUST succeed (budget guarantee + seccomp precond).
        unsafe { pre_exec_rlimits(budget) }?;
        Ok(())
    };

    let mut cmd = Command::new(&child_bin);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: the closure is async-signal-safe (only unshare/setrlimit/prctl).
    unsafe {
        cmd.pre_exec(pre);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| RecorderError::Spawn(format!("spawn recorder child: {e}")))?;

    // ---- Drain stdout CONCURRENTLY (PR4a code-critic HIGH #2) ----
    // The child's IR envelope can exceed the ~64KB OS pipe buffer (a 4000-op migration
    // emits >500KB). If the parent waited-then-read, the child would block on write,
    // never exit, and be SIGKILLed by the wall watchdog -> a SPURIOUS
    // BUILD_RECORDER_BUDGET_EXCEEDED for a legitimate large migration. So we read
    // stdout to EOF on a dedicated thread in parallel with the try_wait/watchdog loop,
    // and join it after the child exits/is killed.
    let stdout_handle = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut s = String::new();
            let _ = out.read_to_string(&mut s);
            s
        })
    });

    // Pipe the request envelope in-memory.
    let child_req = ChildRequest {
        operation,
        ts_source: source.to_string(),
        owner_app: owner_app.to_string(),
        name: name.to_string(),
        hosted: posture == SandboxPosture::Hosted,
        // We requested netns (best-effort) + rlimits (mandatory). The child confirms
        // netns by comparing its own net-ns inode against the parent's (below) and
        // ANDs the result into its report.
        netns_engaged: true,
        // The parent's net-ns inode, captured pre-spawn. The child compares this to its
        // own /proc/self/ns/net inode — a different inode is robust proof the unshare
        // took (vs the false-positive-prone interface-count check) (LOW #1).
        parent_netns_inode: read_netns_inode().unwrap_or(0),
        rlimit_engaged: true,
        heap_limit_mb: budget.heap_limit_mb,
        allow_read_paths: allow_read_paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        schema_types_blob: schema_types_blob.map(str::to_string),
    };
    let req_json = serde_json::to_string(&child_req)
        .map_err(|e| RecorderError::Spawn(format!("serialize child request: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        // Write may fail if the child already died (e.g. seccomp killed it before
        // reading) — that's fine, we detect it via wait below.
        let _ = stdin.write_all(req_json.as_bytes());
        // Dropping stdin closes it (EOF for the child's read_to_string).
    }

    // ---- Wall-clock watchdog (design §8.9) ----
    // RLIMIT_CPU bounds CPU-time; the wall watchdog bounds wall-time (covers a child
    // that sleeps / blocks). We poll for exit; on overrun we SIGKILL.
    let deadline = Instant::now() + Duration::from_millis(budget.wall_ms);
    let mut wall_killed = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Wall budget exceeded — SIGKILL the child.
                    let _ = child.kill();
                    wall_killed = true;
                    // Reap it.
                    break child.wait().map_err(|e| {
                        RecorderError::Spawn(format!("wait after wall-kill: {e}"))
                    })?;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(RecorderError::Spawn(format!("try_wait: {e}"))),
        }
    };

    // Join the concurrent stdout drain — it has read everything the child wrote
    // (to EOF, since the child has now exited / been killed and closed its stdout).
    // A successful recording wrote its envelope before exiting 0.
    let stdout = match stdout_handle {
        Some(h) => h.join().unwrap_or_default(),
        None => String::new(),
    };

    // ---- Classify the termination cause (faithful kernel-level signal) ----
    if wall_killed {
        return Err(RecorderError::BudgetExceeded {
            which: "wall".into(),
        });
    }
    if let Some(sig) = status.signal() {
        return Err(classify_signal(sig, stdout.trim().is_empty()));
    }

    // Exited normally (code 0/non-0). Parse the response envelope.
    if !status.success() && stdout.trim().is_empty() {
        return Err(RecorderError::Spawn(format!(
            "recorder child exited {} with no output",
            status.code().unwrap_or(-1)
        )));
    }
    let resp: ChildResponse = serde_json::from_str(stdout.trim()).map_err(|e| {
        RecorderError::Spawn(format!(
            "parse recorder child response: {e}; raw: {}",
            stdout.chars().take(200).collect::<String>()
        ))
    })?;

    if resp.ok {
        let ir_json = resp.ir_json.ok_or_else(|| {
            RecorderError::Spawn("recorder child reported ok but no output json".into())
        })?;
        Ok((ir_json, resp.report))
    } else {
        match resp.error {
            Some(ChildError::EvalError(m)) => Err(RecorderError::EvalError(m)),
            Some(ChildError::RequestTooLarge {
                field,
                limit,
                actual,
            }) => Err(RecorderError::RequestTooLarge {
                field,
                limit,
                actual,
            }),
            Some(ChildError::SandboxRefused(m)) => Err(RecorderError::SandboxRefused(m)),
            None => Err(RecorderError::Spawn("recorder child failed with no error".into())),
        }
    }
}

/// The SECURITY CORE: spawn a FRESH kernel-sandboxed child for ONE recording, wait
/// under the wall-clock watchdog, and classify the result.
///
/// This is what gives per-invocation tenant isolation (a brand-new locked-down
/// process every call — no pooling, no reuse) and what surfaces a denied syscall /
/// budget overrun as the §8.8 structured error.
pub fn spawn_sandboxed_record(req: &RecordRequest) -> Result<RecordResult, RecorderError> {
    let (ir_json, report) = spawn_sandboxed_frontend(
        ChildOperation::RecordMigration,
        &req.ts_source,
        &req.owner_app,
        &req.name,
        req.posture,
        req.budget,
        &req.allow_read_paths,
        req.schema_types_blob.as_deref(),
    )?;
    Ok(RecordResult { ir_json, report })
}

/// Spawn a FRESH kernel-sandboxed child for ONE schema evaluation. This uses the
/// same child binary, pre-exec budget, wall watchdog, and kernel layers as
/// [`spawn_sandboxed_record`]; only the child entry adapter differs.
pub fn spawn_sandboxed_schema_eval(
    req: &SchemaEvalRequest,
) -> Result<SchemaEvalResult, RecorderError> {
    let (schema_ir_json, report) = spawn_sandboxed_frontend(
        ChildOperation::EvalSchema,
        &req.schema_source,
        &req.owner_app,
        "schema",
        req.posture,
        req.budget,
        &req.allow_read_paths,
        None,
    )?;
    Ok(SchemaEvalResult {
        schema_ir_json,
        report,
    })
}

/// Map a child's terminating signal to the §8.8 structured error.
///
/// - `SIGSYS` (31) ⇒ the seccomp default-deny filter killed it for a denied syscall
///   (subprocess spawn / network socket). This is the FAITHFUL kernel-level signal.
/// - `SIGXCPU` (24) ⇒ `RLIMIT_CPU` tripped (infinite loop) → budget (cpu).
/// - `SIGKILL` (9) ⇒ a BARE kill not attributable to OUR wall watchdog (that path is
///   handled before this via `wall_killed`). Its origin is AMBIGUOUS — it could be the
///   kernel OOM-killer reacting to this child's own allocation, OR an EXTERNAL/
///   ENVIRONMENT event (operator kill, an unrelated cgroup's OOM victim selection,
///   node shutdown). We do NOT assert it was the migration's MEMORY budget — that
///   would mis-emit a 422 authoring-class signal for an environment event
///   (PR4a code-critic MED #6). Crucially we ALSO do not leave it as a non-retryable
///   `BudgetExceeded`: an ambiguous bare SIGKILL maps to [`RecorderError::EnvironmentKilled`]
///   — a RETRYABLE 503-class outcome (recorder-unreachable → the client retries /
///   falls back to LOCAL recording) rather than a 422 authoring reject (PR4a
///   code-critic LOW — bare SIGKILL is an environment event, not an authoring reject).
/// - `SIGSEGV`/`SIGABRT`/`SIGBUS` ⇒ a V8 crash (e.g. RLIMIT_AS made an allocation
///   fail hard) → budget (memory): this IS attributable to the child's own execution,
///   so it STAYS a non-retryable `BudgetExceeded` (a genuinely migration-caused kill).
fn classify_signal(sig: i32, _stdout_empty: bool) -> RecorderError {
    match sig {
        libc::SIGSYS => RecorderError::KilledBySeccomp,
        libc::SIGXCPU => RecorderError::BudgetExceeded { which: "cpu".into() },
        // Bare SIGKILL: ambiguous origin (operator/cgroup-OOM/node-shutdown vs the
        // child's own overrun). Route to the RETRYABLE environment-event class rather
        // than a non-retryable authoring reject — the client falls back to local.
        libc::SIGKILL => RecorderError::EnvironmentKilled(
            "bare SIGKILL (ambiguous: operator kill / unrelated-cgroup OOM / node shutdown)"
                .into(),
        ),
        libc::SIGSEGV | libc::SIGABRT | libc::SIGBUS => RecorderError::BudgetExceeded {
            which: "memory".into(),
        },
        other => RecorderError::BudgetExceeded {
            which: format!("signal-{other}"),
        },
    }
}

fn reject_oversized_source(
    field: &str,
    source: &str,
    limit: usize,
) -> Result<(), RecorderError> {
    let actual = source.len();
    if actual > limit {
        Err(RecorderError::RequestTooLarge {
            field: field.to_string(),
            limit,
            actual,
        })
    } else {
        Ok(())
    }
}

// ===========================================================================
// The §8.9.2 service contract: auth + ownership + concurrency limits
// ===========================================================================

/// The PAT → owned-apps authorizer seam (the §8.6 ownership check at record time).
/// Injected so the service can be exercised without a live control plane; the
/// production impl verifies the PAT and checks the `app_id` against the token's
/// owned apps server-side.
pub trait Authorizer: Send + Sync {
    /// Returns `Ok(())` iff `token` is valid AND owns `app_id`. Fail-closed:
    /// an unknown token / unknown owner / not-owned app is `Err`.
    fn authorize(&self, token: &str, app_id: &str) -> Result<(), String>;
}

/// Per-token + global concurrency limits (design §8.9.2 DoS fairness). A simple
/// counter-based limiter: per-token in-flight cap + a global in-flight cap. A burst
/// past the global cap is `Overloaded` (the HTTP layer queues/retries).
#[derive(Debug, Clone, Copy)]
pub struct ConcurrencyLimits {
    pub per_token_max: usize,
    pub global_max: usize,
}

impl Default for ConcurrencyLimits {
    fn default() -> Self {
        ConcurrencyLimits {
            per_token_max: 4,
            global_max: 32,
        }
    }
}

/// The hosted recorder service. Holds the authorizer + the concurrency state.
#[allow(missing_debug_implementations)] // holds a `Box<dyn Authorizer>` trait object
pub struct RecorderService {
    authorizer: Box<dyn Authorizer>,
    limits: ConcurrencyLimits,
    inflight: std::sync::Mutex<InflightState>,
    pub budget: ResourceBudget,
}

#[derive(Default)]
struct InflightState {
    global: usize,
    per_token: std::collections::HashMap<String, usize>,
}

/// An RAII guard that decrements the in-flight counters on drop (so a panic / early
/// return during recording still releases the slot).
struct InflightGuard<'a> {
    svc: &'a RecorderService,
    token: String,
}
impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        // Recover from a poisoned mutex instead of double-panicking in `drop` (which
        // would ABORT the whole multi-tenant service). A poisoned `inflight` lock just
        // means some prior holder panicked; the counter state is still usable for the
        // saturating decrement here (PR4a code-critic LOW #2).
        let mut st = self
            .svc
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        st.global = st.global.saturating_sub(1);
        if let Some(c) = st.per_token.get_mut(&self.token) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                st.per_token.remove(&self.token);
            }
        }
    }
}

impl RecorderService {
    pub fn new(authorizer: Box<dyn Authorizer>) -> Self {
        RecorderService {
            authorizer,
            limits: ConcurrencyLimits::default(),
            inflight: std::sync::Mutex::new(InflightState::default()),
            budget: ResourceBudget::default(),
        }
    }

    pub fn with_limits(mut self, limits: ConcurrencyLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Try to acquire an in-flight slot for `token` (per-token + global caps). The
    /// HTTP layer queues on `Overloaded`. Returns a guard that releases on drop.
    fn acquire(&self, token: &str) -> Result<InflightGuard<'_>, RecorderError> {
        // Recover from poisoning rather than propagate a panic into the request path
        // (LOW #2): a prior panic-while-held must not wedge the whole service.
        let mut st = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if st.global >= self.limits.global_max {
            return Err(RecorderError::Overloaded);
        }
        let tc = st.per_token.entry(token.to_string()).or_insert(0);
        if *tc >= self.limits.per_token_max {
            return Err(RecorderError::Overloaded);
        }
        *tc += 1;
        st.global += 1;
        drop(st);
        Ok(InflightGuard {
            svc: self,
            token: token.to_string(),
        })
    }

    /// The §8.9.2 `record` operation: authorize (PAT → owns app_id), acquire a
    /// concurrency slot, then spawn a FRESH sandboxed child for this one recording.
    ///
    /// `token` is the PAT/access token; `app_id` is the claimed owner app
    /// (cross-checked server-side). HOSTED posture is enforced here (the kernel
    /// sandbox is mandatory for the multi-tenant path).
    pub fn record(
        &self,
        token: &str,
        app_id: &str,
        ts_source: &str,
        name: &str,
        schema_types_blob: Option<&str>,
    ) -> Result<RecordResult, RecorderError> {
        // §8.6 ownership cross-check — fail-closed.
        self.authorizer
            .authorize(token, app_id)
            .map_err(RecorderError::Unauthorized)?;

        let _guard = self.acquire(token)?;

        let req = RecordRequest {
            ts_source: ts_source.to_string(),
            owner_app: app_id.to_string(),
            name: name.to_string(),
            posture: SandboxPosture::Hosted, // hosted multi-tenant: kernel sandbox mandatory
            budget: self.budget,
            allow_read_paths: vec![], // source is in-memory; deny all fs reads
            schema_types_blob: schema_types_blob.map(|s| s.to_string()),
        };
        spawn_sandboxed_record(&req)
        // _guard drops here → slot released.
    }

    /// Inspection helper: current global in-flight count (used by the isolation e2e
    /// + ops metrics).
    pub fn inflight_global(&self) -> usize {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .global
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct YesAuthorizer;
    impl Authorizer for YesAuthorizer {
        fn authorize(&self, _token: &str, _app_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// PR4a code-critic LOW (SIGKILL classification): a BARE `SIGKILL` is AMBIGUOUS
    /// (operator kill / unrelated-cgroup OOM / node shutdown) and must map to a
    /// RETRYABLE environment-event outcome — NOT a non-retryable 422 authoring reject.
    /// The genuinely migration-caused signals stay non-retryable `BudgetExceeded`.
    #[test]
    fn bare_sigkill_is_retryable_environment_event_not_authoring_reject() {
        use crate::frontend::recorder_http::StructuredError;

        // Bare SIGKILL -> EnvironmentKilled -> retryable 503 (recorder-unreachable class).
        let killed = classify_signal(libc::SIGKILL, true);
        assert!(
            matches!(killed, RecorderError::EnvironmentKilled(_)),
            "bare SIGKILL must classify as EnvironmentKilled, got {killed:?}"
        );
        let se: StructuredError = (&killed).into();
        assert_eq!(se.http_status, 503, "bare SIGKILL must be a 503-class outcome");
        assert!(
            se.retryable,
            "bare SIGKILL must be RETRYABLE (client falls back to local)"
        );
        assert_eq!(se.code, "RECORDER_UNREACHABLE");
    }

    /// The genuinely migration-caused kills (cpu / wall via watchdog / memory crash)
    /// stay NON-retryable 422 authoring/bounded-build rejects — they are attributable
    /// to the child's own execution, NOT an environment event.
    #[test]
    fn migration_caused_kills_stay_non_retryable_422() {
        use crate::frontend::recorder_http::StructuredError;

        for (sig, which) in [
            (libc::SIGXCPU, "cpu"),
            (libc::SIGSEGV, "memory"),
            (libc::SIGABRT, "memory"),
            (libc::SIGBUS, "memory"),
        ] {
            let e = classify_signal(sig, false);
            match &e {
                RecorderError::BudgetExceeded { which: w } => {
                    assert_eq!(w, which, "signal {sig} must classify as budget '{which}'")
                }
                other => panic!("signal {sig} must be BudgetExceeded, got {other:?}"),
            }
            let se: StructuredError = (&e).into();
            assert_eq!(
                se.http_status, 422,
                "migration-caused kill (signal {sig}) must be a 422 reject"
            );
            assert!(
                !se.retryable,
                "migration-caused kill (signal {sig}) must NOT be retryable"
            );
        }

        // A seccomp-denied syscall (SIGSYS) is a sandbox violation — also non-retryable
        // 422 (the migration tried something denied), distinct from an environment kill.
        let sys = classify_signal(libc::SIGSYS, false);
        assert!(matches!(sys, RecorderError::KilledBySeccomp), "got {sys:?}");
        let se: StructuredError = (&sys).into();
        assert_eq!(se.http_status, 422);
        assert!(!se.retryable);
    }

    /// PR4a code-critic LOW #2: a POISONED `inflight` mutex must NOT abort the service.
    /// We poison the lock (panic while holding it inside `catch_unwind`), then exercise
    /// EVERY path that touches the lock — `inflight_global`, `acquire`, and the
    /// `InflightGuard::drop` that runs when the acquired guard is dropped. Under the old
    /// `lock().unwrap()` the guard's Drop would double-panic and ABORT the process; with
    /// `unwrap_or_else(|e| e.into_inner())` they all recover.
    #[test]
    fn poisoned_inflight_mutex_does_not_abort_service() {
        let svc = RecorderService::new(Box::new(YesAuthorizer));

        // Poison the mutex: panic while the lock is held.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = svc.inflight.lock().unwrap();
            panic!("intentional poison while holding the inflight lock");
        }));
        assert!(r.is_err(), "the poisoning closure must have panicked");
        assert!(
            svc.inflight.is_poisoned(),
            "the inflight mutex must now be poisoned"
        );

        // 1. inflight_global recovers (no panic).
        let _ = svc.inflight_global();

        // 2. acquire recovers AND the returned guard's Drop recovers (the LOW #2 abort
        //    path) — drop happens at end of this block.
        {
            let _guard = svc
                .acquire("pat_x")
                .expect("acquire must succeed despite poisoning");
            assert_eq!(svc.inflight_global(), 1, "slot acquired under poison");
        } // InflightGuard::drop runs here — must NOT double-panic/abort.

        assert_eq!(
            svc.inflight_global(),
            0,
            "guard drop must release the slot even under a poisoned mutex"
        );
    }
}
