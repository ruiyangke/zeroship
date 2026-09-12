# zeroship-workflow-v8

The V8 adapter for `zeroship-workflow`. `WorkflowBinding` installs the native
`env.workflows` namespace, captures the host's app identity, translates errors,
and exposes workflow and run handles to JavaScript.

`v8_class.rs` owns argument validation and native handles. `dev.rs` implements
the Rust engine's `WorkflowExecutor` with the worker runtime. PostgreSQL journal
logic and the HTTP client belong to `zeroship-workflow`.

The worker uses `WorkflowBinding::new` for control-plane-backed workflows.
The CLI uses `WorkflowBinding::dev_sqlite` for the local development engine.
