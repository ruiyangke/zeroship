# zeroship-workflow-v8

The V8 adapter for `zeroship-workflow`. `WorkflowBinding` installs the native
`env.workflows` namespace, captures the host's app identity, translates errors,
and exposes workflow and run handles to JavaScript.

`v8_class.rs` owns argument validation and native handles. `AppRuntimeLoader`
binds the customer's native plugins and runtime identity to retained code.
PostgreSQL journal logic and the HTTP client belong to `zeroship-workflow`.

`js/dispatch.js` is the replay interpreter: it reads workflow classes off the
creator entry's namespace, drives the journal-backed `step` surface, and returns
a StepResult-shaped object. `WorkflowBinding` registers it as a host-only module
under `zeroship_runtime::WORKFLOW_DISPATCH_MODULE`, so creator code cannot import
it and nothing is injected into the creator's module graph. It is hand-written
JavaScript embedded with `include_str!`; there is no build step. Startup calls
its `installBodyGuards` export before creator modules evaluate, and
`Runtime::call_workflow_dispatch` calls its `dispatch` export per replay.

`executor.rs` implements the shared service runner's `TaskExecutor`. A trusted
loader supplies a fresh runtime for the assignment's immutable deployment. The
executor installs the runner's deadline interrupt before module initialization;
app code receives replay inputs without task credentials. Shutdown quarantines
the isolate and joins native work before the slot can be reused. The local
executor also uses this lifecycle.
The shared executor reads the retained executable through its live task claim
before calling `WorkflowRuntimeLoader`. The loader receives the verified module
graph and runtime descriptor and supplies the customer's native bindings and
runtime variables. Missing executable bytes fail before constructing an app
isolate. The replay contract resumes a waiting run from its retained dependency
sources after redeployment and engine restart.
The shared executor hydrates referenced input within its payload budget before
module initialization. Lazy output reads use the assignment's captured journal
and live task credentials, so replay never resolves through the app's current
generation. Oversized input fails before a runtime is loaded.
Losing a required replay payload interrupts the isolate and wakes the host
independently of JavaScript promise settlement. The runner releases the task
without committing app outcomes, and a fresh attempt can retry the read. App
code cannot catch that infrastructure failure and continue producing effects.
After receiving a result, the shared executor disposes the isolate and joins
native work before preparing or uploading payloads. Background app work cannot
continue during storage I/O. Transient upload failures retry the prepared bytes
and request identities within the live task budget, without repeating the step
callback. The service stores the payload and commits its history reference;
storage credentials never enter V8.

The worker uses `WorkflowBinding::new` for control-plane-backed workflows.
The CLI uses the shared engine through `WorkflowBinding::service`, using
an app-scoped `AppBackend`. Before evaluating app code, the binding
checks its scope against `RuntimeBuilder::app_id`; an environment variable
cannot supply or override that identity. Native binding tests cover lifecycle
operations, output reads and rejection of foreign app access.
