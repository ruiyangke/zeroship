# The metering transport is built, tested, and configured by nothing we ship

**Date:** 2026-08-20
**Status:** FINDING. Sections 1-6 are evidence. Section 7 is a recommendation,
not a decision: the fix proper needs the operator call already open as #108
(whether `metering.brokers` belongs in argv before SASL turns it into a
credential). Section 8 argues against the verdict and names the one input this
document could not read.
**Verdict:** **CONFIRMED** for every deployment shape this repository ships.
**PARTIAL** only in the narrow sense of section 5: the transport is wired, and
exercised, by nine e2e harnesses. No compose service has ever been pointed at
it.
**Scope:** the path from a usage counter to a durable stream record. Explicitly
NOT: pricing, the spend ladder, Stripe reconciliation, or Connect - all of which
sit downstream of the gap described here and are therefore unreachable from a
compose deployment, but none of which are themselves defective.

---

## 0. Reading conventions

Every claim about current behaviour carries a `file:line` against this working
tree at `93d9e0228`. Claims are labelled:

- **VERIFIED** - I read the cited lines myself.
- **MEASURED** - a number I produced by executing something, with the vehicle named.
- **INFERRED** - a conclusion drawn from verified facts, not itself read.
- **NOT CHECKED** - stated so the reader does not mistake silence for evidence.

---

## 1. What the transport is

**VERIFIED.** The path is complete and, read on its own, good.

A producer's counters live in `Meter` (`crates/metering/src/meter.rs`), an
atomic per-`(app_id, metric)` map. `Meter::drain`
(`crates/metering/src/meter.rs:214`) swaps every counter to zero under the write
lock and returns `UsageEvent`s. The outbox task
(`crates/metering/src/outbox.rs:741`) calls it every
`DEFAULT_OUTBOX_INTERVAL` (10s, `outbox.rs:52`) and hands the batch to
`UsageOutbox::publish_events` (`outbox.rs:422`), which appends to a worker-local
redb WAL first, publishes each event as one stream record keyed by app id, and
trims the WAL sequence only on a broker ack (`outbox.rs:483-553`). A failed WAL
append retains the batch verbatim in memory up to
`DEFAULT_MAX_RETAINED_EVENTS` (100_000, `outbox.rs:70`). The WAL is keyed on a
`WalIdentity` newtype (`outbox.rs:236`) precisely so a per-boot producer source
cannot be passed where a restart-stable name belongs.

**The transport is chosen in exactly one place.** `build_usage_outbox`
(`crates/metering/src/outbox.rs:178`), and its first statement is the whole
finding:

```rust
pub fn build_usage_outbox(
    producer_source: &str,
    wal: &WalIdentity,
    settings: &UsageStreamSettings,
) -> Result<Option<(UsageOutbox, OutboxConfig)>, String> {
    let Some(brokers) = settings.effective_brokers() else {
        return Ok(None);
    };
```

`effective_brokers` (`outbox.rs:131`) is `self.brokers`, trimmed, empty-filtered.
`brokers` comes from `UsageStreamSettings::from_resolved` (`outbox.rs:119`),
whose four arguments are the producer's four resolved `metering.*` settings.
On the worker those are `metering_brokers` and siblings
(`crates/worker/src/config.rs:175`), **compiled default `String::new()`**; on the
gateway the same (`crates/gateway/src/config.rs:205`). There is no other
channel: `UsageStreamSettings::from_env` was deleted on 2026-08-20 and the crate
now declares no env consumer at all (`crates/metering/src/lib.rs:32-37`).

Downstream, the control plane consumes with its own selector
(`crates/control/src/main.rs:788`), driven by `control.stream_transport`
(`crates/control/src/config.rs:110`), **compiled default `String::new()`**, with
a fallback to a `[metering] brokers` key in the file overlay.

---

## 2. What happens when no transport is configured

**VERIFIED. The answer is (d) then (b): a no-op that looks healthy, announced
by a log line.**

### Producers

Both take the `Ok(None)` arm and spawn a drain-and-drop task. Worker
(`crates/worker/src/main.rs:517-523`), gateway
(`crates/gateway/src/main.rs:545-551`), both calling:

```rust
pub fn spawn_disabled_drain_task(meter: Arc<Meter>, interval: Duration, reason: String) {
    compio::runtime::spawn(async move {
        tracing::warn!(
            reason = %reason,
            "{OUTBOX_DISABLED_LOG}; every usage event will be drained and dropped"
        );
        loop {
            compio::time::sleep(interval).await;
            let events = meter.drain();
            if !events.is_empty() {
                tracing::warn!(
                    events = events.len(),
                    reason = %reason,
                    "{OUTBOX_DISABLED_LOG}; dropped usage events"
                );
            }
        }
    })
    .detach();
}
```
(`crates/metering/src/outbox.rs:790-809`)

`events` is bound, counted, and dropped at the end of the loop body. **This is
not a buffer-and-retry that I misread.** The retry machinery
(`retain_for_retry`, the redb WAL, `has_pending_retry`) is all method-on-
`UsageOutbox`, and in this arm no `UsageOutbox` was ever constructed -
`build_usage_outbox` returned before building the transport, the config, or the
WAL. There is nothing holding the events and nothing that could replay them.

The process is otherwise entirely healthy. `/readyz` is unaffected; it gates on
the control-plane version poll and blob-store reachability
(`crates/worker/src/main.rs:457`), neither of which knows metering exists.

### Consumers

Control takes the `None` arm (`crates/control/src/main.rs:896-899`) and logs
`"control: billing event stream disabled; stream forwarding and spend recompute
are disabled"`. `AppState.billing_stream` is `None`, so:

- a second warning fires at cron startup: `"billing stream transport is not
  configured; usage metering/enforcement is disabled (no old-model fallback)"`
  (`crates/control/src/cron/mod.rs:114-118`);
- the whole `if let Some(streams) = state.billing_stream.as_ref()` block is
  skipped (`crates/control/src/cron/mod.rs:257`), so neither the event forwarder
  nor spend recompute is ever spawned.

### What an operator would see

Three `WARN` lines at boot (one per worker replica, one on the gateway, two on
control) and then a `WARN` every 10 seconds per producer for the life of the
process, in JSON, into `docker logs`. **That is the entire signal.** There is no
metric, no `/readyz` effect, no health degradation, no alert. A log line at
`WARN` repeating every 10s in a service that logs at
`info,zeroship_=debug` (`deploy/ops/zeroship.toml:49`) is noise, not a signal.

`--check-config` does report it: `usage_stream_configured=false`
(`crates/worker/src/main.rs:340-344`). But the report is informational -
`report.emit(...); return Ok(());` (`crates/worker/src/main.rs:350-351`) - and
the only consumer of `--check-config` reads its **exit code** and discards
stdout:

```sh
if rsh "cd '$REMOTE_DIR/compose' && ZEROSHIP_IMAGE='$IMAGE' $CHECK_RUN ${svc#*:} ${svc%%:*} --check-config" >/dev/null 2>&1 </dev/null; then
```
(`deploy/scripts/deploy-remote.sh:987`)

So the deploy gate that exists to catch exactly this class of thing reports
`ok worker` on a worker that will bill nobody.

---

## 3. Whether the shipped deployment is in that state

**VERIFIED. Yes, in the compose file, the ops overlay, and the deploy script -
every artefact this repository ships.**

`deploy/compose/docker-compose.yml` declares `redpanda`
(`docker-compose.yml:661`) with no profile, an `auto_create_topics_enabled=true`
broker on `internal://redpanda:9092`, a healthcheck, and a persistent
`redpanda-data` volume. `deploy/scripts/deploy-remote.sh:1054` runs
`docker compose up -d --remove-orphans`, so on the deployed host **the broker
runs.** The stateful-volume gate even keeps it in its
`STATEFUL` list (`crates/zeroship-gatekit/src/stateful_volume.rs:51`) so its
volume cannot regress.

Nothing publishes to it and nothing consumes from it:

| service | metering config in compose | producer/consumer state |
| --- | --- | --- |
| `worker` (`docker-compose.yml:529-616`) | none in `command:` (547-568) or `environment:` (579-589) | `Ok(None)` -> drain-and-drop |
| `gateway` (`docker-compose.yml:441-528`) | none in `command:` (448-467) or `environment:` (486-507) | `Ok(None)` -> drain-and-drop |
| `control` (`docker-compose.yml:226-368`) | no `--stream-transport`, no `ZEROSHIP_CONTROL_STREAM_*` (241-353) | `None` -> forwarder + recompute never spawn |

`deploy/ops/zeroship.toml` - the shared overlay that gateway and control
bind-mount at `/etc/zeroship/zeroship.toml` (`docker-compose.yml:485`, `:360`) -
has **no `[metering]` table at all**; its sections are `[auth]` and
`[observability]`. `grep -n "metering\|broker\|redpanda\|stream"` over both
overlay files returns four hits, all of them `broker_secret` (the OIDC client-
secret derivation key, an unrelated use of the word) in the *example* file.

**`.env` cannot close this gap, and that is decidable from the repo.** Compose
injects `.env` values only where it interpolates `${VAR}`. `docker-compose.yml`
contains no `${...METERING...}` and no `${...STREAM...}` interpolation, there is
no `env_file:` directive in any file under `deploy/compose/`
(`grep -n env_file deploy/compose/*.yml` -> exit 1), and no service uses the
bare-key list form that passes a host variable through. A `ZEROSHIP_METERING_BROKERS=redpanda:9092`
line in `compose/.env` would configure nothing.

### Since when

**VERIFIED, 43 days.** `redpanda` entered compose in `c30c3baac`
(2026-07-08, *feat(stream): add StreamTransport registry and Redpanda adapter*).
`git log -S METERING_BROKERS -- deploy/compose/docker-compose.yml` returns
exactly one commit, `d2dd39395` (2026-08-20), and its compose diff is a comment
edit:

```diff
   # redpanda - Kafka-wire durable stream for the billing-provider platform.
-  # In-compose services should use REDPANDA_BROKERS=redpanda:9092 when S4/S5 wire
-  # the forwarder and worker producer.
+  # The SERVICES do not: the worker and gateway producers take
+  # `--metering-brokers` / `ZEROSHIP_METERING_BROKERS` (canonical
+  # `metering.brokers`, ...). In compose that value is `redpanda:9092`.
```

This is the finding in miniature. The old text was an honest **TODO** - "should
use ... when S4/S5 wire". The new text is a **declarative sentence about a
configured value** - "In compose that value is `redpanda:9092`" - written
beside a file in which that value appears nowhere. S4/S5 wired the producers'
*interface*; nobody wired the *deployment*, and the comment rewrite removed the
only marker that said so.

---

## 4. Blast radius

**VERIFIED, by metric.** Every metric the platform measures is dropped. The
producers are the worker, the gateway, and control's own outbox.

- **Worker platform counters**, five, emitted once per dispatched request via
  `Meter::record_request` (`crates/worker/src/cache.rs:251,256`, reached from
  five call sites in `crates/worker/src/handler.rs`): `requests`, `cpu_us`,
  `wall_us`, `egress_bytes`, `ingress_bytes`.
- **Data-primitive usage metrics**, emitted by `env.db` / `env.kv` /
  `env.storage` at their op boundary through `MeterHandle::record`
  (`crates/metering/src/lib.rs:80`).
- **`gateway_egress_bytes`**, for the static, redirect and error bodies the
  worker never sees (`crates/gateway/src/router/dispatch.rs:1662`,
  `crates/gateway/src/router/streaming.rs:73`, reached from
  `static_serve.rs:327,357`).
- **`net_egress_bytes` / `net_ingress_bytes`**, the raw-socket counters the V8
  runtime emits for `node:net` traffic
  (`crates/runtime/src/node/net/caps.rs:266,274`).
- **Control's own usage outbox** (`start_control_usage_outbox`,
  `crates/control/src/main.rs:851`) is inside the `Some(id)` transport arm and
  therefore never started either.

**INFERRED, and this is the part that matters commercially.** Because
`usage_aggregates` is fed only by spend recompute, which is inside the skipped
block:

1. **No app is ever invoiced for anything.** `charge = base_fee + sum(overage)`
   with every usage term structurally zero.
2. **The spend ladder never fires.** Warn / Degrade / Block are driven by spend
   against the limit; spend is zero, so no app is ever throttled or 402'd, at
   any traffic volume. The free tier is not "quota-capped by construction"; it
   is uncapped.
3. **`GET /api/apps/{id}/usage` returns zeroes.** It reads `usage_aggregates`
   (`crates/control/src/api.rs:1419,1437`), which no writer populates. A creator
   dashboard shows a working, healthy, zero-usage app.

Every one of those is indistinguishable from an app that served no traffic.

### Is anything recoverable?

**VERIFIED: no, not from anything this repository writes.**

- **Not from the counters.** `Meter::drain` swaps each counter to zero
  (`meter.rs:214-250`) and the disabled task drops the result. At any instant a
  process holds at most 10 seconds of unwritten usage.
- **Not from the WAL.** No `UsageOutbox` is constructed in this arm, so no
  `.zeroship/usage-outbox-*.redb` file is ever created.
- **Not from the database.** `usage_aggregates` has no other writer.
- **Not from application logs.** Neither the worker nor the gateway emits a
  per-request access line carrying app id plus byte counts; the worker's two
  `tracing::info!` sites in `handler.rs` (`:1476`, `:1557`) are not that.
- **Not from Caddy.** `deploy/ops/Caddyfile` contains no `log` directive, and
  Caddy 2 writes no access log without one.

**NOT CHECKED, and an operator lead rather than a claim:** the deployment sits
behind Cloudflare (`docker-compose.yml:508-514`). Cloudflare's per-hostname
analytics would give request counts and response bytes per `{app}` subdomain -
two of the five platform counters, at Cloudflare's retention, with no `cpu_us`,
no `wall_us`, and none of the db/kv/storage metrics. That is a reconstruction of
roughly the cheapest part of the bill, and I have not verified it exists.

---

## 5. The PARTIAL nuance: where it IS wired

**VERIFIED.** The producer path is not dead code, which is exactly why the gap
survived. Nine harnesses pass real brokers on the command line:

`tests/e2e_metering_billing.sh:361`, `tests/e2e_spend_state_transitions.sh:150`,
`tests/e2e_account_status_enforcement.sh:146`, `tests/e2e_real_app_end_to_end.sh:124`,
`tests/e2e_lago_billing.sh:184`, `tests/e2e_multi_metric_billing.sh:138`,
`tests/e2e_openmeter_export.sh:150`, `tests/e2e_multi_app_attribution.sh:172`,
`tests/e2e_db_app_end_to_end.sh:282` - each `--metering-brokers "$RP_BROKERS"`.

They even guard against the exact disabled state, at the top of the stack:
`tests/lib/usage_producer.sh` greps each producer's log for the started line and
**fails** on `OUTBOX_DISABLED_LOG`, with a comment explaining that a disabled
producer makes every downstream billing assertion run against genuine silence.

That guard is excellent and it is aimed at the harness. It reads a log file the
harness itself launched. Nothing points it at `deploy/`.

So the honest statement of shapes:

- **e2e harness shape** - wired, exercised, guarded. Not a deployment.
- **compose shape** (`deploy/compose/docker-compose.yml`, what
  `deploy/scripts/deploy-remote.sh` ships and starts) - **not wired**, and never
  has been.

---

## 6. Two adjacent defects found on the way

**6a. The operator checklist configures only half the rail.**
`docs/reference/billing-metering.md:222-227` step 2 tells the operator to set
`ZEROSHIP_CONTROL_STREAM_TRANSPORT` and `ZEROSHIP_CONTROL_STREAM_CONFIG`. It
never mentions the producers. An operator who follows it verbatim gets control
consuming a topic that nobody publishes to - the same zero bill, now with a
consumer group and a green-looking stream.

**6b. `crates/metering`'s test suite says nothing about the disabled arm.**
**MEASURED:** `cargo test -p zeroship-metering --lib` -> 14 passed, 0 failed,
0.11s. Reading the names: six cover `Meter`, seven cover `UsageOutbox` (WAL
reopen, restart identity, retain-and-retry, cap-and-drop-oldest), one covers
`MeterHandle`. **None constructs a disabled producer.** The one assertion that
default settings yield `!producer_enabled()` lives in the *worker* crate
(`crates/worker/src/config.rs:425`) and rules on the settings struct, not on
what the process then does with it.

---

## 7. The smallest change that makes this VISIBLE

Deliberately not the fix. Configuring brokers is blocked on #108 (whether
`metering.brokers` belongs in argv before SASL makes it a credential), and on
whether this deployment wants billing on at all. What should not wait is that
the state is currently indistinguishable from a working one.

**Recommended: a compose gate, in the family that already exists.**
`tests/compose_*_gate.sh` already rules on port exposure, secret strength and
stateful volumes. Add one arm with a floor, per `tests/lib/gate_arms.sh`: for
each backing service compose declares (`postgres`, `redis`, `redpanda`), count
the services configured to reach it. `postgres` and `redis` clear it;
`redpanda` scores zero, and the gate goes red today and stays red until a
service names it or the service is removed. This has the property the brief
asks for: the gap becomes a failing check rather than a `WARN` in a log nobody
reads, and it costs no operator decision to land.

**Also recommended, and independent:**

- Delete the sentence "In compose that value is `redpanda:9092`"
  (`docker-compose.yml:660`). It is the only line in the tree that asserts this
  is configured. Replace it with what is true: no service is pointed at this
  broker; billing is off in this deployment.
- Add the producer step to the operator checklist (6a) so the two halves are one
  instruction.
- Have `deploy-remote.sh` print each server's `usage_stream_configured` from the
  `--check-config` output it already runs and currently discards. Not a refusal
  - a no-metering deployment stays supported, per
  `crates/worker/src/main.rs:508-516` - but a roll should state which posture it
  just rolled.

**Not recommended: making the producers fatal on absent brokers.** The `Ok(None)`
arm is load-bearing and the comment at `crates/worker/src/main.rs:508-516`
argues it correctly: `zeroship dev` and every non-billing harness run a
broker-less worker.

---

## 8. Arguing against this verdict

**"You read a dev-tier default and called it the production path."**
The distinguishing test is what `deploy/scripts/deploy-remote.sh` ships, and it
is decidable: `SNAPSHOT_FILES="compose/.env compose/docker-compose.yml
ops/Caddyfile ops/zeroship.toml"` (`:356`), then
`docker compose up -d --remove-orphans` (`:1054`) in `$REMOTE_DIR/compose`. The
tracked compose file and the tracked overlay *are* the production inputs by
construction of the deploy script. `.env` is ruled out separately in section 3.

**"A fallback you read as drop is really buffer-and-retry."**
Distinguished by which type owns the retry. Buffering lives on `UsageOutbox`;
`build_usage_outbox` returns `Ok(None)` **before** constructing one, so in the
disabled arm no WAL file, no retained `VecDeque`, and no `Arc<dyn
StreamTransport>` exists. `spawn_disabled_drain_task` receives only an
`Arc<Meter>`, a `Duration` and a `String` - it has no handle on which to buffer.

**"The transport being constructible is not the same as configured."**
Agreed, and that cuts my way here rather than against it: section 5 shows it is
constructible *and* constructed, in tests. That is the strongest argument that
this is real rather than stale - the code works, and a passing e2e suite is
precisely what has made the deployment gap invisible for 43 days.

### The one input I could not read, and did not

**`docker-compose.override.yml` on the deployed host is not in this repository.**
`deploy/scripts/deploy-remote.sh:965-972` and `docs/runbooks/deploy-server.md:102`
both confirm it exists and is server-only. The runbook documents its intended
contents - the image tag, a loopback `control` port (`:254-264`), and
`migrate: volumes: !override []` (`:499-515`) - none of which is metering. But
that is a description of intent, not a snapshot of the file. **An override
setting `ZEROSHIP_METERING_BROKERS` on `worker` and `gateway` would refute
sections 3 and 4 outright**, and nothing in this tree can rule it out.

Closing it needs one read-only command on the production host:

```
cd $REMOTE_DIR/compose && docker compose config | grep -i 'METERING\|STREAM_TRANSPORT'
```

Production access is not mine, so **I stopped here rather than run it.** Until
someone does, read this document as: *the repository configures no transport,
and the deployment configures none unless an untracked host file does what no
tracked artefact and no runbook describes.*
