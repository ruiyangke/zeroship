# compio-postgres architecture: measured critique and a staged plan

Date: 2026-08-29. Every number here is measured, not estimated. The method is at
the bottom so it can be re-run.

## The headline defect: the connection core is one cycle

**16 of the crate's modules form a single strongly-connected component.** Every
one of the 240 ordered pairs among them is mutually reachable:

    cancel_token  client     config      connect     connection  connect_raw
    connect_socket connect_tls copy_in   copy_out    maybe_tls_stream
    prepare       query      simple_query statement  tls

There is no layering inside that set. You cannot reason about, test, or replace
any of those modules without the other fifteen.

The periphery IS correctly layered and should be left alone: `escape`,
`replication`, `transaction`, `codec`, `buf_stream`, `types`, `error`,
`keepalive`, `portal`, `row`, `bind`, `binary_copy` are all outside the cycle.

**A caution about this measurement.** A first pass reported 19 modules and 172
edges. That was WRONG: `grep 'crate::[a-z_]+'` also matches rustdoc intra-doc
links like `[`crate::transaction`]`, so documentation was counted as
dependency. `escape.rs` has no `use crate::` at all and was falsely placed in a
cycle. Stripping comments gives **164 edges and 16 modules**. Strip comments
before drawing a module graph.

## What actually creates the cycle

Not deep entanglement - **convenience methods placed on the wrong type.**

### `config.rs` (3445 production lines, highest fan-in in the crate at 17)

A configuration module should be a leaf: data plus parsing. This one imports
`connect`, `connect_raw`, `connect_tls`, `tls`, `Client` and `Connection`.

Every production use of those imports is inside TWO methods:

    2421  pub async fn connect<T>(&self, tls: T) -> Result<(Client, Connection<...>), Error>
    2443  pub async fn connect_raw<S, T>(...)

That is roughly 55 lines out of 3445. Those 55 lines put the module that 17
other modules depend on inside the cycle.

### `client.rs`

The same shape. `Client` carries convenience methods (`query`, `copy_in`,
`copy_out`, `simple_query`, `prepare`) that make it depend on the operation
modules, while those modules need `Client`/`InnerClient` to execute. Five edges.

## The fix, and what it is measured to buy

Rust permits an inherent `impl` block in any module of the defining crate. So
the methods can MOVE without the public API changing at all: `config.connect(tls).await`
still compiles and still means the same thing.

Simulated by removing the corresponding edges and recomputing the SCC:

| Stage | Change | SCC size | Modules freed |
| --- | --- | ---: | --- |
| baseline | - | 16 | - |
| 1 | move `impl Config { connect, connect_raw }` into `connect.rs` | **10** | cancel_token, config, connect, connect_raw, connect_socket, tls |
| 2 | move `impl Client { query, copy_in, copy_out, simple_query, prepare }` into their own modules | **8** | client, copy_out |

Stage 1 moves ~55 lines and frees six modules, including the crate's most
depended-upon one. That is the highest-leverage change available.

After stage 2 the residue is `connection, connect_tls, copy_in,
maybe_tls_stream, prepare, query, simple_query, statement` - protocol
operations that genuinely interact with the connection loop. Those cycles are
intrinsic, not accidental, and chasing them further would be churn.

## Second defect: the split read path is under-tested, and it is the hot one

`BufStream` has two read paths - whole-stream, and split-into-halves. The
mutation sweep found the same mutation repeatedly KILLED on the whole-stream
path and SURVIVING on its split twin; one whole-stream mutation failed 17
library tests while the identical split-path change cleared the entire library
target. Test population, counted independently:

    buf_stream.rs unit tests        23
      of which exercise split        6
    suite files touching split       1

**The split path is what the multiplexed connection loop uses** - the production
path for ordinary concurrent queries. The whole-stream path is the serialized
fallback. The better-tested half is the less-used one.

This is a testing gap rather than a structural one, but it is listed here
because it predicts where the next defect will be, and because the same
asymmetry plausibly exists in `tls_sansio.rs` (`TlsReadHalf`/`TlsWriteHalf`) and
`maybe_tls_stream.rs`.

## Third observation: module size is not itself the problem

    config.rs      3445 prod  (2072 test)
    connection.rs  3205 prod  (3764 test)
    replication.rs 2912 prod  (2977 test)
    pool.rs        2752 prod  (2342 test)

`connection.rs` at 3205 production lines is large, but it is one coherent thing:
the connection event loop. Splitting it for size alone would create more edges,
not fewer. Size is worth revisiting only after the cycle is broken, when the
seams are visible.

## Method, so this can be re-run

```bash
cd libs/compio-postgres/src
for f in *.rs; do m=${f%.rs}; [ "$m" = lib ] && continue
  awk '/^#\[cfg\(test\)\]/{exit} {sub(/\/\/.*$/,""); print}' "$f" \
    | grep -oE "crate::[a-z_]+" | sed 's/crate:://' | sort -u | grep -vx "$m" \
    | while read d; do [ -f "$d.rs" ] && echo "$m $d"; done
done | sort -u > edges.txt
```

Then iterate `join` to a transitive closure and take `$1==$2` for cycle
members. Two properties matter: strip comments (rustdoc links are not
dependencies), and stop at the first `#[cfg(test)]` (test-only imports are not
architecture).
