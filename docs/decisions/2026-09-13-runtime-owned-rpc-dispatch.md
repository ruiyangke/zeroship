# ADR - the runtime owns RPC dispatch

- **Date:** 2026-09-13
- **Status:** Accepted
- **References:** [zeroship standard](../reference/zeroship-standard.md),
  [RPC reference](../reference/rpc.md),
  [Vite plugin reference](../reference/vite-plugin.md),
  [runtime architecture](../architecture/runtime.md)

## Context

RPC execution was divided between Rust and `@zeroship/bootstrap`. The runtime
parsed requests, retained V8 functions, settled promises and handled parts of
stream transport. Bootstrap and an embedded JavaScript copy performed procedure
lookup, validation, capability entry and response framing. Development called
the bootstrap dispatcher through a separate callable entry shape. Streaming
could resolve and invoke a procedure again through a generated fetch wrapper.

This split gave the same platform protocol competing implementations and made
behavior depend on whether a call was developed locally, returned an iterator
or ran from a deployed artifact. It also exposed coordination through mutable
`globalThis.__zs*` hooks. The host already owned the state required to enforce
startup, invocation lifetime, cancellation and transport rules.

The earlier [kernel-cut decision](./2026-04-20-kernel-cut.md) described a
fetch-only kernel and expected RPC to remain in bootstrap. The implementation
has since established RPC as a platform protocol enforced by the runtime. This
ADR supersedes that ownership choice while leaving the historical decision
unchanged.

## Decision

`zeroship-runtime` owns application readiness, RPC target resolution,
invocation context, cancellation, validation scheduling and transport framing.
It resolves the requested RPC name as an exact string key in the captured
application entry and invokes the retained procedure through V8.

The creator entry contract is:

```ts
export default {
  fetch?,
  rpc?: {
    [rpcName: string]: Procedure | { load: () => Promise<Procedure> };
  },
}
```

The runtime snapshots the dictionary before publishing an entry. It accepts an
eager procedure or an explicit loader record, retains the actual callable and
its metadata, and caches lazy resolution for that entry generation. JavaScript
validators remain application code; Rust calls their `parse` methods inside the
native invocation frame.

The Vite plugin owns procedure discovery, explicit wire IDs, generated imports
and export normalization. Its synthetic entry emits callable references and
loader records. It does not inject an RPC parser, fetch router or dispatcher
into the creator artifact. In development, ModuleRunner returns an entry
snapshot to the host, and Rust uses the same native invocation path.

Trusted SDK adapter modules continue to enter an isolate through the runtime's
plugin module registry, represented internally by `PluginModules`. This is a
module delivery seam with reserved-specifier enforcement. It is not an
application dispatcher or a replacement bootstrap package.

Database facade behavior remains in `@zeroship/db`. The host validates the
runtime descriptor and drives plugin startup, while the DB adapter installs SDK
collections and records startup policy through the plugin lifecycle.

Durable workflow dispatch, replay and protocol shape are outside this decision.
Their active refactor owns the replacement and the removal of the retained
workflow bridge. This ADR does not preserve that bridge as an RPC compatibility
contract.

## Consequences

- `@zeroship/bootstrap` is removed from the workspace and creator bundles.
- RPC dispatch does not read `globalThis.__zsDispatch` or an embedded
  JavaScript dispatcher.
- RPC-only applications do not need a generated fetch wrapper.
- Built and development entries expose procedure dictionaries to the host.
- Procedure metadata and validators stay in JavaScript while lifecycle and
  protocol enforcement stay in Rust.
- Reserved host modules cannot be supplied or shadowed by creator artifacts.
- Workflow bridge deletion remains coupled to the separate workflow refactor.

## Rejected alternatives

- **Repair or rename bootstrap.** This would retain competing dispatch paths
  and the creator-bundle dependency.
- **Route platform RPC through generated fetch code.** This would keep protocol
  enforcement in creator artifacts and preserve return-value redispatch.
- **Keep callable `default.rpc(name, input, ctx)`.** This is another dispatcher,
  and development was its in-tree consumer.
- **Move SDK behavior into Rust.** Query builders, collection facades and
  application validators naturally remain JavaScript. Native ownership applies
  to host lifecycle and protocol enforcement.
