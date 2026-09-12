# zeroship-workflow-v8

The V8 adapter for `zeroship-workflow`. `WorkflowBinding` installs the native
`env.workflows` namespace, captures the host's app identity, translates errors,
and exposes workflow and run handles to JavaScript.

`v8_class.rs` owns argument validation and native handles. `dev.rs` implements
the Rust engine's `WorkflowExecutor` with the worker runtime. PostgreSQL journal
logic and the HTTP client belong to `zeroship-workflow`.

`executor.rs` implements the shared service runner's `TaskExecutor`. A trusted
loader supplies a fresh runtime for the assignment's immutable deployment. The
executor installs the runner's deadline interrupt before module initialization;
app code receives replay inputs without task credentials. Shutdown quarantines
the isolate and joins native work before the slot can be reused. The local
executor also uses this lifecycle.
The shared executor hydrates referenced input within its payload budget before
module initialization. Lazy output reads use the assignment's captured journal
and live task credentials, so replay never resolves through the app's current
generation. Oversized input fails before a runtime is loaded.
Losing a required replay payload interrupts the isolate and wakes the host
independently of JavaScript promise settlement. The runner releases the task
without committing app outcomes, and a fresh attempt can retry the read. App
code cannot catch that infrastructure failure and continue producing effects.

The worker uses `WorkflowBinding::new` for control-plane-backed workflows.
The CLI uses `WorkflowBinding::dev_sqlite` for the local development engine.
The replacement shared service binds through `WorkflowBinding::service`, using
an embedded or remote `AppBackend`. Before evaluating app code, the binding
checks its scope against `RuntimeBuilder::app_id`; an environment variable
cannot supply or override that identity. Native binding tests cover lifecycle
operations, output reads and rejection of foreign app access.
