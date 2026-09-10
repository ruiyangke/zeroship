# V8 decoupling of the migration engine

**Status.** SHIPPED, and the tree went further than this document specified. The engine
crates carry no V8 at all: `crates/zeroship-migrate`, `-core`, `-backend`, `-ir`,
`-policy`, `-postgres`, `-sqlite` and `-mysql` name neither `v8` nor `zeroship-runtime`
in any manifest or source file. The `zsv8` cargo feature this document designed does not
exist and is not needed - `crates/zeroship-migrate` declares no cargo features whatsoever,
because the in-Rust V8 host was deleted rather than gated. The napi addon
`crates/zeroship-migrate-node` is built and ships; the root `pnpm build` builds it first
in the chain.

## What it is

The migration engine is V8-free by construction, not by configuration. Nothing in the
engine set can reach an isolate, because no crate in that set has an edge to one.

The composition is a crate graph rather than a feature lattice.
`crates/zeroship-migrate` is the composition root: it re-exports
`zeroship-migrate-core` wholesale and holds one fact, which vendor backends this build
ships (`SHIPPING` in its `src/lib.rs`). The engine, `zeroship-migrate-core`, depends only
on `zeroship-migrate-backend` (the contract), `zeroship-migrate-ir` (the wire) and
`zeroship-migrate-policy` (the PDP). The three vendor crates implement the contract and
depend on the same three; none depends on the engine or on the composition root. That
one-way arrow is what makes neutrality structural: the engine cannot name a vendor
because it cannot see one.

The cut line is `MigrationBackend`, defined in `zeroship-migrate-backend`. Every method
takes dialect-neutral owned types; no driver handle and no runtime handle crosses it.

The two subsystems that used to force V8 into Rust now live outside it:

*Schema authoring.* The JS/TS recorder is `packages/zero-migrate` (published as
`@zeroship/migrate`), evaluated by Node in `packages/zero-migrate-cli`. There is no Rust
recorder, no sandboxed recorder child, and no seccomp/landlock surface in the engine.

*Network execution.* PostgreSQL and MySQL are driven through
`driver::SqlSession` (`crates/zeroship-migrate-backend/src/driver.rs`), a dialect-neutral
seam typed in `Bind` / `Value` / `Row` / `DbError`. The production producer is
`NapiHostSession` in `crates/zeroship-migrate-node`, marshalling each verb to a host
driver: npm `pg` and `mysql2`, wired in `packages/zero-migrate-cli/src/driver-pg.ts` and
`driver-mysql2.ts`. SQLite does not ride the seam - it is an in-process `rusqlite` actor
in `crates/zeroship-migrate-sqlite`, so the seam is an implementation detail of the
network backends and not a bound on `MigrationBackend`.

The platform's own Rust host supplies its own session rather than a Node one:
`crates/zeroship-migrate-server/src/session.rs` defines `CompioPgSession`, a newtype over
`compio_postgres::Client` implementing `SqlSession`. That service's library surface is
V8-free; `zeroship-runtime` and `v8` appear in its manifest only as dev-dependencies, for
a test that authors an IR envelope from a `.ts` migration inside the platform's own
isolate.

## Why it is this way

The in-Rust V8 host was always redundant work. Every host that needs authoring or a
network driver already has V8, `mysql2` and the DSL in its own process; embedding a second
isolate in the engine bought nothing and cost the engine its portability. Do not
reintroduce one to serve either purpose.

Neutrality is enforced by the dependency graph and must stay there. It was previously
policed by hand-written censuses over the engine's own source text, several of which had
gone blind. Cargo sees crate names only, so two things the graph still cannot check remain
covered by census tests and by nothing else: a vendor's *grammar* (a
`format!("EXCLUDE USING gist ...")` names no crate) and the engine's `#[cfg(test)]`
modules, which reach the vendors through dev-dependencies.

`MigrationBackend` and `SqlSession` are both neutral on purpose, and the neutrality of
`SqlSession` is load-bearing in a way that is easy to erode. Concrete driver row, bind and
error types typically have private constructors, so a host driver handed such a type could
neither be called (it cannot construct opaque binds) nor return (it cannot construct rows
or errors). Widening either seam with a concrete driver type closes it to every host but
the one that shipped the type.

Zero tokio still binds the shipped closure. `compio` is a dev-dependency of the vendor
crates, and the live-database test drivers (`postgres`, `mysql`) pull tokio transitively
on dev edges only; `cargo tree -p zeroship-migrate -e normal` stays free of both.

## Open

Nothing open.

## History

The deliberation is in this file's git history (`a21dc8e2f` is the implementing commit)
and in the follow-on proposals it fed: `2026-07-10-migrate-pg-driver-seam-design.md`,
the three `2026-07-11-migrate-napi-*.md` documents, and
`2026-07-12-zero-migrate-redesign-plan.md`.

Six notes are worth carrying forward, because each records a way this work went wrong.

1. Do not read a consumer's `default-features = false` as removing anything when the
   dependency is declared `workspace = true`. Cargo ignores it unless the root
   `[workspace.dependencies]` entry itself sets `default-features`. It was tried, and V8
   stayed in the tree while every check looked green. The load-bearing edit is in the root
   manifest - which then flips every `workspace = true` consumer to features-off and makes
   each one opt back in explicitly.

2. Do not accept `cargo build -p <crate> --no-default-features` as proof that a feature
   left the platform. It proves only that the crate compiles that way. Resolver feature
   unification re-enables the feature for any ordinary workspace build the moment one
   other path depends with defaults on, so the isolated command can pass while the product
   links the feature everywhere it matters. Only a workspace-level assertion measures the
   goal.

3. Do not assert absence with `cargo tree -p <crate> | grep -c v8 == 0` for a crate that
   has its own direct edge to the thing. That criterion was written against
   `zeroship-data-v8`, which is itself a native V8 plugin, so it could never pass and
   said nothing about the edge it was meant to measure. Attribute the edge with a reverse
   tree (`cargo tree -i v8`) instead.

4. Do not treat a features-off *library* build as covering the crate. A features-off test
   run compiles every `tests/*.rs`, every `[[bin]]` that lacks `required-features`, and
   every doctest on an ungated public item. Each is an independent way for a gated symbol
   to break the build that the library check cannot see.

5. Do not trust a grep sweep scoped by an assumed inventory of coupling sites. One such
   sweep here reported "outside the two subsystems and the two binaries: zero hits" while a
   third V8 binary existed and was silently outside the scope. The compile under the target
   configuration is the exhaustive proof; the grep is only a map of where to look first.

6. Do not move a re-export block behind a gate wholesale when it interleaves names from
   both sides of a cut. The mechanical move deletes exactly the neutral names downstream
   crates consume, and it fails at the dependent rather than at the crate being changed.
   Split the block by consumer, name by name.
