# zeroship distributed architecture — scale to millions

> Thinking through what "serve millions of users" actually demands.
> Leads with the architectural insight, works back to concrete choices.
> Ends with sequenced deltas from where we are today.

---

## The scale we're actually targeting

"Millions of users" isn't one number. Break it apart:

| Layer | Realistic year-2 | Stretch year-3 |
|---|---|---|
| Creators (apps deployed) | 100K | 1M |
| End users across creator apps | 50M | 500M |
| Daily requests | 500M | 10B |
| Peak req/s | 50K | 500K |
| Total uploaded bytes | 100 TB | 10 PB |
| Creators with paid customers | 10K | 100K |

Our single worker benchmarks at 512K req/s WinterCG. **Raw compute isn't the bottleneck** — distribution, isolation, and cold-start economics are.

The key shape difference from classic PaaS:

> **Millions of apps, most of them idle almost all the time, occasional small bursts.**

Classic PaaS (Heroku, Railway, Render) assumes: few apps, each with meaningful traffic. Our thesis needs: many apps, each mostly silent. The architectural implications are profound and push us toward Cloudflare Workers' shape, not Heroku's.

---

## The critical architectural insight

**zeroship is Cloudflare Workers + Stripe Connect** — not Vercel + a dashboard.

Cloudflare nailed the "millions of idle apps, cheap isolates, no cold start" economics through:
- V8 isolates, not containers (~5ms cold start, MB not GB memory)
- Snapshot-based warm pools
- Soft multi-tenancy (shared everything, hardened sandbox)
- Anycast edge routing

We already have much of this. What CF *doesn't* have is the creator-economy layer:
- Per-app revenue tracking
- Platform fee collection (15% Shopify-style)
- Creator-to-end-user payout (Stripe Connect)
- Per-app pricing tiers that end users pay
- AI-generated app lifecycle

**Our architecture should steal CF's compute shape wholesale and bolt on the monetization layer as the product moat.**

---

## Current state inventory

What we have today (from AGENTS.md + this codebase):

| Layer | Status | Notes |
|---|---|---|
| V8 runtime | ✅ | 512K req/s WinterCG, compio/io_uring, zero tokio |
| Per-app bundle loading | ✅ | On-demand + LRU eviction |
| CHWBL worker routing | ✅ | XXH3, 150 vnodes |
| Worker fleet | ✅ | Docker Compose, 50 workers tested |
| Control plane | ✅ basic | HTTP pull every 5s, single-region |
| Gateway | ✅ | JWT validation, UDS support |
| PostgreSQL driver | ✅ | compio-native, pooled, HikariCP-style |
| Storage plugin | ✅ just refactored | Backend trait; only LocalFs impl |
| KV plugin | ✅ | In-memory only |
| Auth service | ✅ | Email/pw + OAuth + JWT |
| Deploy | ✅ basic | `zeroship deploy` + .appbundle |
| Observability | ❌ | Scattered logs, no aggregation |
| Metering | ❌ | Not implemented |
| Stripe Connect | ❌ | Not implemented |
| Multi-region anything | ❌ | Single-region only |
| V8 snapshots | ❌ | Cold start is ~100-500ms, not <10ms |
| Egress policy | ❌ | User code can `fetch()` anywhere, no limits |
| Custom domains + SSL | ❌ | Subdomain only |
| Sharded DB | ❌ | Single Postgres, all apps share it |

**Verdict**: solid compute + dev experience, missing almost everything a scale platform needs.

---

## Target architecture

```
                    DNS: <app>.zeroship.app | <custom>.com
                                   │
                              (anycast)
                                   ▼
        ┌─────────────────────────────────────────────────┐
        │  Edge POPs — 20+ initially, 100+ eventually     │
        │                                                 │
        │  ┌───────────────────────────────────────────┐  │
        │  │ TLS termination (ACME automation)         │  │
        │  │ WAF + DDoS (CF / Fastly partnership?)     │  │
        │  │ Static asset CDN cache                    │  │
        │  │ JWT validation                            │  │
        │  │ Per-app + per-IP rate limit               │  │
        │  │ Route lookup (pushed; NOT polled)         │  │
        │  │ Meter emit → event bus                    │  │
        │  └───────────────────────────────────────────┘  │
        └─────────────────────────────────────────────────┘
                                   │
                        (nearest origin region)
                                   ▼
  ┌────────────────────────────────────────────────────────────┐
  │  Origin region (us-east, eu-west, ap-south initially)      │
  │  ┌──────────────────────────────────────────────────────┐  │
  │  │  Worker fleet                                        │  │
  │  │  - V8 isolate per app                                │  │
  │  │  - Snapshot-restored start <10ms                     │  │
  │  │  - Pre-warm pool for top-K apps                      │  │
  │  │  - Hard CPU/mem/wall limits per request              │  │
  │  │  - Per-app egress firewall                           │  │
  │  │  - CHWBL shard routing                               │  │
  │  └──────────────────────────────────────────────────────┘  │
  │       │                    │                    │          │
  │       ▼                    ▼                    ▼          │
  │  ┌────────────┐    ┌─────────────┐    ┌────────────────┐   │
  │  │ Postgres   │    │ KV          │    │ Object storage │   │
  │  │ (sharded,  │    │ (Dragonfly / │    │ (R2/S3)       │   │
  │  │ tiered)    │    │  Upstash)   │    │                │   │
  │  │            │    │             │    │  - per-app     │   │
  │  │ free: RLS  │    │ per-region  │    │    prefix      │   │
  │  │ paid: Neon │    │ cluster     │    │  - CDN cache   │   │
  │  │ ent:  own  │    │             │    │  - lifecycle   │   │
  │  └────────────┘    └─────────────┘    └────────────────┘   │
  └────────────────────────────────────────────────────────────┘
                                   │
                          (async, not request path)
                                   ▼
  ┌────────────────────────────────────────────────────────────┐
  │  Control plane — multi-region, CockroachDB-backed          │
  │  - App / route / secret CRUD                               │
  │  - Deploy orchestration (atomic, rolling, rollback)        │
  │  - Route propagation via event bus (NATS JetStream)        │
  │  - Metering aggregation + Stripe fee collection            │
  │  - Per-creator Stripe Connect accounts                     │
  │  - Audit log                                               │
  └────────────────────────────────────────────────────────────┘
                                   │
                                   ▼
  ┌────────────────────────────────────────────────────────────┐
  │  Observability — tiered retention                          │
  │  - Hot (24h): ClickHouse, per-app queries                  │
  │  - Warm (30d): S3 Parquet                                  │
  │  - Cold: Glacier                                           │
  │  - Traces: OTel, 1% sampled + errors always                │
  └────────────────────────────────────────────────────────────┘
```

---

## The ten architectural choices

Each choice has a pick + rationale + when the decision is revisitable.

### 1. Edge-native vs region-native vs container-native

Three shapes in the market:

| Shape | Who | Cold start | Per-app cost | Fit for us |
|---|---|---|---|---|
| Edge-native | CF Workers, Deno Deploy, Vercel Edge | 5-50ms | ~$0 idle | **Best for compute** |
| Region-native | Vercel (Lambda), AWS Lambda | 100-500ms | ~$0 idle | Too slow cold start |
| Container-native | Fly.io, Railway, Heroku | 500ms-5s | $$-$$$ | Doesn't scale to 1M apps |

**Pick: edge-native compute + region-native data.** Code runs at the edge (or close to it) via V8 snapshots. Data lives in ~3 regions (us-east, eu-west, ap-south). Gateway routes user → nearest edge → nearest region where app data lives.

**Revisit when**: a significant fraction of creator apps need >30s wall time (e.g., video processing). Then add "workers in container" tier for those specific apps.

### 2. Isolation model

Options by trust level:

| Model | Isolation | Cold start | Cost/app |
|---|---|---|---|
| Shared isolate | Weak | 0ms | $0 |
| V8 isolate + limits | Medium | 5-50ms | ~$0 |
| Process per app | Strong | 100-500ms | ~$0.1/mo |
| Firecracker VM per app | Nano-grade | 1-5s | $$-$$$ |

**Pick: V8 isolate per request with hard limits.** Not per-app-persistent; per-request ephemeral isolate, reset between requests. This is what CF does.

Trust model: AI-generated code is "medium-trust" — we don't expect deliberate malice, but we do expect buggy infinite loops and accidental resource exhaustion. Hard CPU/wall/mem timers + egress firewall are non-negotiable.

**Revisit when**: real malicious actor shows up. Then tier: free = V8 isolate, enterprise = Firecracker.

### 3. Cold start path

Today: ~100-500ms per cold bundle load. Not viable for "most apps idle."

Path to <10ms:
- **V8 startup snapshots**: compile user bundle + run top-level into a serialized heap image. Restore from snapshot = <5ms. V8 supports this natively.
- **Pre-warm pool per shard**: top-K apps (by recent traffic) keep a hot isolate ready.
- **Predictive wake**: ML on traffic patterns predicts which apps will hit next; pre-wake them.

**Pick: V8 snapshots in v1, pre-warm pool in v2.** Predictive wake is a v3 problem.

V8 snapshot work is ~2 weeks of focused engineering. Non-trivial but unlocks the whole economic model.

**Revisit when**: actual measured cold-start p99 stays above 50ms with real traffic.

### 4. Database at scale

The "one Postgres with schema per app" model breaks at ~1K apps (connection count ceiling, DDL locking).

**Three-tier model**:

| Tier | DB | Isolation | Cost/app |
|---|---|---|---|
| Free | Shared Postgres cluster + RLS | Row-level | ~$0.01/mo |
| Paid | Neon branch per app | Storage-compute separated | ~$0.50/mo |
| Enterprise | Dedicated Postgres instance | Hardware | $20+/mo |

**Shards**: at the free tier, route `app_id → shard_id → cluster_id`. ~1K apps per cluster. 1K clusters for 1M free apps. Shard routing via consistent hashing with rebalancing.

**Migrations**: per-app migrations use the existing `plugin-db` registerModel pattern. For free tier, migrations run within the RLS-scoped rows. No cross-app DDL locking.

**Pick: shared cluster + RLS for free tier from day one.** Neon branches for paid tier. Dedicated clusters for enterprise (years away).

**Revisit when**: shared-cluster contention bites — probably at ~10K apps per cluster.

### 5. KV at scale

In-memory per-worker (today) doesn't survive restart and doesn't share across workers.

**Pick: Dragonfly per region, app_id prefix isolation.** Dragonfly is Redis-compatible, faster, multi-threaded, memory-efficient.

Free tier: shared cluster, app_id prefix.
Paid tier: per-app database index (Redis SELECT).
Enterprise: dedicated cluster.

Upstash is the managed alternative. Cheaper for early scale but pay-per-request adds up.

**Revisit when**: KV ops exceed 10K/s per region. Either shard or upgrade tier.

### 6. Storage at scale

User uploads to millions of apps → PB scale.

**Pick: Cloudflare R2 as primary, S3 as fallback for AWS-partnership reasons.**

Why R2:
- **Zero egress fees** — the killer differentiator. End-user requests for uploaded files cost us nothing in egress.
- S3-compatible API — our trait refactor already prepares for this
- Multi-region with automatic replication

Configuration: one R2 bucket per region, all apps in that region store there with `<app_id>/` prefix. Creator-facing: `<app>.zeroship.app/uploads/...` transparently proxies through gateway with auth check.

**Per-app quotas** enforced at the plugin layer before calling R2. Prevents runaway storage costs.

**Revisit when**: R2 pricing or reliability changes, or we need features R2 doesn't support (lifecycle rules, Glacier tiers).

### 7. Gateway / edge routing

Two options:

**a. Build our own**: more work, more control, better margins.
**b. Cloudflare Workers as our edge**: fastest to market, CF takes cut.

**Pick: CF/Fastly as the edge layer initially, build our own when scale justifies.**

At < 10K creators, paying CF for edge POPs is vastly cheaper than running our own edge presence globally. At 100K+ creators, owning the edge is a meaningful margin improvement.

Rough crossover: $50K/mo in edge costs = worth owning. Until then, rent.

Gateway-at-edge responsibilities:
- TLS + DDoS (CF native)
- JWT validation (port our gateway logic to a Worker)
- Route lookup (CF KV-backed)
- Rate limit (CF KV counters)
- Meter emit (tail log → our control plane)

**Revisit when**: edge bill hits $50K/mo OR we want features CF doesn't support (specific routing policies, metering detail).

### 8. Deploy atomicity

What consistency guarantee do we give creators on deploy?

| Level | Behavior | Complexity |
|---|---|---|
| Eventual | Some users see old, some new, up to 30s | Easy |
| Request-boundary | Each request sees exactly one version | Medium |
| Transactional | Flush in-flight + switchover | Hard |

**Pick: request-boundary atomicity.**

Mechanism: each incoming request picks a bundle version at the gateway based on the current route table. In-flight requests continue with their selected version; new requests after the route flip hit the new version. No user-visible inconsistency. No in-flight flush needed.

Rollback: route flip back. <10s globally.

**Revisit when**: creators need true transactional deploy (e.g., schema migration that can't coexist with old code). At that point, add a "quiesce" mode that blocks new requests briefly.

### 9. Observability at scale

Per-app logs, metrics, traces at 10B req/day = ~10 TB/day raw.

**Tiered retention**:
- **Hot (0-24h)**: ClickHouse, full request detail per app. Creator dashboard queries hit this.
- **Warm (1-30d)**: S3 Parquet, aggregated per-minute metrics + sampled request logs. Creator queries fall back to this with 1-10s latency.
- **Cold (30d+)**: Glacier. On-demand restore for compliance.

**Traces**: OpenTelemetry, 1% sampled by default, 100% on errors. Stored in the hot tier.

**Per-app dashboard**: creator queries their own logs/metrics through gateway API. Auth-scoped — can't see other apps' data.

**Pick: ClickHouse for hot tier from day one.** It scales to TB without exotic ops. Vector / Fluent Bit for collection.

**Revisit when**: hot tier exceeds 1TB. Shard ClickHouse cluster or move older data to warm faster.

### 10. Event bus for control plane

Route propagation from control plane → 100 edge POPs needs to be <1 second end-to-end.

Options:
- NATS JetStream: lightweight, Go-native, persistent streams, <10ms propagation
- Kafka: proven, but heavy (ZooKeeper/KRaft)
- Redpanda: Kafka-compatible, lighter
- Custom HTTP long-poll: simple, doesn't scale past ~1K subscribers

**Pick: NATS JetStream.** Simple ops, fast, persistent, proven at scale (used by Synadia, etc).

For metering (high volume): also NATS JetStream initially. Swap to Kafka/Redpanda when emitting >1M events/sec.

**Revisit when**: event volume exceeds ~500K/sec.

---

## Gap analysis

Sorted by "what's blocking what."

### Tier A — blocks going beyond 100 creators (ship in Q2)

1. **Stripe Connect + metering pipeline** — creators can't monetize without it. 4-5 days.
2. **Custom domains + ACME automation** — creators need their own domains. 2-3 days.
3. **Real deploy pipeline** — atomic, rolling, rollback. Uses route-table flips. 3-4 days.
4. **Per-app resource limits (hard)** — CPU, memory, wall time. Most of this exists; harden it. 1-2 days.
5. **Egress policy** — block known-abuse domains, rate-limit outbound. 1-2 days.
6. **S3/R2 storage backend** — plugin-storage's S3 impl. 1-2 days.
7. **Observability: minimum viable** — per-app logs visible to creator, error aggregation. 2-3 days.

Total: ~3 weeks focused work. Unlocks: first 1K creators monetizing real apps.

### Tier B — blocks going beyond 10K creators (ship in Q3)

8. **V8 snapshots for <10ms cold start** — the economic unlock. 2-3 weeks.
9. **Sharded Postgres with per-tenant RLS** — routing layer, migration pipeline. 1-2 weeks.
10. **Distributed KV (Dragonfly cluster per region)** — 1 week.
11. **Event bus (NATS) for route propagation** — replace HTTP polling. 3-5 days.
12. **Observability tiers** — ClickHouse + S3 parquet. 1 week.
13. **Edge gateway via CF Workers** — port JWT/rate-limit/route-lookup. 1 week.
14. **Deploy to multiple regions** — control plane knows about regions; creator picks. 1 week.

Total: ~8 weeks. Unlocks: 10K creators with real end-user traffic.

### Tier C — blocks going beyond 100K creators (ship in Q4 / Q1 2027)

15. **Multi-region data** — not just gateway; DB/KV/storage replicated.
16. **App migration across regions** — creator moves app home region.
17. **Dedicated-tier DB infrastructure** — Neon branches per paid app.
18. **Predictive cold-start** — ML-driven pre-warming.
19. **Audit log at scale** — compliance-grade immutable log.
20. **Cross-region replication for premium** — active-active data.

Total: several months, product-dependent.

---

## Non-goals (explicit)

Things that look tempting but are traps at our stage:

1. **Kubernetes as primary control plane.** K8s caps at ~10K pods per cluster. We're shipping V8 isolates inside long-running worker processes — pod-per-app doesn't fit. Use K8s (or Nomad, or raw VMs) only to run the worker FLEET.
2. **Own global edge network.** At <10K creators, paying CF is cheaper than running 100 POPs. Revisit at scale.
3. **Multi-cloud from day one.** Pick AWS or GCP. Make everything else cloud-agnostic where it's cheap (storage backend trait). Multi-cloud is a year-3 problem.
4. **Service mesh.** Istio/Linkerd don't buy us anything at 10 services. Plain HTTP + mTLS is fine.
5. **Kubernetes-native app model.** We're not Heroku. Each app is a bundle, not a pod spec.
6. **PostgreSQL replication from day one.** Single-region primary with point-in-time backup is fine until we need multi-region reads. Creators' apps that need 99.99% uptime are rare.
7. **Our own object store.** R2/S3 is free-ish and battle-tested. Don't build Ceph.
8. **Our own DB engine.** Use Postgres. Every attempt to rebuild Postgres for "planet scale" eventually rebuilds Postgres.
9. **Hardware isolation (Firecracker) for everyone.** Only needed for enterprise tier with hostile-code risk.
10. **Custom TLS termination.** Use Rustls or Cloudflare's edge. Writing a TLS implementation is a two-year project.

---

## What this means for today's decisions

We're at the fork: the scaffold + storage + KV SDKs land in "small number of creators" mode. The next round of features either:
- **A.** keeps building creator-facing SDKs (email, auth, payments, dashboard) — finishes the single-region platform
- **B.** starts the scale-out work (V8 snapshots, sharded DB, edge gateway, metering) — heavier lift before any creator benefits

My recommendation: **A first, B starts Q3.**

Reasoning:
- First 100 creators validate the product. Zero of them will hit scale limits — we have headroom on the current single-region setup for ~500 creators.
- Stripe Connect + observability + deploy polish are blocking creators *right now*.
- V8 snapshots and sharded DB don't help anyone until we have meaningful traffic.
- Building scale infrastructure before product-market fit is how platforms die.

The architecture in this doc is a **destination**, not a Q2 sprint. We'll build toward it.

---

## Concrete sequencing recommendation

**Q2 (May-June) — Tier A: monetization-first**

| Week | Focus |
|---|---|
| 1 | Stripe Connect + metering pipeline (the moat) |
| 2 | Custom domains + ACME, deploy polish, per-app limits |
| 2-3 | S3/R2 storage backend, egress policy, creator dashboard |
| 4 | Observability MVP (per-app logs to creator) |

**Outcome**: 100-1K creators can ship monetized apps. First revenue flows. Platform fee collected.

**Q3 (Jul-Sep) — Tier B: scale foundations**

| Week | Focus |
|---|---|
| 5-7 | V8 snapshots for cold-start economics |
| 8 | Edge gateway via CF Workers |
| 9-10 | Sharded Postgres with RLS |
| 11-12 | NATS event bus for route propagation + observability tiers |

**Outcome**: 10K creators viable. Cold-start p99 <50ms. Platform doesn't melt at moderate scale.

**Q4 (Oct-Dec) — Tier C: multi-region, predictive**

Depends on traffic signals from Q2-Q3. If growth is linear, do Tier A polish + Tier B hardening. If we're approaching 10K creators, start Tier C.

---

## Risks

**Risk 1: We overbuild the architecture before validating demand.**
- Probability: high (this is the eternal infra trap)
- Mitigation: Tier A first, Tier B only after we have real creators hitting limits.

**Risk 2: We underbuild and hit a scale wall during launch.**
- Probability: low-medium
- Mitigation: current architecture handles 500-1K creators with current code. That's much more than the first few months of launch will produce.

**Risk 3: Cloudflare acquires or closes R2 discount pricing.**
- Probability: low (R2 is strategic for CF)
- Mitigation: storage trait abstraction means swap-out is 1-2 days.

**Risk 4: Stripe Connect onboarding friction kills creator activation.**
- Probability: medium (KYC is non-trivial globally)
- Mitigation: US-first for launch. Payout alternatives (Wise, Payoneer) for international tier 2.

**Risk 5: We build the edge on CF Workers and then CF terminates our account.**
- Probability: very low
- Mitigation: our gateway logic is in code we own. Rewriting for AWS CloudFront + Lambda@Edge would be 1-2 weeks.

**Risk 6: V8 snapshot work takes 4× longer than estimated.**
- Probability: medium (V8 embedder APIs are underdocumented)
- Mitigation: 2-3 weeks budget; if not working by week 4, evaluate alternatives (process pools, workerd-style resident processes).

---

## Things I'm uncertain about

Calling these out explicitly:

- **Is the CF-edge-as-our-edge plan actually viable?** CF charges per Worker invocation. At 10B req/day, that might be more expensive than running our own edge. Needs a pricing model before committing.
- **Does V8 snapshot support the full module graph?** Our runtime loads ES modules with top-level await. Snapshotting that has known V8 quirks. Needs a spike.
- **Do we need RLS from day one on free tier, or can we start with schema-per-app and shard later?** Schema-per-app is what we have today. Migrating 10K apps from schema-per-app to RLS is non-trivial.
- **Is Dragonfly production-ready for financial-adjacent KV workloads?** Session storage for paying customers can't lose data. Dragonfly's persistence story is young.
- **How much of this can we buy vs build?** Upstash (KV), Neon (DB), R2 (storage), CF (edge), Stripe (payments), Resend (email). The platform-from-primitives path is faster to market but lower-margin at scale.

Each of these deserves its own spike + doc before the implementing sprint starts.

---

## TL;DR

1. **We're Cloudflare Workers + Stripe Connect**, not Vercel + a dashboard. That's the shape.
2. **Current architecture handles ~500 creators comfortably.** Don't rebuild the stack yet.
3. **Tier A (monetization, deploy polish, observability) is Q2.** That's what real creators need.
4. **Tier B (V8 snapshots, sharded DB, edge gateway, event bus) is Q3.** Only if traffic signals demand it.
5. **Tier C (multi-region data, predictive pre-warm) is Q4+.** Product-maturity-dependent.
6. **10 explicit non-goals** — K8s-native app model, own edge network from day 1, multi-cloud, service mesh, etc. These are classic infra traps.
7. **5 concrete risks** tracked with mitigations.
8. **5 open technical questions** that need spikes before committing.

Next action: pick Tier A starting point. Stripe Connect is the tier's defining feature; starting there forces everything else into alignment.
