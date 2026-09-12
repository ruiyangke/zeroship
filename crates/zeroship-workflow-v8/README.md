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

The worker uses `WorkflowBinding::new` for control-plane-backed workflows.
The CLI uses `WorkflowBinding::dev_sqlite` for the local development engine.
