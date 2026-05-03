# Sandbox preview URLs / port forwarding design

**Date:** 2026-05-02
**Status:** Draft v7 — round-6 revisions (security re-audit: invariants 1+2)
**Audience:** sandbox/controller, sandbox-agent, gateway, builder-frontend
**Depends on:**
- `crates/sandbox/src/backend/mod.rs:117-255` — Backend enum (Docker / K8s / NomadCh) and its unified API.
- `crates/sandbox/src/backend/mod.rs:95-115` — `SandboxInfo` (the per-sandbox public record; **does NOT today carry `signing_key` or `agent_url`** — see § II.0 Lift below).
- `crates/sandbox-agent/src/sig.rs:1-321` — wire-protocol-v1 (Ed25519, 5 s skew, 30 s nonce LRU). Note: `verify_signed` rejects query strings outright (`handlers.rs:110`); see § II.1 Query-string handling.
- `crates/sandbox-agent/src/version.rs:35-45` — `CAPABILITIES` strings (controllers feature-detect via this list, not the protocol version).
- `crates/sandbox/src/backend/nomad_ch.rs:157-167` — today's `signing_key: Arc<SigningKey>` lives **only** inside the Nomad-CH `SandboxRecord`; Docker has no signed-RPC path; K8s uses a `Bearer` token. See § II.0.
- `crates/sandbox/src/registry.rs:80-86` — `SandboxRegistry::get` returns `SandboxInfo` (no key); confirms § II.0 lift work.
- `docs/runbooks/sandbox-nomad-ch.md` — host network model used for the recommended "controller has L3 reach to tap subnets" assumption.
**Unblocks:**
- The builder UI's "Open preview" button.
- AI-builder share links (the AI generates an app, posts a preview URL into chat, end user clicks).
- Vite HMR in-builder (the creator edits a file, the running app reloads).
- Closing the gap between "AI says it works" and "I can see it work" without round-tripping `.zsapp` deploys.

## Executive summary

<!-- Added in round 5 (C5-1): a cold reader needs the 200-word version up top before diving into the 1900-line spec. -->

**Problem.** A creator using the AI builder needs to see their freshly-generated app running with HMR — before publishing. Today the platform has no per-(sandbox, port) URL the browser can hit; the sandbox-agent's surface is internal (`/exec`, `/files`, ...).

**Recommendation.** Ship Option A: an Ed25519-signed agent-mediated proxy. The controller terminates TLS at `*.preview.zeroship.dev`, signs an outbound request to a new agent endpoint `ANY /proxy/{port}/{path:.*}`, which dials `127.0.0.1:{port}` inside the microVM. Backend-agnostic (Docker, K8s, nomad-ch all work the same way); reuses the existing wire protocol; honours the "gateway is dumb" invariant.

**Key tradeoffs.**
- **Latency:** ~10 ms p50 small-request added vs. a hypothetical browser→user-app direct path; ~2.1 s for a 100 MiB upload (hash-then-sign forces buffering). HMR (small WS frames) is unaffected after Upgrade.
- **Capacity:** controller is the per-byte funnel; documented budgets in § XI.6 / § XII. Option B migration is preserved for high-throughput operators.
- **Security:** share tokens are HMAC-SHA-256 with audience binding (`aud: "preview"`), 1 KiB cap, secret_version for rotation; the controller's edge enforces Sec-Fetch-Site, CSP `frame-ancestors`, rate limits at three layers. WS Upgrade has its own canonical (`auth.ed25519-v1.1-ws`) with a domain-separator tag.

**What changes for whom.**
- **Creators:** an "Open preview" button in the AI builder; optionally a "Share" button that mints a 1 h read-only URL. Vite config auto-injected via `@zeroship/vite-preview` (one npm install).
- **SREs:** new metrics, runbook (§ XI), golden-signals dashboard, drain orchestration, sealed-record persistence with AEAD key.
- **sandbox-agent maintainers:** new endpoint family + two new capabilities (`proxy.http-v1`, `proxy.ws-v1`); existing `verify_signed` reused; one new canonical (`auth.ed25519-v1.1` for HTTP, `auth.ed25519-v1.1-ws` for Upgrade).
- **gateway:** unchanged in v1; phase 6 adds an opt-in routing kind for Option B.

**Where to start reading next.** Architecture options (§ A–E) → recommended Option A details (§ II.0–II.8) → AI-builder integration (§ II.9) → operability (§ XI) → latency budget (§ XII).

## Security invariants

<!-- Added in round 6: top-of-doc summary of the two non-negotiable security invariants and the sections that enforce each. The reader should be able to verify each sub-bullet by following the section reference. Any reviewer who cannot trace a sub-bullet to its mechanism is reading a regression. -->

The preview surface MUST hold these two invariants. Every other clause in this document is a means to one of these ends; if you make a change that touches one, re-prove all sub-bullets.

### Invariant 1 — Only authenticated principals reach any preview origin.

Sub-bullets and the section that mechanises each:

- **Anonymous requests get a 401 that does NOT vary by sandbox existence.** The public preview origin returns the same 401 body and the same `login_url` regardless of whether the sandbox exists, is owned by someone else, or is wholly fabricated. (§ III "Public-edge auth contract", § II.2 "Internal route authorize semantics".)
- **Cookie auth is `__Host-`-prefixed, HttpOnly, Secure, with explicit SameSite per-cookie.** Login cookie is `SameSite=Lax` (top-level POST handshake from console origin). Share cookie is `SameSite=Strict` (no cross-site context ever needs to read it; pasted-link UX is preserved by the `?t=` first-hit which carries no cookie state across origins). (§ II.3, § II.6.)
- **Login JWT is single-use, short-lived (30 s), audience-bound, channel-bound.** Server-side `jti` LRU prevents replay; PKCE-style code-verifier binding defeats console-side XSS exfiltration. (§ II.3 "POST handshake".)
- **Share tokens carry `aud: "preview"` AND `iss: <creator-id>`.** Validators reject anything else. Audience separation prevents a future `"aud": "shared-logs"` from being misvalidated; issuer binding gates validation on creator-still-in-good-standing. (§ II.4.)
- **Explicit DELETE share is zero-grace AND propagates to cookies set before the DELETE.** The cookie validator runs the same secret-version logic and respects the same revocation rules as the raw-token validator. (§ II.4 "Revocation", Phase-3 regression test.)
- **Path/route dispatcher is byte-equal with the signature canonical.** No normalization, no decode. A request whose canonical signed path is `/proxy/5173/...` MUST land on the proxy handler, never on `/exec` via path confusion. (§ II.1 "Path normalization & smuggling defenses", regression test in Phase 1.)
- **Sec-Fetch-Site fail-closed for cookie-conversion handlers.** Older browsers without Sec-Fetch-* headers are rejected with `code: "client_too_old"` rather than silently bypassing CSRF defenses. (§ II.6.)
- **AEAD key is file-mounted, never env-var.** `/proc/self/environ` exposes env to anything that can read the controller's procfs; FS permissions on the key file are a stronger boundary. (§ IX.a.)
- **Ed25519-signed `/version` probe on controller restart binds the verifying-key fingerprint.** The controller's persisted record holds `expected_pubkey_fp`; restart re-signs a probe to `/version` and rebinds only on signed-response match. (§ II.5 "Crash semantics".)
- **The slug→typed-id round-trip is case-fixed.** Subdomain slugs are lowercase-only base62 (digits + `a-z`); the typed-id parser rejects mixed-case input from the wire. No DNS case-insensitivity ambiguity. (§ II.3 "Hostname format".)
- **Response-path headers are normalized.** `Set-Cookie: Domain=…` is stripped (defensive — prevents a confused upstream from setting a cookie that can match a different host on the preview eTLD+1 and then leak across previews). Absolute `Location:` URLs pointing at the upstream loopback (`localhost`, `127.0.0.1`, `0.0.0.0`, `[::1]`) are rewritten to the preview origin (prevents the browser from being navigated off-preview by a misconfigured app, where "off-preview" includes the user's own machine, into a non-existent host). Not a hard security boundary (nothing here is the SOLE gate on a security property; it's normalization), but worth noting alongside the cookie / origin invariants. (§ II.1.x.)

### Invariant 2 — Sandbox A cannot reach Sandbox B by any mechanism.

Sub-bullets and the section that mechanises each:

- **`authorize(principal, sandbox_id, port)` is the SOLE gate; `info.user_id == principal.creator_id` is the required check.** Every code path that dispatches to a sandbox goes through it; the four post-auth failure reasons (`auth-failed`, `sandbox-not-found`, `not-owner`, `port-denied`) all surface to the wire as a uniform 404 — **never 400, never 403** — so an attacker cannot probe sandbox existence, ownership, or port-allow-set state through differential responses. The audit log records the actual reason. The agent layer's own port-deny stays at 400 (defense-in-depth; only reachable internally by a misconfigured controller, no public oracle concern). (§ II.2.)
- **Sealed-record file naming is `hex(sha256(sandbox_id))[..32].sealed`.** Even if `sandbox_id` validation has a bug, an attacker-supplied sandbox_id cannot reach a sibling file via path traversal. (§ II.0 §4.)
- **L3 isolation enforced by `iptables -A FORWARD -i zsbx-nm-+ -o zsbx-nm-+ -j DROP` on the host.** VM-A cannot reach VM-B's `10.99.<other>.2:7777` by any path through the host. IMDS (`169.254.169.254`) is also DROPped on FORWARD. The controller's preview API does NOT bind on `10.99.<idx>.1`; it binds on a separate management interface. (§ II.0 "Host network policy", operator runbook.)
- **The sandbox VM sees only the env vars dev tooling needs.** No creator-id, no list of the creator's other sandbox IDs, no console API base beyond the path-prefix Vite needs, no token. The user-app is treated as hostile. (§ II.9 § "Env-var threat model".)
- **postMessage bridge validates `event.origin` exactly.** The console maintains a `Map<iframe, expectedOrigin>`; messages whose origin doesn't match the iframe's expected origin are dropped without action. Every accepted message is audit-logged. (§ II.9.x.)
- **virtio-fs `keys` share is read-only AND symlink-contained.** virtiofsd `--xattr=no --no-readdirplus --sandbox=chroot --readonly`. VM cannot create a symlink in the workspace pointing to host paths outside the share. (§ II.0 §2, regression test in Phase 1.)
- **Per-sandbox signing keys never reach another sandbox's process.** Keys are mounted into the VM via virtio-fs read-only; the agent reads them at boot; the sandbox's user processes have no fs path to them (separate mount, root-owned, mode 0400 from the agent's perspective). (§ II.0 §2.)
- **Share tokens are scoped to one `(sbx, port)` AND audience-bound.** A token for sandbox A path-bound to `/proxy/5173/...` cannot validate against sandbox B regardless of port. (§ II.4 step 5+6.)
- **`is_proxyable_port` is enforced at BOTH the controller and the agent.** Defense-in-depth: a controller bug that lets `9229` through still bounces at the agent. (§ II.1.)
- **`sandbox_id` is generated fresh per VM (UUIDv7 with new entropy); reuse is impossible.** Sealed-record file naming is therefore safe for the lifetime of the platform. (§ II.5.)

The rest of the document is the implementation of these invariants. If a reviewer can construct a counter-example to any sub-bullet, the design is broken; file an issue against the round-6 audit.

## Top matter

### Problem statement

zeroship is "Shopify for AI-generated apps." The builder workflow is:

1. Creator opens the builder in their browser (`builder.zeroship.dev`).
2. The AI generates a Vite/Node app inside a fresh sandbox VM.
3. Inside the VM the user code typically binds `0.0.0.0:5173` (Vite dev server) or `0.0.0.0:3000` (a Node service).
4. **The creator wants to see the running app in their browser, with HMR, before publishing.**
5. Eventually they "Publish" → the built artifact is packaged as a `.zsapp` and deployed to the worker tier; reachable via the production gateway at `<app>.zeroship.ai`.

Step 4 is the gap. The production gateway is for finished, deployed apps — not for ephemeral dev servers running inside a microVM. Today the controller can talk to the in-VM agent on `http://10.99.<100+idx>.2:7777` (see `docs/runbooks/sandbox-nomad-ch.md` § Network), but **nothing lets the creator's browser reach the user's dev server**. The agent's surface is `/exec`, `/files`, `/tree`, `/livez`, `/readyz`, `/version`, `/metrics`, `/shutdown` — there is no proxy.

We need a per-(sandbox, port) URL the creator (and optionally end users via a share link) can open in a browser, with WebSocket support (Vite HMR), arbitrary HTTP, and isolation from every other sandbox in the fleet.

### Goals

1. **Per-sandbox preview URL.** A creator with a live sandbox `s` running a server on port `p` can reach it at a stable URL — e.g. `https://preview-{sandbox-id}-{port}.zeroship.dev/...`.
2. **Arbitrary HTTP/WebSocket.** Vite HMR works (WebSocket Upgrade + binary frames), file uploads work (POST with body), SSE works (long-lived chunked response).
3. **Two access modes.** Authenticated paths for the creator (their own session cookie / bearer token); optionally **shareable** signed-token paths for demoing to a collaborator without a zeroship account.
4. **Backend-agnostic.** Same code path for `docker`, `k8s`, `nomad-ch`. No special-casing.
5. **Compatible with the existing wire protocol.** The agent's auth surface for the proxy MUST be the same Ed25519 scheme as `/exec`. The agent never gets a second auth surface to maintain.
6. **No new third-party agent dependencies.** No ngrok, no Cloudflare Tunnel, no off-host service the agent has to dial out to.
7. **TLS terminated at the platform edge** — never inside the agent.

### Non-goals

- **L4 / non-HTTP protocols.** No SSH, no raw TCP, no QUIC pass-through. HTTP/1.1 + WebSocket only in v1. (HTTP/2 and HTTP/3 backplanes are out — see § VII.)
- **Per-port resource isolation inside the VM.** The whole microVM has a memory + CPU cap; we do not add per-port quotas.
- **Persistent share URLs across sandbox restarts.** Each sandbox boot mints a fresh Ed25519 keypair (`crates/sandbox/src/backend/nomad_ch.rs` `CreateGuard`); share tokens die with the sandbox.
- **Cross-region routing.** Single-region for v1; multi-region is § VII (alternatives considered).
- **Auto-discovery of bound ports.** The creator (or the AI) names the port; we do not scan the VM's listening sockets.
- **Replacing the gateway for production traffic.** Preview URLs are explicitly the *non-production* surface. Published apps go through the gateway as today.
- **Cross-region preview routing.** A sandbox is sticky to the region that created it; the preview URL points there. (See Q-10.)
- **HTTP/2 push or SSE-native streaming protocols.** v1 ships HTTP/1.1 backplane; HTTP/2 ALPN at the public edge is a v1 nice-to-have (Q-11) but not load-bearing.

## Revision history

- **v1 (2026-05-01)** — initial proposal. Recommends Option A (agent-mediated proxy) with a clean migration to Option B (gateway-direct) for operators whose network supports it.
- **v2 (2026-05-01)** — round-1 revisions (completeness + correctness lens):
  - § II.0 added (Lift `signing_key`/`agent_url` into the registry as a Backend trait method) — removes the inaccurate "registry already gives us this" claim.
  - § II.1 added query-string handling subsection (Vite cache-busters + HMR `?token=`); changes `verify_signed` to optionally hash the canonical query into the signature behind a new `auth.ed25519-v1.1` capability.
  - § II.4 token format moved to JSON-then-base64url with explicit `secret_version` and `token_index` fields.
  - § II.5 crash semantics rewritten: persisted-secret store added (sealed-on-disk per controller), so creator-mode preview keeps working across controller restart.
  - Phase plan re-estimated; Phase 0 added (registry lift), Phase 3 estimate revised upward.
  - § II.1 port allow-set: 9229 (Node inspector) explicitly added to the deny-list.
  - § VI risks: R-12 (CORS preflight), R-13 (Sec-Fetch-Site / cookie-conversion CSRF), R-14 (rate limits), R-15 (idle GC vs. long-lived HMR WS), R-16 (concurrent uploads OOM the agent VM) added.
  - § VIII open Qs: Q-10..Q-13 added (regional binding, X-Robots-Tag, h2 ALPN, idempotency).
  - Various correctness fixes: DNS-illegal underscore in sandbox-id; `__zsbx_share` swap-to-cookie threat model; `is_proxyable_port` rule unified.
- **v3 (2026-05-02)** — round-2 revisions (security + threat modeling lens):
  - § II.1 — WebSocket Upgrade canonical is now a **separately-versioned canonical** (`auth.ed25519-v1.1-ws`) with explicit domain-separator tag, NOT a body-hash mutation. Closes a forgery path where a captured non-WS signature on a body-of-form `b"sec-websocket-key=…"` could be replayed as an Upgrade.
  - § II.1 — added explicit "agent recomputes `sha256_hex(body_received)`" sentence; the body-hash claim is *derived*, never trusted from headers.
  - § II.1 — added "Path normalization & smuggling defenses" subsection: paths containing `\r`/`\n`/`\0` are 400; CL+TE both present is 400; chunked extensions stripped; raw passthrough of the `{path}` portion (no decode/re-encode) so the canonical and the wire bytes are identical.
  - § II.1 — added "Retry semantics" subsection: each forward attempt mints fresh `(ts, nonce, signature)`; retries MUST NOT reuse them. Eliminates body-substitution oracle on connection-reset retry.
  - § II.4 — token now includes `"aud": "preview"`; validator rejects unknown `aud`. Forces the share-token surface to fail-closed against any future endpoint that consumes the same HMAC scheme.
  - § II.4 — `iat` is now validated (`iat <= now + 5s` and `iat < exp`), no longer audit-only.
  - § II.4 — token raw-length cap (1 KiB before decode), decoded-JSON size cap (4 KiB), `serde_json` recursion limit (8), `deny_unknown_fields`. Closes the resource-exhaustion vector at the public edge.
  - § II.4 — rotation rate-limit: at most one rotation per sandbox per 5 s; explicit `DELETE token_id=*` is a *zero-grace* invalidation (no 60 s window for rotated-out tokens).
  - § II.3 — `__zsbx_login` flow rewritten: short-lived JWT is delivered via **POST form-submit** to `__zsbx_login`, not `?token=` in URL. The redirect target `next=` is path-only and strictly validated (`^/[a-zA-Z0-9._/~%-]{0,200}$`, no `//`, no `\`). Closes the URL-history / Referer leak.
  - § II.3 — share-token cookie now uses `__Host-` prefix (`__Host-zsbx_share_<sbx>`); host-only with no `Domain` attribute; `SameSite=Strict` upgraded from `Lax` (justified — the residual broken case, "click share link from Slack into a fresh tab", is fine because the link itself carries `?t=` which still works).
  - § II.7 — CSP unified: `frame-ancestors 'self' https://console.zeroship.ai`, contradictory R-4 / R-13 wording corrected.
  - § II.3 — UUIDv7 entropy claim corrected (~2^80 random bits, not 2^131); narrative updated to be honest about time-prefix; argument-from-secrecy strengthened (URL leak is not a security boundary; auth is).
  - § II.3 — Host-header parsing rule made explicit (lowercase strip, regex match, port-suffix tolerated, trailing-dot rejected, IDN/punycode normalized via `idna` crate).
  - § II.4 — added per-creator aggregate token-mint cap (1000/day across all the creator's sandboxes).
  - § II.4 — DELETE share response no longer claims a previous-version grace (explicit revoke is zero-grace).
  - § II.7 — added `Vary: Cookie, Origin` to all responses that may differ on creator vs anonymous.
  - § IX — added "Operational secrets" subsection: TLS private key, AEAD-at-rest key, controller→agent fallback rules; sourcing, rotation, never-log invariants.
  - § II.1 — port deny-list extended: 11211 memcached, 9200/9300 Elasticsearch, 5672/61616 MQ, 9092 Kafka, 2379/2380 etcd, 22 SSH (explicit even though <1024).
  - § II.5 — `pubkey_fp` defined: `hex(sha256(pubkey_bytes)[..16])` (128-bit fingerprint).
  - § VI — R-20 (audit log injection), R-21 (TLS private key compromise), R-22 (token-in-URL → browser history) added.
- **v4 (2026-05-02)** — round-3 revisions (operability lens):
  - § XI added — "Operator runbook outline + golden signals" with concrete metric names, per-bucket SLOs, on-call paging thresholds for each.
  - § II.2 added "Circuit breaker + deep health" subsection — controller opens a per-(sandbox, port) breaker after N consecutive 504/timeout, surfaces 503 with `code: "circuit_open"` until half-open probe succeeds.
  - § II.5 added "Drain orchestration" subsection — five-phase drain (LB un-ready → 5 s grace → 1001 with `reason: "controller-drain"` → close listeners → exit) for both controller and agent.
  - § IX.a expanded — boot-time AEAD key invariant (controller refuses to start without it when persistence is enabled); cert-renewal failure mode + alert thresholds; sealed-record corruption recovery (`sealed-record verify --all`, quarantine).
  - § IX expanded metrics list — added latency histograms (`proxy_request_duration_seconds`, `proxy_upstream_connect_duration_seconds`, `proxy_signature_verify_duration_seconds`), AEAD failures, circuit-breaker state gauge, cert expiry, token-mint/validate counters.
  - § II.5 + § IX.a — multi-controller HA story made explicit: sandboxes are sticky to one controller in v1; cross-controller share-token revocation propagation deferred to a coordinated KV (Q-14 added).
  - § IV Phase rollout — added feature-flag gate `SANDBOX_PREVIEW_FEATURE=v4` and kill switch `SANDBOX_PREVIEW_DISABLED=1`; controller refuses to enable preview on missing AEAD key when `SANDBOX_PERSIST_AUTH=1`.
  - § III + § XI — error-code contract (`code: "rate_limited" | "circuit_open" | "agent_unreachable" | "expired" | ...`) for AI-builder UI consumption.
  - § IX cross-cutting — audit event delivery guarantees: agent ring-buffers up to 10k events, drops oldest on overflow, increments `audit_dropped_total`; controller pulls via long-poll; alert on dropped > 0.
  - § IX.a — sealed-record durability is intentionally ephemeral; documented; no backup procedure required (sandbox-lifetime cap bounds the recoverable state to ≤ 8 h).
  - TLS protocol + cipher policy pinned (§ IX.a): TLS 1.3 only, HSTS preload, no 1.2 fallback.
  - § VIII — Q-14 added (cross-controller share-token revocation), Q-15 added (cert-renewal failure budget).
- **v5 (2026-05-02)** — round-4 revisions (performance + capacity lens):
  - § XII added — "Latency budget" with a derived per-hop table (Host parse, sign, connect, verify, upstream TTFB) and a worked example for HMR (small-frame WS) and a 100 MiB upload.
  - § XI.6 capacity: fd budget math reworked (3 fds per WS, kernel tcp_rmem/wmem implications); WS hard cap reduced from 100k to 30k absent kernel-tuning, with documented tuning recipe to scale higher; per-controller controller→agent connection-pool max raised from 8 to 64 to cover Vite's typical 50-module first-load.
  - § II.1 + § XII — body-hash cost on the request hot path explicitly stated; hashing offloaded to compio's blocking-pool for bodies ≥ 1 MiB; for bodies < 1 MiB, inline (cost < 4 ms).
  - § II.1 — body-buffering memory math now cites both controller AND agent peak (200 MiB peak per request through the platform); fleet total stated.
  - § II.1 — added per-WebSocket bandwidth cap (default 100 Mbit/s; configurable) to close the bandwidth-tunnel abuse vector (C4-12).
  - § II.2 — explicit precedence: rate-limiter → circuit-breaker → sign-and-forward.
  - § II.2 — connection-pool sizing rationale documented (Vite first-load assumption, 64 conn/sandbox).
  - § XI.6 — clear statement that per-WS kernel TCP buffer config is part of the controller VM's tuning prerequisite (`sysctl` recipe in operator runbook).
  - § XII — explicit retry budget: 3 attempts; exponential 100/300/900 ms; total ≤ 1.5 s.
  - § II.1 — note that `Cache-Control` from the upstream user app is preserved; the proxy injects no caching headers on user-app responses.
  - § IX metrics — `port` label replaced with `port_class ∈ {3000, 5173, 8080, other}` to bound histogram cardinality (C4-18); `proxy_ws_bytes_per_second` gauge added.
  - § VIII — Q-16 added (when to enable streaming-mode `auth.ed25519-v2-streaming`).
- **v7 (2026-05-02)** — round-6 revisions (security re-audit lens — invariant 1 "no unauthorized access" + invariant 2 "no sandbox↔sandbox access"):
  - Top-of-doc **Security invariants** section added; lists every sub-bullet with the section that enforces it. The invariant-summary is the doc's contract surface for security reviewers.
  - § II.3 / § II.6 — cookie SameSite contradiction resolved: share cookie `SameSite=Strict`, login cookie `SameSite=Lax`. Both sections now specify byte-exact values.
  - § II.3 — `__zsbx_login` JWT spec hardened: `exp = iat + 30s`, `aud: "login"`, server-side `jti` LRU (30 s TTL), PKCE-style code-verifier binding to defeat console-side XSS exfiltration.
  - § II.4 — explicit-DELETE revocation now states cookie validators run identical secret-version logic + respect the same revocation. Phase-3 regression test added: explicit DELETE invalidates cookies set before the DELETE.
  - § II.1 — canonical-version dispatcher byte-equality with route matcher made explicit; added `/proxy/5173/%2e%2e%2fexec` regression test (must hit proxy via v1.1, never `/exec`).
  - § II.3 — Host header lowercase vs. typed-id case-sensitivity resolved: subdomain slug is lowercase base62 (digits + `a-z`), typed-id parser rejects mixed-case wire input.
  - § II.1 — header-trust paragraph reworked: signed canonical covers (method, path, ts, nonce, body-hash) only; controller MUST scrub `X-Forwarded-For` before audit-logging; controller→agent leg confidentiality assumption stated.
  - § II.3 — `next` regex now percent-decodes BEFORE matching; rejects `%2f`, `%5c`, `//`; redirect target must be on the same origin as the share endpoint.
  - § II.5 — `/version` probe on restart is now a signed envelope: persisted `expected_pubkey_fp`, controller signs probe, agent's signed response decodes under persisted key AND `pubkey_fingerprint` matches → rebind succeeds.
  - § III + § II.2 — 401-vs-404 oracle closed: public-edge anonymous → 401 regardless of sandbox existence (`login_url` invariant); internal route 404 for "auth-failed OR sandbox-not-found OR not-owner"; audit log records actual reason.
  - § II.4 — HMAC-input invariant pinned: validators MUST NOT canonicalize JSON; HMAC input is the exact base64 string the controller emits.
  - § II.6 — Sec-Fetch-Site fail-closed policy specified; missing Sec-Fetch-* → `code: "client_too_old"`; Origin-header check fallback documented.
  - § II.4 — token now carries `iss: <creator-id>`; v7 audit-only logging; v2 of token format gates abuse via `iss`.
  - § IX.a — `$SANDBOX_AEAD_KEY` env var deprecated; only `SANDBOX_AEAD_KEY_PATH` (file-mount) supported. Rationale: `/proc/self/environ` exposes env across container boundaries on shared hosts.
  - § A "Operational threat model" — controller→agent leg precondition: must be on a network where passive observers are excluded; otherwise Wireguard or mTLS. Operator-blocking precondition.
  - § II.9.x **NEW SECTION** — postMessage bridge protocol: console validates `event.origin` exactly per-iframe; envelope schema (`{sandbox_id, port, kind, payload, nonce}`); console maintains `Map<iframe, expectedOrigin>`; every accepted message audit-logged.
  - § II.2 — `authorize(principal, sandbox_id, port)` semantics spelled out; `info.user_id == principal.creator_id` required; lookup failure → 404 (not 403). Phase-3 regression test added (creator-A cookie + creator-B preview URL → 404).
  - § II.0 §4 — sealed-record filename now `hex(sha256(sandbox_id))[..32].sealed`; defense-in-depth against `sandbox_id` validation bugs. Regression test added.
  - § II.0 **NEW SUBSECTION** "Host network policy" — `iptables -A FORWARD -i zsbx-nm-+ -o zsbx-nm-+ -j DROP`, `-d 169.254.169.254 -j DROP`, controller preview API binds on management interface (NOT `10.99.<idx>.1`). Phase-1 regression test added (VM-A → VM-B at `10.99.<other>.2:7777` MUST fail).
  - § II.9 **NEW SUBSECTION** "Env-var threat model" — minimal env-var set spelled out; user-app is hostile.
  - § II.0 §2 — Docker keys-share + virtio-fs keys share documented as read-only; virtiofsd flags `--xattr=no --no-readdirplus --sandbox=chroot --readonly` enumerated. Phase-1 symlink-containment regression test added.
  - § II.1 — empty-vs-absent query handling specified byte-exact. Corner-case test added.
  - § II.0 §4 — `agent_url` derived from `vm_index` at restart, not sealed. Reduces sealed-record contents.
  - § II.4 / § VII — per-token revocation upgraded from "follow-up" to **Phase 5** with explicit ETA.
  - § II.6 / § II.9 — iframe `sandbox` attribute drops `allow-popups`. (Builder UX flagged for follow-up if a creator app actually needs it.)
  - § II.4 — sealed record now stores `(sv_current, sv_previous)`; multi-controller failover preserves grace window.
  - § II.5 — `sandbox_id` reuse impossibility stated explicitly (UUIDv7 with new entropy per VM).
  - § II.1 — `is_proxyable_port` defense-in-depth re-stated: enforced at controller AND agent.
  - § II.4 — path-prefix scope (`scope: "ro+/path/prefix"`) reserved for Phase 5; v1 residual-risk documented.
  - § II.3 — DNSSEC zone-signing recommended for `*.preview.zeroship.dev`.
  - § II.0 — agent listener: 0.0.0.0:7777 inside the VM is effectively tap-only because the VM has only one interface (tap). Stated explicitly.
  - § IX.a — host-swap encryption requirement (or no-swap) so signing keys / pre-AEAD record don't leak.
  - § II.9.x — postMessage bridge audit log: every cross-iframe message logged with `(sandbox_id, kind, origin-validated, decision)`.
  - LOWs: `Vary: Cookie, Origin` on 401 anonymous-fallback (§ III); CAA records on `*.preview.zeroship.dev` (§ IX.a); JWT-confusion separator changed `.` → `~` (§ II.4); validate-side TTL ceiling `exp <= now + 1week` (§ II.4).
  - Phase 1 / Phase 3 / Phase 4 regression-test plans expanded (see § IV).
  - § II.1.x **NEW SUBSECTION** "Header rewriting on the response path" — concrete spec for the four header transforms (Set-Cookie Domain strip, Location host rewrite, Refresh url rewrite, plus Set-Cookie Path pass-through), the request-path header table (Host / Origin / X-Forwarded-* contracts), the explicit non-rewriting decision for body content, the Vite-plugin `server.origin` interaction, and edge cases (`<base href>`, service workers, Set-Cookie ordering, third-party Location targets). New decision **D-17** in the decision table; new open question **Q-19** (CORS allowlist for `console.zeroship.ai` against preview origins, deferred to Phase 4). Phase 1 test plan extended with 5 header-rewrite tests. Security-invariants block (Invariant 1) gets a new bullet acknowledging response-path normalization as a defensive posture (not a hard security boundary).
- **v6 (2026-05-02)** — round-5 revisions (clarity + dev UX lens):
  - **Executive summary** added above "Top matter" — 200-word "what is this" for cold readers.
  - § II.9 added — "AI builder integration" consolidates scattered references (env-var injection, error-code contract, Vite plugin, system-prompt edits, console preview vs. public-edge) into one section.
  - Appendix A Vite config — fixed for the **console preview** case (the most common path during AI building, not the public-edge case): two configs documented, plus a `@zeroship/vite-preview` plugin that auto-detects via env vars and applies the right one.
  - § VII migration story (Option A → Option B) made concrete: same user-visible URL; per-operator flag `SANDBOX_PREVIEW_VIA_GATEWAY=true`; per-sandbox override; rollback procedure.
  - Section numbering bug fixed: II.6 (failure-mode summary) moved before II.7 (cross-origin policy) and II.8 (idle GC); now II.6 → II.7 → II.8 in order.
  - Decision table — D-14 (WS canonical separation), D-15 (path passthrough), D-16 (per-WS bandwidth cap) added; column convention noted.
  - Risk severity scale documented at top of § VI: `Low | Medium | High | Critical`; consistent with v3+ entries.
  - § III API spec — public-edge auth contract spelled out (cookie OR `?t=`; anon → 401 with `code: "unauthorized"`).
  - Sequence diagrams updated to use the round-2 canonical cookie names (`__Host-zsbx_preview_<sbx>`, `__Host-zsbx_share_<sbx>`).
  - Phase rename: Phase 4.1 → Phase 5 (operability gates); Phase 5 → Phase 6 (Option B migration); chronological.
  - Glossary added at end of doc for ALPN, CT-monitoring, CHWBL, h2/h2c, HSTS, HPACK, OCSP.
  - Round-N HTML comment markers retained (review trail) — final-merge cleanup task tracked.

## Decisions

Settled choices, in priority order. Each is followed by a one-line rationale and a deeper section reference. New readers: skim the **Architecture options (§ A–E)** first if any of these are unfamiliar — the table below assumes that context.

| # | Decision | Rationale | Section |
|---|---|---|---|
| **D-1** | Preview is implemented as an Ed25519-signed proxy through the controller to a new agent endpoint `ANY /proxy/{port}/{path*}`. | Single design path that works on docker/k8s/nomad-ch; reuses the existing wire protocol; no agent egress required; controller already has L3 to taps. | § II |
| **D-2** | Public DNS uses a wildcard `*.preview.zeroship.dev` subdomain, NOT the bare `*.zeroship.dev`. | Subdomain isolation from `console.zeroship.ai`, `auth.zeroship.ai`, and the per-app gateway domains; no cookie/CORS bleed. | § II.3 |
| **D-3** | Port and sandbox-id encoded in the **subdomain**: `preview-{sandbox-id}-{port}.preview.zeroship.dev`. | Enables `cookies + CSP + same-origin` to bind correctly per-(sandbox,port); avoids Vite's `base` complications; lets us blackhole specific sandboxes by DNS without path-rewriting. | § II.3 / § VIII open-Q |
| **D-4** | Auth modes: (a) creator session cookie issued by `console.zeroship.ai` (cross-domain via short-lived bearer minted on demand), (b) HMAC-signed share tokens carried as query parameter `?t=<base64>` (then immediately swapped for a HttpOnly cookie). | (a) is the default; (b) opt-in per-port; both validate at the controller's public edge, never at the agent. | § II.4 |
| **D-5** | Share-token validator is **preview-only**. The same token MUST NOT grant `/exec` or `/files` access. Path-allowlist enforcement at the public route. | A share token leaking to GitHub Gist must not let strangers run shell commands inside the VM. | § VI risk "auth bleed" |
| **D-6** | Body size for signed proxy requests is capped at **100 MiB by default** (configurable). Larger uploads return 413 from the controller. | The Ed25519 canonical hashes the full body; streaming-upload would require a new wire format. v1 buffers and caps. | § II.1 / § VI |
| **D-7** | Streaming requests **between controller and agent** use HTTP/1.1 chunked transfer; signature covers the whole-body hash, computed once on the controller before forwarding. | Body hash is needed before the first byte goes on the wire — this is the cost of body-binding the signature; document it, cap it. | § II.1 |
| **D-8** | WebSocket Upgrade is a single signed request (the HTTP-Upgrade exchange); the bidirectional frame stream that follows is **outside** the signature surface. The Upgrade is signed; the post-Upgrade bytes are the upgraded TCP socket. | Mirrors how every reverse-proxy (nginx, Caddy, Envoy) handles this; signing every frame would require a new framed-auth protocol. | § II.1 |
| **D-9** | Public-edge HTTP server lives in the existing `crates/sandbox` controller binary (which already has ntex), not in `crates/gateway`. | Keeps the gateway free of sandbox-network-model coupling (Invariant: "the gateway is dumb"); leaves room for D-13 below. | § II.2 |
| **D-10** | Add capability bit `proxy.http-v1` and (later) `proxy.ws-v1` to `version.rs::CAPABILITIES`. Controller refuses to mint a preview URL for a sandbox whose agent doesn't advertise the capability. | Old agents (pre-this-feature) get a clean error instead of a 5xx; mirrors how `auth.ed25519-v1` is detected today. | § V migration |
| **D-11** | Per-sandbox secret is rotated on Drop (sandbox stop); revoking a single token = bumping a per-token-index byte in the secret OR maintaining a tiny in-process revocation set. v1 ships with **whole-sandbox revoke only** (rotate the secret); per-token revoke is a follow-up. | Stateless HMAC is cheap and correct; per-token revoke adds a state table; ship the simpler thing first. | § II.4 |
| **D-12** | `127.0.0.1` from the agent's perspective IS the same netns as the user's app — both run as PID-1 + fork inside the same microVM (verified: `crates/sandbox/scripts/nomad-vm-wrapper.sh` execs the agent as PID 1 inside CH; user processes are forked off `/exec`). The agent dialing `127.0.0.1:{port}` reaches the user code. | Documented assumption; verify in test (§ IV.1 phase 1 e2e) and add a regression check. | § II.1 |
| **D-13** | Option B ("gateway-direct routing into tap subnets") is **not v1**, but the design preserves a clean migration path: the gateway grows a new manifest entry kind that maps `preview-*.preview.zeroship.dev` → `10.99.<100+idx>.2:{port}`, and the controller-side proxy can be turned off per-operator. | Option B is faster (one fewer hop) but couples the gateway to the backend's network model — viable for nomad-ch on a single host, painful for docker (different bridge) and k8s (CNI). Ship A, support both later. | § VII alternatives |
| **D-14** | WebSocket Upgrade uses a **separate canonical** (`auth.ed25519-v1.1-ws`) with a fixed domain-separator tag, NOT a body-hash mutation. | Round-2 fix: prevents a captured non-WS signature whose body matches `b"sec-websocket-key=…"` from being replayed as an Upgrade. Domain separation is the standard fix; cheap. | § II.1 (Sec-WebSocket-Key binding) |
| **D-15** | Proxy `{path}` is **passed through raw** — no URL-decode/re-encode, no `..` collapse, no slash-dedup. Smuggling defenses (CL/TE rejection, control-byte filter, length cap) sit on top. | Round-2: any path mutation creates canonical/wire divergence and signature failures; user-app handles its own normalization. | § II.1 (Path normalization) |
| **D-16** | Post-Upgrade WebSocket frames have a **per-connection bandwidth cap** (default 100 Mbit/s each direction). | Round-4: post-Upgrade has no per-frame policy; without a cap a malicious user-app can tunnel egress through the WS at line rate. | § II.1 (Per-WebSocket bandwidth cap) |
| **D-17** | Response-path header rewriting is a transport-layer concern. The proxy strips `Set-Cookie: Domain=` and rewrites absolute `Location:` host+port (plus `Refresh:` analogously). Body content is **NEVER** rewritten — the Vite plugin's `server.origin` is the canonical-URL story for Vite; non-Vite apps must use relative URLs or read `X-Forwarded-Host`. | We avoid streaming-body rewriting (correctness + performance trap: false positives, broken streaming, CSP / SRI / integrity hazards). The four header rewrites are mechanical and cheap. | § II.1.x |

## Architecture options

### A. Agent-mediated HTTP/WS proxy (recommended for v1)

```
Browser (creator or end user)
      │  HTTPS
      ▼
edge TLS (Let's Encrypt wildcard *.preview.zeroship.dev)
      │  HTTP/1.1 (loopback)
      ▼
sandbox-controller :443
   ├─ AuthN (creator session OR share-token validator)
   ├─ Sandbox lookup (registry: subdomain → sandbox_id)
   └─ Sign + forward to agent_url + /proxy/{port}{path}
      │  HTTP/1.1 (controller has L3 to 10.99.<100+idx>.0/30)
      ▼
sandbox-agent :7777
   ├─ verify_signed (existing Ed25519 v1)
   ├─ /proxy/{port}{path} handler
   └─ dial 127.0.0.1:{port} (same netns as user code)
      │
      ▼
user's Vite dev server / Node service / whatever
```

**Pros**

- **Zero gateway changes.** Invariant "the gateway is dumb" is honoured — the gateway never learns about preview traffic.
- **Zero network changes.** The controller already has L3 reach to every tap subnet (k8s: cluster CNI; nomad-ch: host routes; docker: same docker0 bridge). No Wireguard, no peering, no VPN.
- **Backend-agnostic.** Single code path. The Backend enum doesn't even need a new method — preview is a pure derivation of `(sandbox_id → agent_url, signing_key)` which the registry already gives us.
- **Reuses wire-protocol-v1.** The agent learns one new endpoint family; everything else (skew check, nonce LRU, audit events) is shared with `/exec`.

**Cons**

- **Extra hop.** Browser → controller → agent → app is two extra TCP hops vs. a hypothetical browser → gateway → app. For HMR (a tight loop of small WebSocket frames) this adds maybe 1 ms p50 in-region. For large file uploads it's dominated by the upload itself. Acceptable for v1 dev-only traffic.
- **Controller as proxy capacity.** Every preview byte goes through the controller. We measure (§ IV.4 benchmarks) and budget; if it bites we fast-track Option B for the operators who can run it.
- **Body buffering on signed requests.** D-7 — the canonical-string hashes the full body. 100 MiB cap.

### B. Gateway-direct routing into tap subnets

```
Browser → edge TLS → gateway → 10.99.<100+idx>.2:{port}
```

The gateway grows a per-sandbox manifest entry: `preview-{id}-{port}.preview.zeroship.dev` → `10.99.<100+idx>.2:{port}`, with no auth (or a thin JWT check at the gateway).

**Pros**

- One fewer hop.
- Gateway's existing routing already does WebSocket and HTTP uniformly.
- Bypasses the controller's per-byte cost.

**Cons**

- **Gateway must have L3 reach into the tap subnets.** Works on a single host (gateway and sandbox-host colocated); cross-rack needs Wireguard mesh or BGP.
- **Backend coupling.** docker uses a separate docker0 bridge; k8s uses CNI (the gateway needs to be a Pod inside the cluster); nomad-ch uses host taps. The gateway today knows nothing about any of this. Teaching it three network models breaks the "gateway is dumb" invariant.
- **Auth is harder.** The Ed25519 signing key lives in the controller. Either (i) the gateway also holds per-sandbox keys (key distribution problem), or (ii) the agent grows a *second* auth surface (JWT-from-gateway), violating Goal 5.

**Verdict:** good migration target for nomad-ch operators on a single host. Not v1.

### C. Reverse-tunnel from agent to a public edge

The agent dials out to a controller-run tunnel server and registers `(sandbox-id, port)` mappings. Browser hits the tunnel server.

**Pros**

- Works behind NAT (agent doesn't need an inbound port).
- Multi-region native — the agent picks the closest tunnel.

**Cons**

- **Tunnel server is a new service to operate** (control loops, sticky routing, auth). Effectively an in-house Cloudflare Tunnel.
- **SPOF.** Every sandbox depends on the tunnel server reachability.
- **Reinvents what off-the-shelf vendors solved.** Goal 6 ("no new third-party deps") doesn't preclude *building* one; but the cost is enormous vs. Option A.

**Verdict:** the right answer if we ever go multi-region with split-network operators; not v1.

### D. Per-sandbox CNI + ingress controller

k8s only — kills the unified-interface promise. **Skip.**

### E. Browser → agent direct via WebRTC data channels

Interesting research; far too complex for v1; survives § VII as a footnote.

### Recommendation

**Ship A.** Plan B as a per-operator opt-in once we have one. Skip C/D/E.

---

## II. Detailed design — Option A

### II.0 Prerequisite — lift `(agent_url, signing_key, preview_secret)` to the registry

<!-- Added in round 1: addressing CRITICAL #1, #2, #3 — the registry does NOT today carry agent_url or signing_key. -->

**Today's reality (verified against the codebase as of commit `08465319`):**

- `SandboxRegistry::get` returns `SandboxInfo`, which has `(sandbox_id, user_id, project_id, backend, backend_hint, created_at_secs, last_used_at_secs)` — **no signing key, no agent URL**. See `crates/sandbox/src/registry.rs:80-86` and `crates/sandbox/src/backend/mod.rs:95-115`.
- The per-VM `SigningKey` is held inside `nomad_ch.rs::SandboxRecord` (`crates/sandbox/src/backend/nomad_ch.rs:157-167`). It does NOT escape that struct.
- The Docker backend has no signed-RPC path — it shells out via `docker exec`, no agent HTTP is involved. Preview support on Docker requires teaching the Docker backend to spawn the agent inside the container and to mint a per-container Ed25519 keypair.
- The K8s backend uses a Bearer token mounted into the Pod, not Ed25519 (`crates/sandbox/src/auth.rs`). Preview support on K8s requires aligning K8s on Ed25519 (a one-time switch matched by mounting the verifier pubkey via a ConfigMap, which is in fact what `sig.rs` says it expects).

The proposal's claim that "preview is a pure derivation of the registry" is therefore **wrong as-stated** — there is real abstraction work to do first. We address it explicitly:

#### Plan

1. **Extend `SandboxInfo`** (or add a sibling struct, `SandboxAuth`) with three new fields:
   ```rust
   pub struct SandboxAuth {
       pub agent_url: String,                  // http://10.99.<idx>.2:7777 or k8s service URL
       pub signing_key: Arc<SigningKey>,       // moved from per-backend records
       pub preview_secret: [u8; 32],           // round-1: per-sandbox HMAC secret
       pub secret_version: u32,                // bumped on rotate-all
   }
   ```
   `signing_key` does NOT serialize (Serialize impl skips it). `preview_secret` is treated identically.

2. **Add a `Backend::session_auth(&self, sandbox_id) -> SandboxAuth` method**, with backend-specific implementations:
   - `NomadCh` returns the existing per-VM record's keys. **Keys reach the VM via a virtio-fs share** (see "Key share invariants" below).
   - `Docker` mints a fresh keypair at container-create, mounts the pubkey into the container as a tmpfs file, holds the signing key in-process. (One-time backend upgrade; tracked in § IX cross-cutting.)
   - `K8s` mints a keypair at Pod create, mounts the pubkey into the Pod as a `ConfigMap`. (Aligns the K8s backend with the existing `sig.rs` trust model. The `Bearer`-token path is removed; this is a coordinated wire change, gated on capability `auth.ed25519-v1` already advertised by the agent.)

   **Key share invariants (round-6 Invariant-2 I1 + I7):**

   | Backend | Mount type | Path inside | Mode | Read-only? | Symlink containment |
   |---|---|---|---|---|---|
   | `NomadCh` | virtio-fs (`virtiofsd`) share named `keys` | `/run/keys/` | `0400` (root inside VM) | **YES** — `virtiofsd --readonly --xattr=no --no-readdirplus --sandbox=chroot` | virtiofsd `--sandbox=chroot` confines symlink resolution to the share root; a symlink in the workspace pointing to `/run/keys/...` cannot resolve out of the workspace share root |
   | `Docker` | bind-mount with `:ro` | `/run/keys/` | `0400` (UID 0 inside container) | **YES** — `docker run -v <host-keys>:/run/keys:ro` | Docker's bind-mount + read-only flag prevents writes; symlink-to-host-path is not resolvable because the mount root is the host directory itself |
   | `K8s` | `ConfigMap` projected volume | `/run/keys/` | `defaultMode: 0400` | **YES** — projected volumes are read-only by spec | k8s projected volumes never resolve symlinks across the share |

   **Phase-1 regression test (round-6 I7).** Inside VM-A's user shell: `ln -s /run/keys/controller-pubkey ~/workspace/leak.txt`. Then on the controller host, attempt to resolve `~/<host-side-workspace-share-root>/leak.txt`. Expected: the resolution does NOT escape the workspace share root (virtiofsd `--sandbox=chroot` makes the symlink resolve to nothing usable from outside the VM). Repeat for Docker (bind-mount escape attempt) and K8s.

   **No-symlink fallback option.** If a future virtiofsd version drops `--sandbox=chroot`, the fallback is `--xattr=no --no-readdirplus` and a separate filesystem-namespace shim that strips symlinks at virtiofsd's exit. Tracked as Q-17 if needed.

3. **`SandboxRegistry` carries the `Arc<SandboxAuth>`** alongside the existing `Sandbox` record. `get_auth(&Uuid) -> Option<SandboxAuth>` is the new lookup the preview proxy uses. Cheap; clones an Arc.

4. **Persistence (controller-restart durability — addresses CRITICAL #2):**
   - At sandbox-create time, the controller writes a sealed record to disk. **Filename is a hash of the sandbox-id, not the sandbox-id itself** (round-6 Invariant-2 CRITICAL-3):
     ```
     sealed-records/<hex(sha256(sandbox_id))[..32]>.sealed
       contents (after AEAD-unseal):
         expected_pubkey_fp, expected_verifying_key (32 bytes),
         signing_key (encrypted), preview_secret (encrypted),
         sv_current, sv_previous (round-6 I6: failover-grace preservation)
     ```
     `agent_url` is NOT sealed (round-6 I3): it is recomputed deterministically from `vm_index` at restart. Smaller attack surface; same security property; no migration risk if the agent's listen address ever changes.
   - **Path-traversal hardening (round-6 Invariant-2 CRITICAL-3).** Even if `sandbox_id` validation has a bug (e.g., a future contributor adds a typed-id parser that accepts `..`), the SHA-256 hashing means an attacker-supplied sandbox_id cannot reach a sibling file: hash output is a fixed-width hex string in `[0-9a-f]`. The persist code uses `Path::join` with the hash output ONLY; the raw sandbox_id is never composed into a filesystem path. Phase-0 regression test: pass a sandbox_id of literal `"../../etc/passwd"` to `seal()`; assert the resulting file is `sealed-records/<hex>.sealed`, NOT `sealed-records/../../etc/passwd.sealed`.
   - Encryption: a single controller-wide AEAD key (XChaCha20-Poly1305, key from `SANDBOX_AEAD_KEY_PATH` file mount; ops doc § IX.a — round-6 H8). Sealed records survive a controller crash.
   - **AEAD nonce strategy.** Each sealed record uses a fresh 24-byte XChaCha20 nonce written alongside the ciphertext (`nonce || ciphertext || tag`). The associated-data carries the sandbox-id hash (the filename's stem), binding the ciphertext to the file location.
   - On controller restart, before accepting traffic, the controller re-reads each sealed record and re-populates the registry. `cleanup_orphans_at_startup` retains its existing role (kill VMs whose record file is gone) but is now joined by `restore_auth_records_at_startup` (re-arm signing keys for VMs that *are* still alive). Each record is validated against the agent via the signed `/version` rebind probe (§ II.5).
   - Sealed records are deleted on `Backend::stop`.
   - Trade-off: this is the smallest persistence surface that keeps creator-mode preview working across controller restart. Share-token persistence is in-scope as a v1 simplification — the same encrypted record holds `preview_secret`.

5. **Phase-0 lands the lift, including:**
   - `Backend::session_auth` method (each backend implements it).
   - `SandboxAuth` struct + sealed-record codec.
   - K8s backend Ed25519 alignment (the auth-mode flip).
   - Docker backend agent-launch path.

   This is the prerequisite for everything else. Estimated 1.5–2× the original Phase-1 size (8–12h, see updated phase plan).

#### Updated non-goals (replaces the old line)

- ~~Persistent preview URLs that survive a controller restart.~~ **REVISED:** Preview URLs DO survive a controller restart (sealed-record persistence in II.0 §4). Share tokens survive too, since the per-sandbox secret is in the same record. What does NOT survive is a sandbox restart — when the VM dies, the record is deleted, all tokens for that sandbox die. (Symmetric: the sandbox is gone, the URLs that pointed at it are gone.)

### II.0.-1 Agent listener binding (round-6 Invariant-2 I12)

<!-- Round-6 Invariant-2 I12: explicit. -->

The sandbox-agent listens on `0.0.0.0:7777` inside the VM. The VM has exactly one network interface (the tap pair attached to `10.99.<100+idx>.0/30`); there is no other interface to bind on. The listener is therefore **effectively tap-IP only** even though the bind address is wildcard. Stated explicitly because a future contributor might worry that `0.0.0.0` exposes the agent on additional interfaces — there are no additional interfaces to expose on. The host's iptables FORWARD-DROP rules (§ II.0.0) ensure the only path from outside the VM to `10.99.<idx>.2:7777` is via the controller's own management interface, which has the L3 reach the platform requires.

If a future backend adds a second vNIC (e.g., for outbound egress through a per-app NAT gateway), the agent's listener must be reconsidered: bind explicitly on the tap-side IP, NOT `0.0.0.0`. Tracked as a regression test target.

### II.0.0 Host network policy (round-6 Invariant-2 CRITICAL-4)

<!-- Round-6 Invariant-2 CRITICAL-4: the most important fix in this round. The previous /30-subnet model relied on routing absent FORWARD rules; an attacker on VM-A could reach VM-B by spoofing source IPs or relying on the host kernel forwarding between the two tap subnets (which IS the controller's L3-reach property). Add explicit iptables/nft rules that the operator's runbook MUST install. -->

The /30-subnet model (`10.99.<100+idx>.0/30` per sandbox; agent at `.2`, host at `.1`) gives each microVM a tiny routed subnet. Routing without explicit DROP rules would leave VM-A able to reach VM-B's `10.99.<other>.2:7777` because the host has L3 to BOTH subnets and forwards by default. This is a tenant-isolation defect; the host kernel MUST be configured to DROP every VM↔VM forward path before the controller starts accepting sandbox creates.

**Required iptables rules (operator runbook entry):**

```sh
# Drop every packet whose ingress AND egress are sandbox tap interfaces.
# zsbx-nm-+ matches zsbx-nm-100, zsbx-nm-101, etc. (the per-sandbox tap names; see
# crates/sandbox/src/backend/nomad_ch.rs).
iptables -A FORWARD -i zsbx-nm-+ -o zsbx-nm-+ -j DROP

# Block IMDS access from any sandbox VM. 169.254.169.254 is the AWS / GCP / Azure
# metadata endpoint; many cloud hosts run an IMDS responder, and a creator app
# inside the VM should NEVER reach it (would otherwise leak controller IAM credentials).
iptables -A FORWARD -i zsbx-nm-+ -d 169.254.169.254 -j DROP

# Block the controller's own API IP from sandbox VMs. The controller MUST NOT bind its
# preview API on 10.99.<idx>.1 (the per-VM host gateway address) — it binds on a
# separate management interface (e.g., 0.0.0.0 on the management subnet). If the operator
# has somehow bound it on the per-VM gateway, this rule is the safety net:
iptables -A FORWARD -i zsbx-nm-+ -d <CONTROLLER_MGMT_IP>/32 -p tcp --dport <CONTROLLER_API_PORT> -j DROP
```

Equivalent nft rules (if the operator runs nftables instead):

```nft
table inet zeroship {
    chain forward {
        type filter hook forward priority 0;
        iifname "zsbx-nm-*" oifname "zsbx-nm-*" drop
        iifname "zsbx-nm-*" ip daddr 169.254.169.254 drop
        iifname "zsbx-nm-*" ip daddr <CTRL_IP> tcp dport <CTRL_API_PORT> drop
    }
}
```

**Controller binding rule (round-6 Invariant-2 CRITICAL-4 §C4-tail).** The controller's preview API listener MUST bind on a separate management interface (`0.0.0.0:443` on the management subnet, or an explicit `controller.private.network:443`). It MUST NOT bind on `10.99.<idx>.1` for any sandbox-index `idx` (the per-VM host-gateway IP). Sandbox VMs can only see `10.99.<idx>.1` as their default gateway and should never receive a preview-API response from it. The controller's startup self-check verifies this binding constraint and refuses to serve preview if violated (`FATAL: preview API listener overlaps sandbox host-gateway IP <ip>`).

**Phase-1 regression test (added in round 6):**

1. Boot two sandboxes on the same controller (indexes A=100, B=101).
2. Inside VM-A: `curl --max-time 2 http://10.99.<other-than-A>.2:7777/livez`. Expected: timeout or "no route to host"; MUST NOT return 200.
3. Inside VM-A: `curl --max-time 2 http://169.254.169.254/latest/meta-data/`. Expected: timeout / DROP. MUST NOT return any IMDS data.
4. Inside VM-A: `curl --max-time 2 http://10.99.<a>.1:443/sandboxes/<b-id>/preview/...`. Expected: connection refused (controller doesn't bind on 10.99.<a>.1) OR DROP if the safety-net rule is in place. MUST NOT reach the preview API.
5. From the host (control plane): `curl http://10.99.<idx>.2:7777/livez` succeeds — the host retains L3 reach for the controller→agent leg.

These tests live in `tests/sandbox_isolation.sh` (gated on root + nomad-ch backend) and are part of the Phase-1 e2e suite.

### II.0.1 Goal 5 reaffirmation — agent's auth surface is unchanged

The lift in § II.0 is a **controller-side** restructuring. The agent's wire surface is unchanged:

- `auth.ed25519-v1` continues to mean exactly what it means today (`X-Sbx-Timestamp/Nonce/Signature` over `(method, path, ts, nonce, sha256_hex(body))`).
- New capability `proxy.http-v1` adds the new endpoint family but reuses the same canonical-string contract.
- New capability `auth.ed25519-v1.1` (see § II.1) is OPT-IN and signals that the agent will accept requests whose canonical *also* hashes the URL query string. Default-off; controllers send v1 unless they need queries.



### II.1 Agent endpoint: `ANY /proxy/{port}/{path*}`

#### Routing surface

```
HTTP/1.1 GET    /proxy/5173/index.html         HTTP/1.1
HTTP/1.1 POST   /proxy/3000/api/upload         HTTP/1.1
HTTP/1.1 GET    /proxy/5173/@vite/client       HTTP/1.1   (Upgrade: websocket)
HTTP/1.1 PUT    /proxy/3000/files/foo          HTTP/1.1
HTTP/1.1 DELETE /proxy/3000/items/42           HTTP/1.1
ANY    /proxy/{port}/{path:.*}    →    127.0.0.1:{port}/{path}
```

#### Rust signature (sketch)

```rust
// crates/sandbox-agent/src/handlers.rs
pub async fn proxy(
    req: HttpRequest,
    state: State,
    parts: web::types::Path<(u16, String)>,   // (port, path)
    body: Bytes,
) -> HttpResponse {
    if !verify_signed(&req, &body, &state) { return unauthorized(); }
    if state.is_draining() { return draining(); }

    let (port, path) = parts.into_inner();
    if !is_proxyable_port(port) { return err(400, "port not allowed"); }

    if is_websocket_upgrade(&req) {
        return proxy_ws(req, state, port, path, body).await;
    }
    proxy_http(req, port, path, body).await
}
```

#### Authentication

**Same Ed25519 scheme as `/exec`** (`crates/sandbox-agent/src/sig.rs`), **plus a v1.1 extension that hashes the URL query string** for the proxy endpoints (the existing `/exec`, `/files`, `/tree` paths are unchanged):

```
canonical_v1   = method + "\n" + path + "\n" + ts + "\n" + nonce + "\n" + sha256_hex(body)
canonical_v1.1 = method + "\n" + path + "?" + canonical_query + "\n" + ts + "\n" + nonce + "\n" + sha256_hex(body)
                where canonical_query = sorted-by-name url-encoded "k=v&k2=v2..." (RFC 8785 inspired)
```

<!-- Round-6 Invariant-2 I2: empty-vs-absent query handling pinned byte-exact. -->

**Empty/absent query handling (round-6 I2).** The byte-handling rule for the canonical's query slot:

- If the URL bytes contain a `?`, then `canonical_query` is the bytes between the `?` and the `#` (or end-of-URL if no fragment). This includes the empty string for `https://.../foo?` (the `?` is present but no params).
- If the URL bytes do NOT contain a `?`, then `canonical_query` is the empty string AND the canonical does NOT include the literal `?` separator: `canonical = method + "\n" + path + "\n" + ts + ...` (note: no `?`).
- Controller signer and agent verifier hash the same byte string per these rules. Pin in the canonical-string emitter (`crates/sandbox/src/preview.rs`) and verifier (`crates/sandbox-agent/src/sig.rs`).

**Corner cases (Phase-1 test additions, round-6 I2):**

| URL | canonical fragment |
|---|---|
| `/proxy/5173/foo` | `GET\n/proxy/5173/foo\n...` (no `?`) |
| `/proxy/5173/foo?` | `GET\n/proxy/5173/foo?\n...` (literal `?`, empty query) |
| `/proxy/5173/foo?a=1` | `GET\n/proxy/5173/foo?a=1\n...` |
| `/proxy/5173/foo?a=1&b=2` | `GET\n/proxy/5173/foo?a=1&b=2\n...` (sorted: `a=1&b=2`) |
| `/proxy/5173/foo#frag` | `GET\n/proxy/5173/foo\n...` (fragment ignored) |
| `/proxy/5173/foo?a=1#frag` | `GET\n/proxy/5173/foo?a=1\n...` |

<!-- Added in round 1: addressing CRITICAL #5 — Vite serves every asset request with `?t=<cache-buster>`, and the existing handlers.rs:110 rejects any query string outright. Without this the proxy 401s every Vite asset. -->

**Query-string handling (round-1 fix).** The current agent (handlers.rs:110) returns 401 if any query string is present, on the principle that the canonical doesn't cover queries. That is correct security policy for `/exec` (where queries had no defined meaning) but is incompatible with proxying Vite (which appends `?t=<ts>` to every asset URL) or any proxied app that uses query parameters. We fix this with a **scoped, opt-in** capability:

- New capability `auth.ed25519-v1.1` advertised by an agent that knows how to verify the v1.1 canonical.
- `/proxy/{port}/{path*}` accepts the v1.1 canonical (queries hashed) **only**. The rest of the agent's surface (`/exec`, `/files/...`, `/tree`) continues to reject queries and use v1.
- The agent picks the canonical version by inspecting the request path: paths under `/proxy/` use v1.1; everything else stays on v1.
- Old controllers that don't speak v1.1 see `proxy.http-v1` advertised but the proxy returns `501` if the controller never bumped its signer.
- This contains the wire change to one endpoint family and avoids relaxing the query-rejection rule on the security-sensitive endpoints.

<!-- Round-6 Invariant-1 CRITICAL-4: dispatcher byte-equality with the route matcher. -->
**Dispatcher byte-equality invariant (round-6).** The agent picks the canonical version by inspecting `request.path` — the **exact same byte string** ntex's route matcher uses to dispatch the handler. There is no normalization, no decode, no collapsing between the canonical-version chooser and the route matcher. The implementation rule:

```rust
// crates/sandbox-agent/src/handlers.rs (round-6)
fn dispatch(req: &HttpRequest) -> CanonicalKind {
    let raw_path: &[u8] = req.path().as_bytes();   // identical reference for both checks
    if raw_path.starts_with(b"/proxy/") { CanonicalKind::V1_1 } else { CanonicalKind::V1 }
}
// route matcher gets the same `raw_path`; ntex routes on `request.uri().path()` which is the
// percent-encoded form (same as `req.path()`).
```

A request whose URL is `/proxy/5173/%2e%2e%2fexec` therefore: (1) ntex dispatches via the proxy route handler (the path starts with `/proxy/`), (2) the canonical-version chooser picks v1.1, (3) the v1.1 verifier hashes the path-with-percent-escapes-intact (no decode), (4) the proxy handler forwards `127.0.0.1:5173/%2e%2e%2fexec` (Vite returns 404 because no such asset). At no point does the request reach the `/exec` handler. **Phase-1 regression test (round-6):** a signed `GET /proxy/5173/%2e%2e%2fexec` (with the v1.1 canonical) MUST 401 if the canonical doesn't bind the percent-encoded path, MUST land on the proxy handler if it does, and MUST NEVER land on `/exec`. Negative test: a v1 canonical (no domain-separator tag) sent to `/proxy/...` → 401 with `code: "wrong_canonical_version"`.

**Path-only `proxy.http-v1` capability** still indicates "agent has the proxy handler at all"; **`auth.ed25519-v1.1`** indicates "agent verifies queries". Both must be present for the controller to mint a preview URL.

The signature covers the **agent-facing path** (`/proxy/{port}/{path}`), the canonical-encoded query string (v1.1 only), the request method, the body bytes, the timestamp, and the nonce.

<!-- Round-6 Invariant-1 H1: previous wording ("intermediate header tampering not in threat model") was too strong; it implied the controller→agent link could be MitM-trusted. Reworded to spell out which headers are signed (none), which are trusted, and what the operator must guarantee. -->

**Header-trust contract (round-6).** Headers are NOT in the signature canonical. The trust contract:

| Header | Signed? | Trusted on receive? | Controller-side action |
|---|---|---|---|
| `X-Sbx-Timestamp`, `X-Sbx-Nonce`, `X-Sbx-Signature` | covers themselves | yes (verifies signature) | controller emits |
| `Host` | no | rewrite to agent address | controller rewrites |
| `X-Forwarded-For` | no | **must NOT be forwarded raw to audit** | controller scrubs / replaces with verified peer-IP before any log emission (round-6 H1: a creator's user-app could otherwise inject `X-Forwarded-For: legitimate-customer-ip` and the controller's audit-pipe would record the spoofed IP) |
| `X-Forwarded-Host`, `X-Forwarded-Proto` | no | trusted from controller, untrusted from upstream | controller emits; agent passes through; audit reads only the controller-emitted form |
| Hop-by-hop (RFC 7230 §6.1) | no | dropped | stripped at both edges |
| Custom user-app headers | no | passed through | not logged |

**What we do NOT cover with the signature.** Header tampering between controller and agent. The signed canonical guarantees integrity of (method, path, query, body, ts, nonce) only.

**Operator-blocking precondition (round-6 H9).** The controller→agent leg therefore MUST run on a network where the operator's threat model excludes passive observers. For nomad-ch on a single host this is the host's loopback or a host-private bridge — fine. For multi-host or shared-tenant networks the operator MUST run the controller→agent leg over Wireguard or mTLS; the runbook (`docs/runbooks/sandbox-preview.md`) treats this as a deploy-blocking precondition. The signed canonical's body-hash binding ensures that an attacker who intercepts the leg cannot substitute the body or path; they CAN drop or stall traffic. Confidentiality of the request bytes themselves (e.g., a creator's API key in a query parameter) is the operator's responsibility on this leg, not ours.

**Headers are forwarded transparently** with the following rewrites at the controller:

- `Host` → set to `10.99.<100+idx>.2:7777` (the agent's listen address). Original Host preserved as `X-Forwarded-Host`.
- `X-Forwarded-Proto: https` (the public scheme).
- `X-Forwarded-For` extended with the client IP.
- `X-Sbx-Timestamp`, `X-Sbx-Nonce`, `X-Sbx-Signature` — added by the controller.
- Hop-by-hop headers (RFC 7230 §6.1) stripped: `Connection`, `Keep-Alive`, `Proxy-*`, `TE`, `Trailers`, `Transfer-Encoding`, `Upgrade` (re-added for WS — see below).

#### WebSocket Upgrade handling

The agent must:

1. Verify the signed Upgrade request (the body for an Upgrade is empty; `sha256_hex(&[])` is the well-known empty hash already used in `crates/sandbox-agent/src/sig.rs`).
2. Strip the hop-by-hop headers it doesn't want to forward, but **preserve** `Upgrade: websocket`, `Connection: Upgrade`, `Sec-WebSocket-Key`, `Sec-WebSocket-Version`, `Sec-WebSocket-Protocol`, `Sec-WebSocket-Extensions`.
3. Open a TCP connection to `127.0.0.1:{port}` and replay the request line + headers.
4. Read the upstream response. If it is `101 Switching Protocols`, copy the response headers back to the controller and **splice the two TCP sockets together**.
5. If it is anything else (e.g. 400, 502), return that response as a normal HTTP response.

Splicing pseudocode:

```rust
async fn splice(mut downstream: TcpStream, mut upstream: TcpStream) -> std::io::Result<()> {
    let (mut dr, mut dw) = downstream.split();
    let (mut ur, mut uw) = upstream.split();
    let to_up = compio::io::copy(&mut dr, &mut uw);
    let to_dn = compio::io::copy(&mut ur, &mut dw);
    // First side to error/close ends the WS lifetime.
    futures_util::future::select(Box::pin(to_up), Box::pin(to_dn)).await;
    Ok(())
}
```

(All compio, no tokio — invariant.)

D-8: the **post-Upgrade frames are not signed.** This matches every off-the-shelf reverse proxy. The threat is "an attacker on the wire can MitM the frames"; we rely on platform-edge TLS for confidentiality and integrity of the browser → controller leg, and on the in-cluster network (or Wireguard mesh) for the controller → agent leg. The agent only accepts signed Upgrade requests, which authoritatively names a single user-provided port; once spliced, the agent has no policy to apply per-frame.

**Per-WebSocket bandwidth cap (round-4 C4-12).** The post-Upgrade splice has no policy per-frame BUT it does have a per-connection bandwidth cap to prevent the platform being abused as a tunneled-egress pipe (creator's user-app dials out to attacker.com from inside the VM, then streams those bytes through the WS to a public preview URL). Default `SANDBOX_PREVIEW_WS_BANDWIDTH_BYTES_PER_SEC = 12_500_000` (100 Mbit/s); enforced on each direction independently via a leaky bucket integrated into the splice loop. Excess sleeps the splice (compio yield) until the bucket refills. Metric: `proxy_ws_bandwidth_throttled_total{direction}`. Operator-tunable; bumped via env per-creator tier.

#### Path normalization & smuggling defenses

<!-- Added in round 2: addressing C2-6 (path traversal) and C2-7 (HTTP request smuggling via dual CL/TE). -->

The `{path}` portion of `/proxy/{port}/{path:.*}` is **passed through raw** — no URL-decode, no reencode, no `..` collapsing, no slash-deduplication. The canonical-string and the wire bytes are byte-identical. Mutating the path in the proxy would create canonical/wire divergence and signature failures; not mutating it puts the burden on the upstream (which is correct — Vite, Express, etc. all do their own normalization).

We DO defend against transport-layer smuggling and control-character injection at both the controller and agent edges:

- **Reject any path containing `\r` (0x0D), `\n` (0x0A), or `\0` (0x00).** Returns `400 invalid path` before signing/verifying. These are the bytes used in HTTP smuggling attacks; ntex rejects most by default but we belt-and-suspenders this in our path handler.
- **Reject requests carrying both `Content-Length` AND `Transfer-Encoding: chunked`.** RFC 7230 §3.3.3 calls out the smuggling vector explicitly. The controller and agent both `400 conflicting framing` such requests before reading body or signing. Tested in the agent's existing test scaffold (will add a regression test in Phase 1).
- **Strip chunked extensions.** A chunked body of `5;malicious=true\r\nhello\r\n0\r\n\r\n` is rewritten to `5\r\nhello\r\n0\r\n\r\n` before forwarding. The parser ignores the extension per spec, but stripping eliminates round-trip ambiguity between controller/agent parsers.
- **Reject `Content-Length: 0` paired with a non-empty body.** Frame-disagreement smuggling.
- **Reject any path longer than 8 KiB** (after the `/proxy/{port}/` strip). Pathological inputs.
- **Header total size cap of 32 KiB** (ntex default; documented).

These rules apply to BOTH the public-edge → controller leg AND the controller → agent leg. The signature only covers `(method, path, ts, nonce, body-hash)`; smuggling that reorders a second request inside the body would fail signature verification (the second request's hash isn't in our canonical), but the body-cap and CL/TE rejection close the door earlier.

#### Retry semantics

<!-- Added in round 2: addressing C2-4 — body-substitution oracle on connection-reset retry. -->

Each forward attempt mints a **fresh `(ts, nonce, signature)` tuple**. If the controller's first attempt to the agent fails (TCP RST mid-write, agent 5xx, timeout) and the controller retries, the retry MUST recompute the canonical and re-sign. Reusing `(ts, nonce)` from the first attempt with a different body would create a body-substitution oracle (the agent's nonce LRU would still accept the second `(ts, nonce)` because the agent never saw the first; an attacker placed between controller and agent could then alter the body within the 30 s nonce window).

Implementation rule: the signing function `sign_outbound(...)` is called inside the retry loop, not above it. The agent-side nonce LRU sees each retry as a distinct nonce; honest retries pay one cheap LRU insert. Documented in `crates/sandbox/src/preview.rs::forward_signed`.

<!-- Reworked in round 2: addressing C2-1 — the round-1 "fold key into body hash" canonical mutation conflated body-hash with WS-key-hash, opening a path where a captured non-WS signed POST whose body matches `b"sec-websocket-key=...\n"` could be replayed as an Upgrade. We now define a SEPARATE, domain-separated canonical for WS Upgrade, versioned independently. -->

**Sec-WebSocket-Key binding (round-2 fix).** RFC 6455 makes `Sec-WebSocket-Key` a fresh, random per-connection nonce; the upstream's `Sec-WebSocket-Accept = base64(SHA1(key + GUID))` proves the upstream really saw the same key. Without binding, an attacker who captures the controller's signed Upgrade and re-emits it with a *different* `Sec-WebSocket-Key` (within the 5-second skew window) can hijack the WS via response-header MitM.

We bind by introducing a **separate canonical** for WS Upgrade requests, NOT by mutating the body-hash. This avoids any chance that a non-WS signature collides into a valid WS Upgrade signature:

```
canonical_v1.1     = "ED25519-V1.1\n"      method "\n" path "?" canonical_query "\n" ts "\n" nonce "\n" sha256_hex(body)
canonical_v1.1-ws  = "ED25519-V1.1-WS\n"   method "\n" path "?" canonical_query "\n" ts "\n" nonce "\n" sha256_hex(sec_websocket_key)
```

- `"ED25519-V1.1"` and `"ED25519-V1.1-WS"` are **fixed ASCII domain-separator tags** prepended to every canonical from this version forward. Verifiers reject any canonical that doesn't start with the expected tag.
- The WS canonical's body-hash slot is replaced by `sha256_hex(sec_websocket_key)` (the literal header bytes, lowercased, trimmed). The Upgrade body MUST be empty (RFC 6455 §1.3) — verifier rejects WS Upgrade with a non-empty body before computing the signature.
- The non-WS canonical (`v1.1`) does NOT include a WS-key field; it cannot collide with the WS canonical because the leading domain-separator differs.
- The legacy v1 canonical (no tag) is kept as-is for `/exec`, `/files`, `/tree`; the agent picks the canonical version by the path prefix (`/proxy/...` → v1.1 family; everything else → v1).
- Documented in `auth.ed25519-v1.1` capability: `proxy.ws-v1` REQUIRES `auth.ed25519-v1.1` (controllers refuse a sandbox that lacks one).
- Effect: a captured signature for any non-WS path cannot be replayed as a WS Upgrade (different domain-separator); a captured WS Upgrade signature with a substituted `Sec-WebSocket-Key` fails verification (different key hash).
- Cost: one extra SHA-256 over a ~24-byte input on the WS Upgrade path; negligible.

**Body-hash is derived, never trusted (round-2 clarification).** The agent (and the controller verifying its own canonical against the on-wire request) recomputes `sha256_hex(body_received)` before plugging it into the canonical. The headers carry the signature ONLY; the body-hash claim inside the canonical comes from observed bytes. This is the standard "sign-the-message-not-the-claim" rule; explicit because a future contributor copying the canonical-string code might be tempted to read a "X-Sbx-BodyHash" header and substitute it.

#### Body streaming

For non-Upgrade requests, the wire protocol's body-hash binding (D-7) forces the controller to know the SHA-256 of the body before it can sign. Two options:

1. **Buffer the entire body at the controller**, hash it, sign, then forward to the agent (which buffers again to verify the hash). Simple; bounded memory per request; scales to N concurrent uploads × body cap.
2. **New wire mode `auth.ed25519-v2-streaming`** that signs only `(method, path, ts, nonce)` and uses a per-request HMAC-on-each-chunk envelope. Big new surface; defer.

**v1 ships option 1** with a **per-sandbox concurrency cap** + a per-request body cap, calibrated against agent VM RAM:

<!-- Reworked in round 1: addressing CRITICAL #4 — naive 100 MiB cap × N concurrent uploads OOMs the agent VM. -->

- **Per-request cap:** `SANDBOX_PREVIEW_MAX_BODY_BYTES` = 100 MiB default. Larger → 413 from the controller *before* any byte hits the wire (size taken from `Content-Length`; chunked uploads buffer up to the cap then 413).
- **Per-sandbox concurrent-large-body cap:** `SANDBOX_PREVIEW_MAX_INFLIGHT_LARGE` = 2 default. A "large body" is any signed request with `Content-Length > 1 MiB`. Above this number, the controller queues; if the queue grows beyond `MAX_INFLIGHT_LARGE_QUEUED` = 4, returns 503 with `Retry-After`.
- **Per-controller global concurrent-large-body cap:** `SANDBOX_PREVIEW_GLOBAL_MAX_INFLIGHT_LARGE` = 64 default. Hard upper bound on total controller-side memory pressure: 64 × 100 MiB = 6.4 GiB across all sandboxes, which fits comfortably in any reasonable controller VM.
- **Agent-side: same cap with explicit RAM-bounded enforcement.** The agent VM's typical RAM is 2 GiB (`crates/sandbox/scripts/nomad-vm-wrapper.sh`); 100 MiB body × 2 concurrent = 200 MiB peak agent RAM for proxy buffering. Acceptable.
- **End-to-end memory math (round-4 C4-1).** A single 100 MiB upload is buffered TWICE: once at the controller (to compute SHA-256 + sign), once at the agent (to verify hash + buffer for forward). Peak resident memory per request through the platform is therefore ~200 MiB across two hosts. With 64 concurrent globally on the controller AND ≤ 2 concurrent per agent, the FLEET aggregate worst-case is 64 × 100 MiB (controller) + Σ_per_sandbox 2 × 100 MiB (agents). For 32 active sandboxes this is 6.4 + 6.4 = 12.8 GiB across the fleet. Document the duplication explicitly so a future contributor doesn't try to "eliminate the agent-side buffer" without designing a streaming wire mode.
- **Hash offload.** SHA-256 of 100 MiB ≈ 200 ms on a single core (with SHA-NI; ~500 MB/s). Inline hashing on the request future would block one compio worker for that duration. **For bodies ≥ 1 MiB, hashing is offloaded to compio's blocking-pool** via `compio::dispatch::blocking_run`; small bodies (< 1 MiB) hash inline (cost < 4 ms). Closes the head-of-line stall when 64 concurrent uploads land on the same compio worker.
- **Anything that needs > 100 MiB** is steered to `zeroship.storage` presigned uploads (see § VI R-2). For pre-publish workflows (no app credentials yet) we ship a *preview-mode storage shim* — a zero-config object store keyed by the sandbox's preview secret, accessible via the SDK's `zeroship.storage` API but isolated to the sandbox's lifetime. Documented in § IX cross-cutting.

**Long-term**: ship `auth.ed25519-v2-streaming` (chunk-HMAC envelope; one HMAC per 64 KiB chunk + final commitment) so we can stream uploads through the proxy without buffering. Defer to a follow-up ADR; the v1 caps are explicitly conservative.

The agent reciprocates: the existing `verify_signed(&req, &body, &state)` call already buffers the entire request body via `ntex::util::Bytes` in the `/exec` path. The agent gains a new body cap (`SANDBOX_AGENT_PROXY_MAX_BODY_BYTES`, same default 100 MiB) and a per-process inflight-large counter mirroring the controller's; if either is exceeded the agent returns 413 / 503 directly.

#### Connection lifetime

- **Per-HTTP-request** — opens a new connection to `127.0.0.1:{port}` each time. We do NOT pool. Vite + Node servers are local-process; the cost of a fresh loopback connect is ~50 µs; pooling adds keep-alive idle complexity that isn't worth it for v1.
- **WebSocket** — one upstream socket per inbound WS connection, lives as long as either side stays open.
- **On agent drain** (`/shutdown` flips the drain flag) the proxy returns `503 draining` for new requests. **In-flight HTTP requests** complete normally (ntex's existing drain semantics). **In-flight WebSocket connections** are closed with `1001 Going Away`, RST forwarded to the upstream — the user's app sees the EOF and terminates the WS.

#### Failure modes

| Symptom | Agent response | Why |
|---|---|---|
| Port not listening | `503 port {n} not listening` | Most common — user crashed Vite mid-edit. Vite's WS reconnect sees this and retries. |
| `connect()` ECONNREFUSED | same as above | indistinguishable from "not listening" |
| Upstream timeout (no response in 30 s) | `504 upstream timeout` | configurable per-port; default 30 s headers, 5 min total |
| Upstream `connect()` succeeds, sends garbage | `502 upstream malformed response` | upstream is not HTTP |
| Body too large (> 100 MiB) | `413 payload too large` | enforced before reading body to completion |
| Port not in allow-set | `400 port not allowed` | see below |

#### Port allow-set

<!-- Reworked in round 1: addressing MAJOR #13 (allow-list was confused — 3000/5173/8080 are above 1024, so listing them in a "below 1024" allow-list was redundant) and MAJOR #18 (9229 Node inspector is RCE-capable; must be denied even if user binds it). -->

The agent refuses to proxy:

- **Port 0** — invalid.
- **The agent's own port (7777)** — anti-loop.
- **The Node inspector port (9229)** — the V8 inspector debug protocol over plaintext WS allows arbitrary code execution in the inspected process. Any creator-app exposing 9229 (intentionally or by accident) would otherwise hand the public-edge URL to RCE-as-a-feature. Document and deny outright; the deny-list is *not* env-overridable for 9229.
- **TCP ports under 1024** — privileged-port range; deny by default. The kernel already restricts non-root binds, but PID-1 inside libkrun is root, so a user *can* bind 22 (SSH), 23 (telnet), etc. We do not proxy any of these. There is **no privileged-port allow-list** (the previous draft listed 80/443 as exceptions; in practice creators run dev servers on ≥ 3000 and exposing 80/443 would also conflict with our own edge cert).
- **Any port the controller marks reserved** (operator hook; v1 ships hardcoded).
- **A controller-configurable explicit deny-list** seeded with high-risk service ports across infra categories. Defense-in-depth against creators who unwittingly expose a database, message queue, or service-discovery port to the public preview URL. Operator-overridable but ships restrictive by default.

```rust
const HARDCODED_DENY: &[u16] = &[
    0,
    AGENT_PORT,            // 7777
    9229,                  // Node --inspect / --inspect-brk (RCE-as-a-feature)
    22,                    // SSH (also <1024 below; explicit for documentation)
];
const DEFAULT_DENY: &[u16] = &[
    1099, 5005,            // Java debug (JMX, JDWP)
    6379, 11211,           // Redis, memcached
    27017,                 // Mongo
    3306, 5432,            // MySQL, Postgres
    9200, 9300,            // Elasticsearch HTTP + transport
    5672, 61616, 9092,     // AMQP, ActiveMQ, Kafka
    2379, 2380,            // etcd client + peer
];

fn is_proxyable_port(port: u16, deny_list: &[u16]) -> bool {
    if HARDCODED_DENY.contains(&port) { return false; }
    if port < 1024 { return false; }       // privileged range
    if deny_list.contains(&port) { return false; }
    true
}
```

<!-- Round-6 Invariant-2 I9: defense-in-depth. -->

**Defense-in-depth invariant (round-6 I9).** `is_proxyable_port` is enforced at **both** the controller (in `authorize`, before `forward_signed`) AND the agent (in `proxy()`, before dialing `127.0.0.1:{port}`). The two implementations share the same `HARDCODED_DENY` constant via `crates/core/src/preview_ports.rs`; the dynamic deny-list is configured per-deployment and identical at both layers (controller pushes the list to the agent at sandbox-create via the existing config channel). A controller bug that lets `9229` through still bounces at the agent with `400 port not allowed`. Phase-1 regression test: with a controller patched to accept port `9229` (test-only), assert the agent still 400s.

<!-- Round-2 expansion (C2-27): ports for memcached, Elasticsearch, MQ, Kafka, etcd added; SSH listed explicitly for documentation despite being privileged. -->

#### 127.0.0.1 vs 0.0.0.0 (D-12 detail)

The agent runs inside the microVM as PID 1 (`crates/sandbox-agent/src/main.rs`); user-spawned processes (`/exec node server.js`) are forked off the agent and inherit its netns. The whole VM has one netns. So:

- A user-bound `127.0.0.1:5173` is reachable by the agent dialing `127.0.0.1:5173`.
- A user-bound `0.0.0.0:5173` is reachable as `127.0.0.1:5173` by the agent (same netns, loopback works).
- Vite by default binds `127.0.0.1` unless the creator passes `--host`. We document a recommended Vite config (§ II.5) that sets `host: '0.0.0.0'` for clarity and `allowedHosts` for the preview domain.

We add a regression test (§ IV phase-1) that boots a fixture Vite, confirms agent loopback proxy works for both bind variants.

### II.1.x Header rewriting on the response path

<!-- Added in round-6 (in-place clarification): § II.1 mentioned Set-Cookie Domain stripping and Location rewriting only in passing. This subsection is the concrete spec; the "Header rewrites" stanza in § II.2 cross-references here. -->

#### Why this exists

When the agent forwards a response from the user's app (Vite, Node, anything binding inside the VM) back through the controller to the browser, several headers carry hardcoded references to the in-VM upstream — typically `localhost:5173`, `127.0.0.1:5173`, or the agent's loopback hostname. Without rewriting, the browser sees these and either drops cookies, navigates off the preview origin, or fails CORS preflight. Body content is **NOT** rewritten (per-byte regex on every response is too costly; rewriting breaks streaming and integrity); we rely on the Vite plugin (D-15, see "Interaction with the Vite plugin" below) to make Vite generate URLs with the public preview host via `server.origin`.

The four header transforms below sit at the controller's response-path boundary, AFTER the agent forwards the upstream response and BEFORE bytes are written to the downstream browser. They run on every response, including 3xx redirects, 4xx error responses, and the headers half of WebSocket Upgrade (101). They do NOT run on post-Upgrade WebSocket frames — § II.1 already establishes that the proxy TCP-splices after Upgrade and does not parse frames.

#### Required rewrites

| Header | When it appears | Rewrite | Why |
|---|---|---|---|
| `Set-Cookie: ...; Domain=<x>` | App or framework sets a cookie with explicit `Domain=` attribute | **Strip the `Domain=` attribute entirely.** Cookie becomes host-only on the preview origin. | A `Domain=localhost` (or any value other than the preview host) makes the browser drop the cookie outright (RFC 6265 §5.3 — the cookie's domain MUST domain-match the request-URI's host, else reject). Stripping yields a host-only cookie that works on the preview origin. App authors who want a precise `Domain=` are out-of-scope for preview; production has a different cookie story (`__Host-` prefix on the published-app gateway). |
| `Set-Cookie: ...; Path=<x>` | App may set absolute `Path=` | Pass through unchanged. | `Path=` is path-relative, not host-related. No rewrite needed. |
| `Location: http://<host>:<port>/<path>` (and `https://...`) | App sends a 3xx redirect with an absolute URL pointing at the upstream | **Rewrite host+port** to the preview origin (`https://preview-{slug}-{port}.preview.zeroship.dev`). Preserve path + query + fragment byte-exactly. | A `Location: http://localhost:5173/login` would navigate the browser AWAY from the preview origin (or fail entirely on the user's machine, where `localhost:5173` is unreachable). Relative `Location:` values (`/login`, `./foo`, `?q=1`) pass through untouched — they're already preview-origin-relative. |
| `Refresh: 0; url=<absolute>` | Rare meta-refresh-style header (some older frameworks) | Same as `Location:` — rewrite the absolute URL's host+port. | Same browser navigation hazard as `Location:`. |

Controller-side function shape (pseudo-Rust):

```rust
/// Run on every response, after `agent_forward(...)` returns, before writing to the
/// downstream client. Mutates the header map in place.
fn rewrite_response_headers(headers: &mut HeaderMap, preview_origin: &str) {
    // 1. Strip `Domain=` attribute from every Set-Cookie value (cookie parsing must
    //    handle multi-cookie responses: ntex/http exposes one Set-Cookie header per
    //    cookie via headers.get_all("set-cookie"); preserve emission order).
    rewrite_set_cookie_strip_domain(headers);

    // 2. Rewrite absolute Location host+port; relative Locations untouched.
    rewrite_absolute_location(headers, preview_origin);

    // 3. Same treatment for Refresh header's url= parameter.
    rewrite_refresh_url(headers, preview_origin);
}

fn rewrite_set_cookie_strip_domain(headers: &mut HeaderMap) {
    // Iterate all Set-Cookie values (browser cookie-jar uses last-write-wins on
    // equal name+path; preserve relative order). For each value, parse
    // attributes case-insensitively; drop any attribute whose key matches
    // `Domain` (ASCII case-insensitive); reassemble; re-emit.
}

fn rewrite_absolute_location(headers: &mut HeaderMap, preview_origin: &str) {
    // If Location starts with http:// or https:// (case-insensitive scheme),
    // parse host+port; replace with preview_origin's host+port; preserve
    // path + query + fragment byte-exactly. Else: pass through.
}
```

**Cookie-parsing note.** A response can carry multiple `Set-Cookie` headers (one per cookie); ntex exposes them via `headers.get_all("set-cookie")`. The rewriter MUST iterate all values, MUST preserve their relative order (browser cookie-jar is last-write-wins on equal name+path; reordering changes user-visible state), and MUST handle attribute-key matching case-insensitively (per RFC 6265 §5.2 the attribute names like `Domain` and `Path` are case-insensitive). Edge case: a quoted attribute value (`Domain="foo.example"`) is non-standard but seen in the wild; the parser MUST handle quoted forms or fall back to "if Domain= appears anywhere in the value, strip the whole attribute pair through the next `;`".

#### What we deliberately do NOT rewrite

- **Response body content (HTML / JS / CSS / JSON).** Per-byte regex on every response is too expensive on the hot path (latency budget § XII assumes header-only handling); false positives are inevitable (a `localhost:5173` literal inside a code-block or a string constant becomes a silent bug); streaming responses can't be rewritten without buffering, which breaks SSE and HMR. The Vite plugin's `server.origin` setting causes Vite to generate canonical URLs with the public preview host upstream of us; for non-Vite apps the user is responsible for using relative URLs or `window.location.origin`.
- **WebSocket frames after Upgrade.** § II.1 establishes that we TCP-splice after the 101 Switching Protocols handshake; frames are opaque bytes to the proxy. Rewriting frame payloads would require terminating the WS protocol and re-framing, which we explicitly do not do.
- **`<base href="...">` HTML tag.** Same reason as body content — opaque to the proxy. If a user app emits `<base href="http://localhost:5173/">`, the browser sees it and breaks. Document this as a known pitfall.
- **CORS `Access-Control-*` response headers.** Pass through verbatim; the user's app or the Vite plugin owns CORS policy. The proxy does NOT inject `Access-Control-Allow-Origin`; if the upstream emits it pointing at `localhost:5173`, the browser will reject the preflight — which is correct behaviour, the app is misconfigured. Q-19 (below) tracks the corner case of console-side CORS allowlist for legitimate cross-origin fetches FROM preview TO console.
- **`Content-Security-Policy` from the upstream.** Pass through. § II.6 ("CSP at the preview origin") establishes that the platform sets a baseline CSP at the public edge for preview origins; if the upstream adds its own CSP, browsers AND-merge them per spec. Rewriting CSP would entangle us in the user app's security posture.

#### Request-path headers we DO rewrite

When forwarding browser → controller → agent → upstream, the controller MUST rewrite a few request-path headers as well. These are listed here (rather than in the existing § II.2 "Header rewrites" stanza, which is now a cross-reference) so request and response sides sit side-by-side:

| Header | Action | Why |
|---|---|---|
| `Host:` | **Pass-through** to the upstream (the Vite plugin's `server.allowedHosts` accepts the preview hostname pattern). | Pass-through is the simpler choice. The Vite plugin (see "Interaction with the Vite plugin" below) is the source of truth for which hosts the dev server accepts; rewriting `Host` to `localhost:5173` would defeat origin-based defenses Vite has (and would also break Vite's own absolute-URL generation in some plugin paths). § II.2's existing line "`Host` → `10.99.<100+idx>.2:7777`" applies only to the **controller→agent** TCP/HTTP layer, not to the agent→upstream-app layer; the agent passes the original `Host` through to `127.0.0.1:{port}`. |
| `Origin:` | Pass-through (the original browser-supplied Origin: `https://preview-{slug}-{port}.preview.zeroship.dev`). | Required for Vite's HMR + cors middleware to see the real browser origin and validate it against `server.cors.origin` config. Rewriting Origin to `null` or `localhost` would silently break CORS. |
| `X-Forwarded-For` | Append the immediate client IP (RFC 7239 / de-facto comma-separated chain). Per round-6 invariant-1: the audit-log path MUST scrub anything before our hop before logging — XFF is untrusted as a security signal. | Standard practice; documented as an audit-log signal but the Vite app shouldn't trust it for auth decisions. |
| `X-Forwarded-Host` | Set to the preview hostname (`preview-{slug}-{port}.preview.zeroship.dev`). | So the upstream app knows what hostname the browser sees, in case the app generates absolute URLs (e.g., for emails, OAuth callbacks). Without it, an app that reads `Host:` for absolute-URL generation would still work (because we pass `Host` through); apps that already use `X-Forwarded-Host` get the right answer. |
| `X-Forwarded-Proto: https` | Set unconditionally (the controller→agent leg is plaintext HTTP/1.1, but the browser-side connection IS TLS). | Tells the upstream the browser-side connection is TLS. Some frameworks use this for URL generation (Express's `req.protocol` with `trust proxy`, Rails's `request.ssl?`). |

(The existing § II.2 "Header rewrites" stanza is preserved as a one-line summary; this subsection is now the canonical spec.)

#### Interaction with the Vite plugin (D-15)

The Vite plugin (`@zeroship/vite-preview`, packaged in `sdks/vite-plugin-preview/` — referenced in § IX cross-cutting follow-ups and Appendix A.1) handles app-layer URL configuration. Without it, the proxy alone is not sufficient to make a typical Vite dev server reachable from the public preview origin. The plugin sets, at minimum:

- `server.host = '0.0.0.0'` — so the agent's loopback dial reaches the dev server (D-12 establishes that `0.0.0.0` and `127.0.0.1` binds are equivalent inside the VM netns; this is for clarity).
- `server.allowedHosts = [<preview-host-pattern>]` — so Vite doesn't reject our incoming `Host:` header (Vite's anti-DNS-rebinding default).
- `server.origin = 'https://preview-{slug}-{port}.preview.zeroship.dev'` — so Vite-generated asset URLs (e.g., `<script type="module" src="...">` injected by `vite/client`) use the public host. **This is the load-bearing piece for the "no body rewriting" decision** — without `server.origin`, Vite emits asset URLs with `http://localhost:5173/...` baked into the served HTML, and we would be stuck with the choice of either body-rewriting (rejected) or broken pages.
- `server.hmr.host = <preview-host>`, `server.hmr.clientPort = 443`, `server.hmr.protocol = 'wss'` — so the in-browser HMR client dials our preview origin via the WebSocket proxy path. Without this, the HMR client dials `ws://localhost:5173` from the browser, which doesn't exist on the user's machine — HMR silently dead.
- `server.cors = { origin: <preview-host-pattern> }` — so cross-origin module imports during HMR are accepted.

The plugin auto-detects `ZEROSHIP_PREVIEW_HOST` (controller-injected env var; see § II.9 "Env-var threat model" for the full list of preview-related env vars exposed to the VM) and computes the host pattern; the creator does not configure it manually.

For non-Vite apps (Express, Hono, plain Node servers, Python frameworks), the user must either:
- Use relative URLs everywhere (`fetch("/api/foo")`, not `fetch("http://localhost:5173/api/foo")`), or
- Read `Host:` / `X-Forwarded-Host:` from the request and use that for absolute URL generation (the standard reverse-proxy pattern).

The proxy CANNOT magically make a hardcoded `http://localhost:5173/api` in user JS work. This is documented in Appendix A.2 ("Manual config / non-Vite frameworks").

#### Edge cases worth calling out

1. **`<base href="...">` HTML tag.** If user HTML hardcodes a base URL pointing at `localhost`, we don't rewrite it. Document for users; the AI builder system prompt (§ IX cross-cutting follow-ups) should instruct the model to avoid emitting absolute base hrefs.
2. **Service workers.** A user app's service worker MUST be served from the preview origin (it is, automatically — the proxy serves it under the preview host with no rewrite needed beyond what's already covered above). Scoping is `/`. The browser registers it under the preview origin; on sandbox teardown the SW outlives the sandbox in the user's browser cache, but the next fetch to the preview origin returns 404 from our edge, and the SW's `fetch` handler typically falls back gracefully or the user dismisses the tab. No platform-side action needed.
3. **CORS preflight on cross-origin fetch from preview to console.** Preview origin (`*.preview.zeroship.dev`) and console (`console.zeroship.ai`) are different eTLD+1. Any app fetch from preview → console must clear CORS preflight; the console-side allowlist policy for which preview origins it accepts is **Q-19** (new open question, see § VIII).
4. **Set-Cookie ordering.** When stripping `Domain=` from multiple `Set-Cookie` headers, the rewriter MUST preserve their emission order (browser cookie-jar tracks last-write-wins on equal name+path; reordering can flip which cookie survives in the jar).
5. **Absolute Location pointing at a host the app doesn't know about.** If the upstream emits `Location: http://example.com/...` (a redirect to a third-party site), we do NOT rewrite — only host+port matching the upstream loopback patterns get rewritten. v1 implementation: rewrite if-and-only-if the URL's host is `localhost`, `127.0.0.1`, `0.0.0.0`, or `[::1]` (with optional port). Any other host passes through. (We do NOT match the agent's loopback IP `10.99.<idx>.2` in the rewrite rule because the user app inside the VM sees its own loopback, not the agent's; the agent's IP only appears on the controller→agent leg, which is not exposed to the upstream.)
6. **Set-Cookie with no Domain attribute.** Pass through unchanged (the cookie is already host-only on the preview origin, which is what we want).

### II.2 Controller endpoint: `ANY /sandboxes/{id}/preview/{port}/{path*}`

This is the **internal** authenticated forwarder used by both:
- the **public** edge route (`preview-{id}-{port}.preview.zeroship.dev/...`), and
- direct console-side fetches from `console.zeroship.ai` (creator UI).

```rust
// crates/sandbox/src/handlers.rs (new module: preview.rs)
pub async fn preview_proxy(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(Uuid, u16, String)>,
    body: Bytes,
) -> HttpResponse {
    // 1. AuthN: bearer creator-token OR signed share-token.
    //    Round-6 H4: do NOT differentiate "auth failed" vs "no sandbox" vs "not owner" on the wire.
    //    Audit log carries the actual reason; the response is uniform.
    let principal_opt = authn(&req, &state).await.ok();

    let (sandbox_id, port, sub_path) = path.into_inner();

    // Single coalesced check: principal must be Some, sandbox must exist, principal must own it.
    // Any of these failing produces an identical 404 on the wire.
    let info_opt = state.registry.get(sandbox_id);
    let authorized = match (&principal_opt, &info_opt) {
        (Some(p), Some(info)) => authorize(p, info, port).await,
        _ => false,
    };

    if !authorized {
        // round-6 H4 (refined): audit the actual reason — one of four distinct
        // post-auth failure modes — but return uniform 404 on the wire.
        let reason = match (&principal_opt, &info_opt) {
            (None, _)                        => "auth-failed",
            (_, None)                        => "sandbox-not-found",
            (Some(p), Some(info))            => {
                if !is_proxyable_port(port, &state.deny_list) {
                    "port-denied"
                } else if !owns(p, info) {
                    "not-owner"
                } else {
                    "denied-other"   // future-proofing; should not occur today
                }
            }
        };
        audit::record_authz_failure(&req, sandbox_id, port, reason).await;
        return err(404, "not found");           // wire response is uniform
    }

    // 3. Lookup agent_url + signing_key for this sandbox. (info_opt is Some at this point.)
    let info = info_opt.unwrap();
    let agent_url = info.agent_url();           // http://10.99.<100+idx>.2:7777
    let signing_key = info.signing_key();       // controller-side per-sandbox SK

    // 4. Sign + forward.
    let outbound_path = format!("/proxy/{port}/{sub_path}");
    forward_signed(req, body, &agent_url, &outbound_path, signing_key).await
}
```

#### `authorize` semantics (round-6 Invariant-2 CRITICAL-2)

<!-- Round-6 Invariant-2 CRITICAL-2: authorize was previously a one-line stub. -->

```rust
async fn authorize(
    principal: &Principal,        // creator session OR validated share-token claims
    info:      &SandboxInfo,      // already-looked-up registry record
    port:      u16,
) -> bool {
    // 1. Port must be in the proxyable allow-set (defense-in-depth; agent ALSO checks).
    if !is_proxyable_port(port, &state.deny_list) { return false; }

    // 2. Sandbox-owner check is the SOLE authorization gate.
    match principal {
        Principal::Creator(creator_id)  => info.user_id == *creator_id,
        Principal::ShareToken(claims)   => {
            // share token already validated against this exact (sbx, port) by the public-edge
            // auth layer; this re-check is belt-and-suspenders against principal-construction bugs.
            claims.sbx == info.sandbox_id && claims.port == port
        }
    }
}
```

**`authorize` is the SOLE gate.** No other code path may dispatch to a sandbox without going through this function. Reviewers: every call site that touches `agent_url`, `signing_key`, or `forward_signed` MUST be preceded by an `authorize` call against the same `(principal, info, port)` triple. The compiler enforces this via a non-`Clone`, non-`Copy` `AuthorizedDispatch` token returned only by `authorize`; `forward_signed` requires it as a parameter:

**Port-deny is one of four authorization-layer reasons that all surface to the wire as 404 uniform** (`auth-failed`, `sandbox-not-found`, `not-owner`, `port-denied`). The reason a port-not-allowed request returns 404 here — not 400 — is oracle safety: differentiating port-deny from sandbox-not-found leaks the existence of arbitrary sandbox IDs to anyone who can guess one and probe with port 22. The agent layer's port-deny stays at 400 (defense-in-depth bounce; only reachable internally by a misconfigured controller, no public oracle concern). The audit log records the specific reason on every authorize failure; the wire is uniform.

```rust
pub struct AuthorizedDispatch { _private: () }   // not constructable outside authorize()
async fn forward_signed(
    _gate: AuthorizedDispatch,                    // proof of authorization
    req: HttpRequest, body: Bytes, agent_url: &str, path: &str, key: &SigningKey,
) -> HttpResponse { /* ... */ }
```

**Phase-3 regression test (added in round 6):** boot two sandboxes (creator-A's and creator-B's). Issue a request from creator-A's authenticated browser session against `preview-<creator-B-slug>-5173.preview.zeroship.dev`. Expected: 404 (not 403, not 401) per H4. Repeat with the internal route `GET /sandboxes/<creator-B-sandbox-id>/preview/5173/foo` carrying creator-A's bearer. Expected: 404. Audit log records `not-owner-or-port-denied`.

#### Streaming pipe

The controller MUST NOT buffer responses. Especially for HMR (tight WebSocket frames) and SSE (the AI builder dispatches token streams), every millisecond of buffering shows up as creator-facing lag. The forward loop:

- Reads chunks from upstream (the agent) using compio's async IO.
- Writes them to the downstream client (the browser) without waiting for the full body.
- Honours the upstream's `Content-Length` or `Transfer-Encoding: chunked` framing.
- On client disconnect (downstream closes), **cancels the upstream request** (drops the request future, which closes the controller→agent socket, which causes the agent to close the agent→user-app socket).

<!-- Added in round 1: addressing CRITICAL #6 — ntex's stock service surface doesn't expose response body as an async stream; spell out the implementation path. -->

**Implementation note (round-1, sizing reworked round-4).** ntex's high-level `web::HttpResponse` builder buffers the body. For streaming, the proxy uses `ntex::http::body::BodyStream` (or `BoxedBodyStream`) and constructs the response with `HttpResponseBuilder::streaming(...)`. Connector side: we reuse `compio::net::TcpStream` directly for the upstream call rather than wrap it in `awc`/`reqwest` (no tokio dependency). The controller→agent code path is therefore a thin compio-based HTTP/1.1 client, ~200 lines.

Connection pool: a per-sandbox bounded queue **(max 64 idle connections, idle-timeout 30 s)** — distinct from agent→app (no pool, see "Connection lifetime" below). Sizing rationale (round-4 C4-5): Vite's first-load fetches ~50 modules; with HTTP/1.1 keep-alive but no h2 multiplexing on the backend leg, ≥ 50 parallel connections are needed to avoid a head-of-line queue. 64 caps each sandbox; 1k sandboxes × 64 = 64k fds — within the budget in § XI.6. Per-controller global cap `SANDBOX_PREVIEW_BACKEND_POOL_MAX = 32_768` prevents a runaway sandbox from starving the rest. (Long-term: HTTP/2 to the agent on a single shared connection eliminates the need for the pool; deferred — same h2-frontend / h1-backend that ntex supports already wins for the public-edge leg.)

**Request-handling precedence (round-4 C4-14).** The controller's preview path applies checks in this fixed order, cheapest-first:

1. Host parse → 421 if format invalid.
2. Rate limit (per-IP → per-sandbox → per-creator) → 429 if any bucket empty.
3. AuthN (cookie OR share token) → 401 if invalid.
4. AuthZ (principal allowed for sandbox+port) → 403 if denied.
5. Circuit breaker check → 503 `code: "circuit_open"` if open.
6. Body size cap → 413 if `Content-Length` over cap.
7. Hash + sign → forward.

Rejecting before signing keeps the expensive cryptographic path unloaded under abuse.

**Cache-Control passthrough (round-4 C4-10).** The proxy preserves upstream `Cache-Control`, `ETag`, `Vary`, `Last-Modified` headers verbatim — the user's app decides cacheability of its responses. The proxy injects no caching headers EXCEPT on platform-control endpoints (`__zsbx_login`, `__zsbx_share`, `/__zsbx_*`), which always carry `Cache-Control: no-store`. Documented so future contributors don't add "for performance" caching that bypasses the user app's own cache directives.

#### Circuit breaker + deep health

<!-- Added in round 3 (C3-2): without a deep health check, a wedged agent (livez OK, proxy syscall stuck) hides behind /livez green and every request 504s for 30 s. -->

The controller maintains a per-`(sandbox, port)` circuit breaker:

```
state: Closed → Open (after >= 3 consecutive 504/timeout/connect-refused in 30s)
                ↓ wait 5 s
              HalfOpen → (success) → Closed
                       → (failure) → Open (next wait = min(prev*2, 60s))
```

- **Closed:** all requests forwarded.
- **Open:** controller short-circuits with `503 {code: "circuit_open", retry_after_ms: <wait>}` without forwarding. WebSocket Upgrade requests likewise rejected.
- **HalfOpen:** the next single request is forwarded; success closes the breaker, failure re-opens with exponential backoff.

In addition, the controller probes the agent's *proxy path* directly via a signed `HEAD /proxy/_self/healthz` every 60 s while the breaker is closed. This is a synthetic loopback request that the agent serves locally (without dialing user code); a non-200 within 5 s flips the breaker open without waiting for organic traffic to fail. New agent capability bit `proxy.healthz-v1` (advertised by agents implementing this endpoint; controllers tolerate older agents by skipping the synthetic probe).

Metrics: `proxy_circuit_breaker_state{sandbox,port}` (gauge: 0 closed / 1 half / 2 open), `proxy_circuit_breaker_transitions_total{from,to}`.

Alert: `proxy_circuit_breaker_state == 2 for > 5 min` → page (the sandbox is unreachable beyond a transient blip).

#### Header rewrites

Outbound (controller → agent):
- `Host` → `10.99.<100+idx>.2:7777`
- `X-Forwarded-Host` → original public Host
- `X-Forwarded-Proto: https`
- `X-Forwarded-For` extended

Outbound on response (agent → browser):
- Strip `X-Sbx-*` (debug-only; we never want to leak controller-internal trace IDs to creators).
- Strip `X-Powered-By: ntex` (the agent's default).
- Preserve `Set-Cookie`, `Cache-Control`, etc. — pass through.

#### Vite gotchas

Vite's dev server has three things that break naïve proxying:

1. **`server.allowedHosts`** — by default `[localhost, 127.0.0.1, ...]`. Without explicit allow-listing, Vite returns `403 Invalid Host header`. We document the canonical config (§ II.5).
2. **`server.hmr.clientPort`** — the HMR client connects back to a configurable URL. If not set, it tries to reach the same host:port the page came from, with `ws://`. Our preview is `https://`, so we need `clientPort: 443` and `protocol: 'wss'`.
3. **`base: './'`** — for assets. With path-based URLs (rejected in D-3 in favour of subdomain) this would matter; with subdomain routing it's a no-op.

### II.3 Public DNS + edge

```
*.preview.zeroship.dev    A    <controller public IP>
                          AAAA <v6 if applicable>
```

Wildcard certificate via Let's Encrypt DNS-01 (we already do this for `*.zeroship.ai` per the existing gateway). Cert lives at the controller's edge, terminated by the controller's TLS listener.

**Hostname format:** `preview-{sandbox-slug}-{port}.preview.zeroship.dev`

<!-- Reworked in round 1: addressing MAJOR #8 — the typed-id format `sbx_<base62>` contains an underscore, which is illegal in DNS labels per RFC 1035 §2.3.1 and is rejected by some browsers + most DNSSEC validators. -->
<!-- Reworked in round 2: addressing C2-23 (UUIDv7 entropy honesty), C2-8 (Host-header parsing rule made explicit). -->

- `sandbox-slug` is the typed-id with the `_` replaced by `-` (e.g. `sbx_01HF…` → `sbx-01HF…`). The slug is a **DNS-safe rendering** of the typed-id; the canonical typed-id continues to use `_` everywhere except DNS labels. The conversion is purely textual; we do NOT change the typed-id format.

  <!-- Round-6 Invariant-1 CRITICAL-5: case-folding round-trip. -->
  **Slug case invariant (round-6 CRITICAL-5).** Subdomain Host headers are case-insensitive per RFC 1035 §2.3.3 and RFC 4343, but base62 typed-ids are case-sensitive (`sbx_01HfA` ≠ `sbx_01HFa`). To eliminate the round-trip ambiguity we PIN the slug to **lowercase only**: `sandbox-slug ::= sbx-[a-z0-9]{20,40}`. The typed-id generator is constrained to emit only lowercase base62 (digits + lowercase letters, 36 symbols — entropy reduced from 128 bits to ~103 bits over the same 22 chars; still infeasible to brute-force). Any incoming typed-id with an uppercase letter on the wire is rejected (`400 invalid_typed_id`); existing typed-ids that contain uppercase letters (legacy from before this constraint landed) are migrated by a one-shot rename pass during Phase-0. The slug→typed-id round-trip is now byte-identical: `slug.replace("-", "_") == typed_id`, no case-folding, no collisions.

  Cross-reference: `crates/core/src/typed_id.rs` ships a `TypedId::Lowercase` constructor used by every preview-touching path. The general-purpose typed-id constructor remains case-sensitive for backward-compat with non-preview entities.
- `port` is decimal `[1–65535]`.
- DNS subdomain length cap: 63 chars per label. `preview-` (8) + `sbx-<22>` (26) + `-` (1) + `<5>` = 40 chars. Comfortably under the 63-char cap and the 253-char total length cap.
- **Slug entropy (corrected).** UUIDv7 is `[48-bit ms timestamp | 12 bits | 4-bit version | 12 bits | 2-bit variant | 62 bits random]` per draft-ietf-uuidrev-rfc4122bis. After base62-encoding the 128-bit value, the slug carries ~74 random bits and ~48 timestamp-derived bits. An attacker who knows the creation window (e.g. from a leaked screenshot) narrows the timestamp to a small range, leaving ~74 bits to brute force — still infeasible (2^74 ≈ 10^22) but the right number to put in the doc, not 2^131. (The earlier "2^131 on the slug alone" claim treated all bits as uniformly random; they aren't.)
- **The slug is not a security boundary.** A leaked subdomain (Discord paste, screenshot, Slack thread) is *expected* to be reachable; what gates access is the per-sandbox auth (creator cookie or share token). Anonymous-by-default mode (Q-2) is documented as having no slug-based protection.

#### Host-header parsing (anti-rebinding)

<!-- Round-2 (C2-8): make the parsing rule fully explicit. -->

The controller's public-edge handler MUST validate the `Host` header before any routing:

```rust
fn parse_preview_host(raw: &str) -> Result<(SandboxSlug, u16), HostError> {
    let lower = raw.trim().to_ascii_lowercase();
    let lower = lower.strip_suffix('.').unwrap_or(&lower);          // tolerate trailing dot
    let host  = lower.rsplit_once(':').map(|(h, _p)| h).unwrap_or(&lower); // strip :port
    // IDN normalize. Reject if the input was non-ASCII *and* doesn't equal its punycode form.
    let ascii = idna::domain_to_ascii_strict(host).map_err(|_| HostError::Idn)?;
    if ascii != *host { return Err(HostError::Idn); }
    let m = PREVIEW_HOST_RE.captures(&ascii).ok_or(HostError::Format)?;
    Ok((SandboxSlug(m["slug"].into()), m["port"].parse()?))
}

// ^preview-(sbx-[a-z0-9]{20,40})-([1-9][0-9]{0,4})\.preview\.zeroship\.dev$
//                ^^^^^^^^^^^^^^^^ lowercase base62 only — see "Slug case invariant" above
static PREVIEW_HOST_RE: Lazy<Regex> = Lazy::new(|| ...);
```

- Trailing-dot is tolerated (some clients append it).
- `:port` suffix is stripped.
- Input must be its own punycode (rejects Unicode confusables like Cyrillic `х`).
- A trailing-segment match enforces the `.preview.zeroship.dev` suffix; anything else returns `421 Misdirected Request` (NOT 404 — 421 signals to the client that this Host can't be served here, which discourages broken clients from caching).

**Subdomain rationale (D-2, D-3):**

- **Cookie isolation.** A cookie set by `preview-foo-5173.preview.zeroship.dev` is scoped to that subdomain; cannot bleed to `preview-foo-3000.preview.zeroship.dev` (different host).
- **CSP / origin.** Each preview is a distinct origin. A creator running an XSS-prone test app can't cross-origin attack their own auth domain.
- **DNS-level blackhole.** If a sandbox needs to be killed (CSAM, abuse), we can return NXDOMAIN for that subdomain at the authoritative DNS layer without code changes.

**Why not the bare `*.zeroship.dev`?** That root is shared with `console.zeroship.dev` (planned), `auth.zeroship.dev`, and any other platform-level subdomain. Cookie scoping with `Domain=zeroship.dev` would bleed. Subdomaining to `*.preview.zeroship.dev` makes the boundary explicit.

#### Anonymous access

Two access modes (D-4):

- **Creator session cookie (POST handshake — round-2 rewrite).** The creator is logged into `console.zeroship.ai`. To access `preview-foo-5173.preview.zeroship.dev` (a different origin), the console **POST-form-submits** a short-lived bearer token to the preview origin's `__zsbx_login` endpoint. The token is in the request body, never in a URL or query string:
  ```html
  <!-- emitted by console.zeroship.ai when the creator clicks "Open preview" -->
  <form method="POST"
        action="https://preview-foo-5173.preview.zeroship.dev/__zsbx_login"
        target="_top">
    <input type="hidden" name="token" value="<short-lived-jwt-from-console>">
    <input type="hidden" name="next"  value="/">
    <noscript><input type="submit" value="Open preview"></noscript>
  </form>
  <script>document.currentScript.previousElementSibling.submit();</script>
  ```
  - The handler sets HttpOnly cookie `__Host-zsbx_preview_<sbx>=<sandbox-scoped-jwt>` then 303-redirects to a sanitized `next`.
  - `next` is **path-only**: AFTER `percent_decode`, must match `^/[a-zA-Z0-9._/~-]{0,200}$`, must NOT contain `//`, `\`, `%2f`, `%5c`, `%2e%2e`. Anything else is replaced by `/`. The redirect target MUST be on the same origin as the share endpoint (the redirect is constructed as `Location: <decoded-next>`, the browser keeps it on the current origin; we do NOT emit absolute URLs). <!-- Round-6 Invariant-1 H2: validate AFTER percent-decode, reject `%2f`/`%5c`. -->
  - The request requires `Origin: https://console.zeroship.ai` (or a configured per-region console origin); cross-origin POSTs from elsewhere are rejected.
  - Sec-Fetch-Site/Mode/Dest enforcement (§ II.6) applies.
  - Cookie TTL = 1 h; refresh by hitting `__zsbx_login` again.

  **Login JWT contract (round-6 Invariant-1 CRITICAL-2).** The short-lived JWT carried in the form body is fully specified:

  | Claim | Value |
  |---|---|
  | `iat` | issue time (unix s) |
  | `exp` | `iat + 30` (30-second TTL — minimum viable for a top-level navigation) |
  | `aud` | `"login"` (literal; preview validators reject; share-token validators reject) |
  | `iss` | `"console.zeroship.ai"` (or per-region console FQDN) |
  | `sub` | creator's typed-id (e.g., `usr_01HF…`) |
  | `sbx` | sandbox typed-id this token is scoped to |
  | `cv_h` | SHA-256 of a PKCE-style code-verifier the console generated and held in `sessionStorage` |
  | `jti` | random 128-bit identifier for replay defeat |

  The console must also POST a header `X-PKCE-Verifier: <verifier>` whose `sha256_hex(verifier)` matches `cv_h`. This binds the JWT to a value that lives only inside the console's `sessionStorage` and is NEVER reflected back into any URL or cookie; an XSS on the preview origin cannot exfiltrate the verifier (different origin), and an XSS on the console origin would already need to read sessionStorage AND replay within the 30-second window AND defeat the `jti`-LRU. (TLS channel-binding via RFC 8471 was considered; rejected — too few TLS stacks expose the keying material in a portable way.)

  **Server-side jti LRU.** The preview origin's `__zsbx_login` handler maintains an in-process LRU keyed on `jti` with a 30-second TTL. On JWT verify, the handler:

  1. Decodes + verifies signature (the console signs JWTs with a per-region key the controller knows).
  2. Validates `iat <= now + 5`, `exp > now`, `aud == "login"`, `iss` is in the allowlist.
  3. Validates `cv_h == sha256_hex(X-PKCE-Verifier)`.
  4. **Rejects if `jti` is already in the LRU (replay).** Otherwise inserts.
  5. Validates `sub` corresponds to a creator-still-in-good-standing (account active, not banned).
  6. Validates `sbx` matches the path-bound sandbox-id AND `info.user_id == sub`.
  7. Sets the cookie, deletes the JWT (single-use guaranteed by the LRU), 303-redirects.

  The LRU is per-controller; in single-controller-per-sandbox v1, this is sufficient (a JWT is bound to one sandbox which has exactly one controller). When sandboxes go multi-controller (Q-14), the LRU moves to compio-redis.

  Failure modes log at WARN with the failure category but never surface the failed claim values to the client (would otherwise oracle-leak which check failed).

  **Why POST not GET.** A GET with `?token=<jwt>` lands the secret in the browser address bar, history, the navigation API, possibly extension tab-sync, and any 3xx Location header rendered to the user. POST with `target="_top"` does the same UX (full-page navigation) without the URL-leak. Tradeoff: requires JS for the silent submit (the `<noscript>` button is the graceful fallback). Documented in § VI R-22.

- **Share token.** A signed token in the query string converts to an HttpOnly cookie via the same mechanism (`__zsbx_share?t=<token>` → cookie). After conversion, the URL bar shows a clean path. The cookie is `__Host-zsbx_share_<sbx>=<jwt>`; HttpOnly; Secure; **`SameSite=Strict`** (round-2 upgrade from Lax — see C2-17 / R-22 below); Path=/; no Domain attribute (host-only). Residual: the share URL itself contains the token and ends up in browser history; recommend ≤ 1 h TTL for shares pasted into chat. § II.4 covers minting.

  <!-- Round-6 Invariant-1 CRITICAL-1: lock the SameSite values byte-exact; both this section and § II.6 must agree. -->
  **Cookie SameSite invariant (round-6).** Within this design the share cookie is **always** `SameSite=Strict` (the cross-site context where Lax would matter — pasted-link UX from Slack — is preserved by the `?t=` first-hit, which carries no cookie state across origins; the redirect after `__zsbx_share` is same-origin, so `Strict` does not break the flow). The login cookie `__Host-zsbx_preview_<sbx>` is `SameSite=Lax` because the POST handshake is a top-level navigation from `console.zeroship.ai` — a different origin — and `Strict` would block the cookie from being sent on the redirect. § II.6 documents these two values byte-exactly so a future contributor doesn't accidentally swap them.

  - `next=` validation as above (path-only).
  - Token-conversion responses include `Cache-Control: no-store`, `Referrer-Policy: no-referrer` (to suppress Referer on the redirect target's outbound resource fetches), and `Clear-Site-Data: "cache"` where supported (best-effort hint to drop the URL from cache; does not clear history per spec).

### II.4 Share tokens (optional public preview)

#### Flow

```
Creator (console)
  POST /sandboxes/{id}/preview/{port}/share
  body: {expires_in_secs: 3600, scope: "ro"}
  → 200 OK
    {
      "token_id": "shr_<22-char-base64url>",
      "share_url": "https://preview-foo-5173.preview.zeroship.dev/__zsbx_share?t=<base64>",
      "expires_at_unix": 1746139200,
      "scope": "ro"
    }
```

> **Implementation note (Phase 3 / round-7 alignment).** The wire-stable
> `token_id` is `shr_` + the raw 22-char base64url `tid` claim (16 random
> bytes, no padding). The internal storage (`tid` claim, audit-table key,
> sealed-record entry) holds the raw `tid` bytes verbatim — the `shr_`
> prefix is added at the JSON boundary in `POST /share` and `GET /share`
> response builders, and stripped (where needed) when accepting future
> `DELETE /share/{token_id}` parameters. This keeps the prefix
> presentation-only — no migration of stored audit rows or sealed records.

#### Token format

<!-- Reworked in round 1: \n-separator/sv issues. Reworked in round 2: addressing C2-2 (audience binding), C2-12 (parser hardening), C2-13 (rotation race), C2-20 (iat unvalidated). -->

JSON-then-base64url, HMAC-SHA-256 separately:

```
payload_json = {
  "v": 1,                           // token format version
  "aud": "preview",                 // audience tag — see "Audience binding" below
  "iss": "usr_01HF…",               // round-6 H7: issuer = creator typed-id; gates abuse on revocation
  "sbx": "sbx_01HF…",               // sandbox typed-id (lowercase base62; round-6 CRITICAL-5)
  "port": 5173,
  "exp": 1746142800,                // unix seconds (validate-side ceiling: exp <= now + 604800; round-6 LOW-4)
  "iat": 1746139200,                // unix seconds (issued-at; validated, not just audited)
  "scope": "ro",                    // see scope strings below
  "sv": 1,                          // secret_version this token is bound to
  "ti": 0                           // token_index (per-token revocation hook)
}
payload_b   = base64url(payload_json)
sig_b       = base64url(HMAC-SHA256(per_sandbox_secret_v<sv>, payload_b))
token       = payload_b + "~" + sig_b   // round-6 LOW-3: separator `~` (not `.`) to avoid JWT confusion
```

<!-- Round-6 LOW-3: the round-2 spec used `.` to look JWT-ish-but-not-JWT. Tools that auto-detect "looks like a JWT" (logs scrubbers, observability vendors) keep flagging tokens as JWTs and trying to parse them, leading to false-positive PII alerts. Switch to `~`, which is in the unreserved set per RFC 3986 §2.3 and never appears in base64url, but doesn't trigger JWT detection. -->

**Separator (round-6 LOW-3).** The `~` separator is RFC 3986 §2.3 unreserved (no URL escaping needed) and not in base64url's alphabet (which is `A-Za-z0-9_-`). Result: `payload_b ~ sig_b` is unambiguous to split (one `~` only). It also breaks JWT-pattern auto-detection in observability tooling (which looks for `<base64>.<base64>.<base64>`), reducing false-positive secret-scanner alerts.

**`iss` claim (round-6 H7).** The `iss` claim binds the token to the creator who minted it. v7 audit-only: the validator records `iss` in audit logs without rejecting on `iss` mismatch (existing tokens predate this field). v8 (next bump) will gate validation on `iss` corresponding to a creator-still-in-good-standing (account active, not banned, not over quota). This lets us revoke every token a banned creator ever minted with a single status flip.

**Hard size caps** (round-2, addressing C2-12 resource-exhaustion):

- The raw `?t=...` query value is rejected (`400 token too long`) above **1 KiB** *before* base64 decode.
- Decoded `payload_bytes` rejected above **4 KiB**.
- `serde_json::from_slice` configured with `recursion_limit(8)` and `deny_unknown_fields` (forces forward-incompat tokens with novel fields to fail closed).
- Field-by-field hard caps: `aud ≤ 32 ASCII`, `scope ≤ 32 ASCII`, `sbx ≤ 64 ASCII`, all integer fields fit u64.

The `.` separator is JWT-compatible visually but the contents are NOT a JWT (no header section; HMAC alg implicit; we don't want JWT's algorithm-confusion footguns). Validation:

1. Split on `.`. Reject if more than one `.` is present.
2. Decode `payload_b` → JSON, with the size + recursion caps above.
3. Read `sv`; look up `per_sandbox_secret_v<sv>` from the registry. Missing → 401 revoked.
4. Recompute HMAC over `payload_b` (untrusted bytes) with the secret; constant-time-compare against `sig_b`.
5. Validate audience: `aud == "preview"` (constant). Anything else → 401 wrong audience. Forces fail-closed against any future endpoint family that consumes the same HMAC scheme.
6. Validate field types, `iat <= now + 5s` (small skew), `iat < exp`, `exp > now`, `sbx == path-bound sandbox-id`, `port == path-bound port`, `scope` allows the request method.

Field ordering inside `payload_json` does not affect validation (HMAC is over the on-wire bytes which are stable per emitter; we ship a canonicalized emitter to keep audit-log consistency).

<!-- Round-6 Invariant-1 H5: pin the HMAC-input invariant byte-exact. -->

**HMAC-input invariant (round-6 H5).** The HMAC is computed over the **exact base64url string the controller emits** (`payload_b`, ASCII bytes). Validators MUST NOT canonicalize JSON before HMAC verification — the field order, whitespace, and any other emitter quirks are part of the signed bytes by definition. The validator's only job is `hmac_sha256(secret, payload_b.as_bytes())` and a constant-time compare. This is a **wire-format invariant**: any future code change that re-encodes the JSON (even round-trip-stable serde re-encoding) before HMAC breaks every issued token. Pin in the test suite: emit a token with field order `{exp, sbx, port, ...}` AND a token with field order `{sbx, exp, port, ...}`; both verify if and only if the validator hashes the on-wire bytes; both fail if the validator hashes a re-encoded form.

**Per-sandbox secret** (`per_sandbox_secret_v<N>`) is generated at sandbox-create alongside the Ed25519 keypair. Persisted via § II.0 sealed records; the registry holds a small ring of **at most two consecutive versions** during rotation (e.g. when `sv=2` is current, `sv=1` tokens are honoured for a 60 s grace period, then cleared).

**Rotation rate-limit (C2-13).** A sandbox's secret may rotate at most **once per 5 seconds**; further rotation requests within that window 429. A second rotation that does happen (≥ 5 s later but still inside the previous grace window) **immediately ages out the now-two-rotations-old version** (only one prior version is ever honoured). Explicit `DELETE token_id=*` is a **zero-grace** invalidation: the previous version is dropped immediately with no 60 s window. Organic rollover keeps the grace window; explicit revoke does not (closes a "I revoked but old tokens still work for 60 s" surprise).

<!-- Round-6 Invariant-1 CRITICAL-3: cookie validators must run identical secret-version logic. -->
**Cookie-after-revoke invariant (round-6).** Cookies set by `__zsbx_share` carry the same JSON payload (and HMAC) as raw `?t=<token>` requests; the cookie is essentially a transport for the same token. The cookie validator runs **identical logic** to the raw-token validator (see "Validation at the public edge" below) and respects the **same revocation rules**: when explicit `DELETE token_id=*` aged out the previous secret-version, every cookie minted under that version fails HMAC verification on the next request and the user is bounced to a 401. The cookie's lifetime in the browser is irrelevant — the cookie's *validity* is gated by server-side secret-version state.

Phase-3 regression test (added in round 6): mint a share token; convert to cookie via `__zsbx_share`; issue a request with the cookie (200 expected); explicit-DELETE all share tokens; issue another request with the same cookie → 401 with `code: "revoked"`.

**`ti` (token_index)** is a monotonic counter per `(sandbox, secret_version)`. Reserved for per-token revocation; v1 always uses `0` and revocation is whole-secret-bump (see below).

**`scope`** is currently one of:
- `"ro"` — read-only HTTP methods (GET, HEAD, OPTIONS) only.
- `"rw"` — full HTTP method set including WebSocket Upgrade.

`scope` is a stable string field, additive. Future values (`"rw-no-ws"`, etc.) are strings — the JSON encoding means newlines or other separator chars in scope identifiers no longer break the parser.

**Wire status for scope-violation.** The cookie-conversion handler (the first request that carries `?t=…`, before the cookie is set) returns `403 code: "scope_forbidden"` to the creator UI — a helpful diagnostic so the AI-builder can render "this read-only link cannot perform that action; ask the creator for a `rw` token". The dispatch path through `authorize` (every subsequent cookie-bearing request) collapses scope-mismatch into the same `404` uniform response as port-deny + not-owner + sandbox-not-found, so an attacker on the public edge cannot use a 403 vs 404 oracle to detect "this is a real, owned, ro-scoped token". Internally the audit log records `reason=scope-mismatch` so operators can still tell the failure modes apart; the wire stays uniform.

<!-- Round-6 Invariant-2 I10: path-prefix scope deferred to Phase 5; document residual risk. -->

**Path-prefix scope (round-6 I10 — deferred to Phase 5).** A future scope grammar `"ro+/path/prefix"` (e.g., `"ro+/api/public/"`) lets a creator share only a subtree of their app rather than the whole sandbox. v1 ships without it, accepting the residual risk: a `ro` token grants read access to **every** path on the sandbox, including any internal admin UI the user-app exposes. Mitigations:

- The creator is warned at share-mint time ("this token grants read access to every URL on the sandbox").
- AI-builder system prompts instruct the model to avoid binding internal admin UI on the same port as the public-facing service.
- Phase-5 ETA: 1 week of work; ships alongside per-token revocation. The grammar is `scope ::= ("ro" | "rw") ("+" path-prefix)?` where `path-prefix` matches `^/[a-zA-Z0-9._/~-]{0,200}$` (same alphabet as `next=`).

**Audience binding (`aud`).** The `"aud": "preview"` claim is the sole audience accepted by the preview-public-edge validator. If a future feature ever defines `"aud": "shared-logs"` or similar, the preview validator rejects those tokens unconditionally. This is JWT-style audience separation without the JWT format pitfalls; cheap to add now, expensive to retrofit later.

#### Validation at the public edge

```rust
const TOKEN_RAW_MAX:     usize = 1024;            // 1 KiB (hard cap on `?t=` length)
const PAYLOAD_BYTES_MAX: usize = 4096;            // 4 KiB (decoded JSON cap)

fn validate_share_token(
    sandbox_id: Uuid,
    port: u16,
    method: &Method,
    raw_token: &str,
    auth: &SandboxAuth,
) -> Result<TokenClaims, AuthError> {
    // 0. Length precondition (round-2: avoid OOM via giant base64).
    if raw_token.len() > TOKEN_RAW_MAX { return Err(AuthError::Malformed); }

    // 1. Split on '~' (round-6 LOW-3), decode parts. Exactly one '~' allowed.
    let (payload_b, sig_b) = raw_token.split_once('~')
        .ok_or(AuthError::Malformed)?;
    if sig_b.contains('~') { return Err(AuthError::Malformed); }
    let payload_bytes = base64url_decode(payload_b).map_err(|_| AuthError::Malformed)?;
    if payload_bytes.len() > PAYLOAD_BYTES_MAX { return Err(AuthError::Malformed); }
    let sig_bytes     = base64url_decode(sig_b).map_err(|_| AuthError::Malformed)?;
    if sig_bytes.len() != 32 { return Err(AuthError::Malformed); }

    // 2. Parse claims with hardened deserializer.
    //    `deny_unknown_fields` forces fail-closed on unknown forward fields;
    //    `recursion_limit` caps stack growth on adversarial JSON.
    let mut deserializer = serde_json::Deserializer::from_slice(&payload_bytes);
    deserializer.disable_recursion_limit();
    let mut tracker = serde_path_to_error::Track::new();
    let de = serde_path_to_error::Deserializer::new(
        serde_stacker::Deserializer::new(&mut deserializer),
    );
    let claims: TokenClaims = TokenClaims::deserialize(de)
        .map_err(|_| AuthError::Malformed)?;
    if claims.v != 1                { return Err(AuthError::UnsupportedVersion); }
    if claims.aud.as_str() != "preview" { return Err(AuthError::WrongAudience); }
    // round-6 LOW-4: validate-side ceiling on exp (defense-in-depth against a
    // controller bug that mints a token with exp=now+10y).
    const TTL_CEILING_SECS: u64 = 7 * 24 * 60 * 60;     // 1 week
    if claims.exp > unix_now() + TTL_CEILING_SECS {
        return Err(AuthError::TtlTooLong);
    }
    // round-6 H7: record iss in audit (do not gate yet; v7 audit-only).
    audit::record_iss(claims.iss.as_str());

    // 3. Pick secret by `sv`; allow current OR previous-during-grace.
    let secret = auth.secret_for_version(claims.sv)
        .ok_or(AuthError::Revoked)?;

    // 4. Constant-time HMAC compare over the on-wire bytes (`payload_b` ASCII).
    let expected = hmac_sha256(secret, payload_b.as_bytes());
    if !constant_time_eq(&sig_bytes, &expected) {
        return Err(AuthError::BadSig);
    }

    // 5. Bind the token to the request being authorized.
    let now = unix_now();
    if claims.iat > now + 5      { return Err(AuthError::FutureIat); }
    if claims.iat >= claims.exp  { return Err(AuthError::Malformed); }
    if claims.exp <= now         { return Err(AuthError::Expired); }
    if claims.sbx  != sandbox_id { return Err(AuthError::SandboxMismatch); }
    if claims.port != port       { return Err(AuthError::PortMismatch); }
    if !scope_allows(&claims.scope, method) {
        return Err(AuthError::ScopeForbidden);
    }
    Ok(claims)
}
```

**Constant-time comparison** is mandatory for `sig` (timing leaks the secret bit-by-bit otherwise) — implemented via `subtle::ConstantTimeEq` (already a transitive dep via `ed25519-dalek`). Other equality checks (`sbx`, `port`, `aud`) are non-secret identifiers and use normal `==`; the timing channel they expose carries no secret material.

#### Revocation

V1: bump `preview_secret`. All outstanding tokens for that sandbox die at once. Cheap, simple, audit-friendly.

```
DELETE /sandboxes/{id}/preview/{port}/share?token_id=*
→ 200 {revoked: "all"}
```

Per-token revocation is **Phase 5 GA** (round-6 Invariant-2 I4 — promoted from "follow-up" to scheduled work). Two storage choices:
- A revocation set keyed by `token_index` (small in-memory bitset per sandbox; cheap), or
- A token-id table with explicit issue/revoke entries (more state, more queryable).

We pick the bitset approach. Persistence: the bitset rides in the same sealed record as `preview_secret`, capped at 1024 tokens per (sandbox, secret_version) — beyond that, the cap means "rotate the whole secret." API surface: `DELETE /sandboxes/{id}/preview/{port}/share?token_id=<shr_…>` flips bit `<token_index>` for the current `secret_version`. Validation reads the bit on every request.

**Operational pressure (round-6 I4 motivation).** Without per-token revoke, a creator who shared 50 sandboxes' previews can't safely revoke one — the whole-secret-bump kills all 50. The pressure to NOT rotate (for fear of breaking active demos) creates a soft-failure mode where leaked tokens stay live. Phase-5 ETA: 1 week of work, gated on the operability work in § XI.

#### Stateless HMAC vs. stateful token table — tradeoff

| | Stateless HMAC (chosen) | Stateful table |
|---|---|---|
| Issue cost | 1× HMAC | 1 row insert |
| Validate cost | 1× HMAC + memcmp | 1 row select |
| Revoke single token | bump secret = revoke all | UPDATE row |
| Survive controller restart | NO (secret in memory) | YES (if table is durable) |
| Auditing | "creator minted N tokens at time T" — not retrievable | enumerable |
| Storage | O(1) | O(active tokens) |

Combined with non-goal "Persistent share URLs that survive sandbox restarts," stateless is fine. (When the sandbox dies, the secret dies, all tokens die — symmetric.)

### II.5 Lifecycle

#### Creation

- Preview URL is **implicit** on sandbox create. The controller computes `preview-{sandbox_id}-{port}.preview.zeroship.dev` deterministically; no separate API call. The creator's UI just shows the URL once they bind a port.
- Share token requires an explicit `POST .../share` (§ II.4).

#### Probing

- Before showing the preview UI link, the console may call `GET /sandboxes/{id}/preview/{port}/probe` (controller-internal; auth: creator) which signs and forwards a `HEAD /` to the agent and returns:
  ```json
  { "listening": true, "status": 200, "ws_capable": true }
  ```
  This avoids exposing a "loading…" preview iframe before the user's server is up. Small UX nicety; not load-bearing.

#### Teardown

- `DELETE /sandboxes/{id}` — the registry drops the sandbox; subsequent preview requests get 404 from the controller (lookup miss) BEFORE any forward. In-flight HTTP requests on the controller→agent leg complete or fail when the agent goes away. In-flight WebSocket connections receive a `1001 Going Away` close frame from the controller side.
- **No background sweeper for preview URLs.** Their existence is purely a function of the registry; no orphan state to clean.

#### Crash semantics

<!-- Reworked in round 1: the previous draft contained a logical contradiction. The signing key was claimed to be in the registry; it isn't. So a controller restart KILLS BOTH share-token AND creator-mode preview unless we persist (signing_key, preview_secret). -->

- **Sandbox crash (agent unreachable).** Controller's request to the agent fails with `connect refused` or timeout; controller surfaces 502 to the client. The creator's UI auto-retries (Vite WS reconnect, or a `<meta http-equiv="refresh">` on a status page).
- **Sandbox restart vs. crash semantics (round-6 H3 reworked).** When the VM is recycled by the scheduler the sandbox-id may persist (if the scheduler restarts the same job with the same ID) but the agent's verifying key resets. **The `/version` rebind probe is a SIGNED envelope, not a plain GET.** The persisted sealed record carries `expected_pubkey_fp` AND the controller's outbound signing key (the same key used for `/exec`). On controller restart, the rebind sequence is:

  1. Controller reads sealed record → `(expected_pubkey_fp, agent_url, signing_key, preview_secret, sv_current, sv_previous)`. Agent URL is recomputed from `vm_index` (round-6 I3: not sealed; deterministic from index).
  2. Controller signs a `GET /version` request with `signing_key` (full v1 canonical: `(method, path, ts, nonce, sha256_hex(""))`).
  3. Agent verifies the signature using its in-VM verifying key (the agent's verifying key never changes during a single VM lifetime; it changes on VM recreate).
  4. Agent responds with the standard `/version` JSON: `{ "pubkey_fingerprint": "<fp>", "capabilities": [...], "version": "..." }`. The agent ALSO signs the response body using its **signing** key — i.e., the agent has its own Ed25519 keypair for outbound signing, whose verifying key the controller knows from sandbox-create (this is the same `pubkey_fingerprint` we're checking). The response carries `X-Sbx-Resp-Signature` over the response body.
  5. Controller verifies: (a) the response signature decodes under the persisted `expected_pubkey_fp`'s implied verifying key (the controller stored the verifying key, not just the fingerprint), AND (b) the JSON body's `pubkey_fingerprint` field byte-matches the persisted `expected_pubkey_fp`. **Both must hold** for rebind.
  6. On mismatch (signature fails OR fingerprint fails): rotate (delete sealed record, mark sandbox as "recreating", surface 410 Gone to in-flight clients). On match: rebind succeeds; controller resumes serving preview traffic for that sandbox.

  Why both checks are needed: the fingerprint check alone defends against a fresh VM coming up with a new key (it would have a different fingerprint). The signature check alone defends against an attacker on the controller→agent leg replaying an old signed `/version` body. Together they defeat both: a replayed response from a recycled VM has the right signature on a body with the wrong fingerprint; a fresh VM's response has the right fingerprint but a signature that doesn't decode under the persisted key.

  - **`pubkey_fp` definition** (round-2, C2-26; refined round-6): `lowercase_hex(sha256(verifying_key_bytes)[..16])` — a 128-bit truncation of SHA-256 over the 32-byte Ed25519 public key. The persisted record stores BOTH the fingerprint AND the full 32-byte verifying key (`expected_verifying_key`); the fingerprint is for fast equality checks, the verifying key is for actually verifying signatures during rebind. Sufficient against accidental collision; the security-critical comparison is the signature decode, not the fingerprint. Documented in `crates/sandbox-agent/src/version.rs`.
  - **`sandbox_id` reuse impossibility (round-6 I8).** Each VM gets a freshly generated UUIDv7 with new entropy at create time. Reuse is impossible — the timestamp prefix monotonically advances and the random-bits portion is fresh per call. The sealed-record file naming (`hex(sha256(sandbox_id))[..32]`) is therefore safe against any conceivable collision; an attacker cannot precompute a sealed-record path before the controller mints the sandbox. Documented in `crates/core/src/typed_id.rs`.
- **Controller crash.** With the § II.0 sealed-record persistence, on restart the controller:
  1. Rehydrates `(agent_url, signing_key, preview_secret, secret_version)` from the on-disk records.
  2. Validates each record by calling `GET /version` against the agent and matching `pubkey_fp` → only records that match are restored; mismatches are deleted (the sandbox was recycled).
  3. The existing `cleanup_orphans_at_startup` continues to delete VMs that have no sealed record at all.
  4. **Both creator-mode AND share-token access keep working** post-restart for sandboxes whose VMs survived. The 5-second skew window means in-flight requests during the controller restart will fail (they were already going to fail — the controller was down) but new requests issued after restart succeed.
- **Controller crash with `restore_auth_records_at_startup` failure** (corrupt sealed file, AEAD key mismatch, etc.) — that single sandbox's preview is forever lost; the controller logs an error and proceeds. The creator's UI shows "preview unavailable; restart sandbox". This is a graceful-degradation envelope, not silent data loss.

#### Drain orchestration (controller restart / agent shutdown)

<!-- Added in round 3 (C3-10): the v3 doc said "drain returns 503" but didn't sequence the LB hand-off. -->

A planned controller restart (deploy, scale-down) follows a documented five-phase drain:

| Phase | Action | Duration | Observable |
|---|---|---|---|
| 1 | Flip readiness probe to NOT_READY (`/readyz` returns 503). | t = 0 | LB stops sending NEW connections. |
| 2 | Wait for the LB's withdrawal to complete (LB health-check interval). | t = 0 → 10 s | No new requests arrive. |
| 3 | For every active WebSocket: send `Close 1001` with `reason: "controller-drain"`; flush. Hold the controller-side socket open for 5 s to let the client receive + reconnect to a peer controller. | t = 10 → 15 s | Clients reconnect; HMR resumes (post-reconnect race tolerated by Vite). |
| 4 | Drain in-flight HTTP requests (max wait 30 s; force-close on timeout). Stop accepting on the listening socket. | t = 15 → 45 s | All bytes flushed or aborted. |
| 5 | Exit. | t = 45 s | Process terminates. |

Same five phases for agent shutdown (`/shutdown` flips the agent's drain flag at phase 1). The agent doesn't have an LB so phase 2 is a 2-second fixed wait for in-flight signed requests to settle.

`SANDBOX_PREVIEW_DRAIN_TIMEOUT_SECS` (default 45) caps the total drain window; force-kill after.

A controller fired at by SIGKILL skips the entire sequence — open WebSockets see RST, clients see TLS abort. Documented as "ungraceful exit; reserved for runaway processes only."

Metrics: `proxy_drain_active` (0/1 gauge), `proxy_drain_aborted_ws_total{reason}`, `proxy_drain_phase` (gauge 0–5).

### II.6 Cross-origin policy (CORS, Sec-Fetch, CSP)

<!-- Added in round 1: addressing Missing A (CORS preflight) and Missing B (Sec-Fetch-Site / cookie-conversion CSRF). -->

**At the preview origin (`*.preview.zeroship.dev`)** the controller's edge enforces:

#### CORS

- `OPTIONS` preflights with `Origin: https://console.zeroship.ai` (or the configured per-region console origins) get:
  ```
  Access-Control-Allow-Origin: https://console.zeroship.ai
  Access-Control-Allow-Credentials: true
  Access-Control-Allow-Methods: GET, POST, PUT, DELETE, HEAD, OPTIONS
  Access-Control-Allow-Headers: Authorization, Content-Type, X-Requested-With
  Access-Control-Max-Age: 600
  Vary: Cookie, Origin
  ```
- Other origins get `Access-Control-Allow-Origin: null` (a deliberate denial — null Origin can't carry credentials cross-site).
- The user's app's own response headers (CORS or otherwise) pass through unchanged for non-preflight requests; we only intercept OPTIONS that hit the controller's auth-protected internal `/sandboxes/.../preview/...` paths.

<!-- Round-2 (C2-30): added Vary: Cookie so intermediate caches don't serve creator-authenticated responses to anonymous users. -->

#### Cookie-conversion (Sec-Fetch-* enforcement)

`__zsbx_share` and `__zsbx_login` accept ONLY top-level navigations:

- Reject if `Sec-Fetch-Mode != "navigate"` or `Sec-Fetch-Dest != "document"`.
- Reject if `Sec-Fetch-Site == "cross-site"` AND not a top-level navigation. (Top-level cross-site navigations carry no Origin and `Sec-Fetch-User: ?1` if user-initiated; we accept those.)
- For POSTs (rare; the GET form is canonical), additionally require `Origin == https://<request-host>` (i.e., self-Origin).
- All responses to these handlers include `Cache-Control: no-store` and the appropriate `Set-Cookie` flags. <!-- Round-6 Invariant-1 CRITICAL-1: SameSite is per-cookie, not blanket. -->
  - **`__Host-zsbx_preview_<sbx>` (login cookie) — `SameSite=Lax`.** The login flow is a top-level POST navigation from `console.zeroship.ai` (a different origin); `Strict` would drop the cookie on the cross-site → same-site redirect. Lax is the minimum value that lets top-level navigations carry it.
  - **`__Host-zsbx_share_<sbx>` (share cookie) — `SameSite=Strict`.** No cross-site context ever needs to read the share cookie; the share-link first-hit relies on the `?t=` query param (same-site after the conversion redirect). Strict closes the residual cross-site CSRF surface.
  - Both cookies always carry `HttpOnly; Secure; Path=/`; no `Domain` attribute (host-only via the `__Host-` prefix invariant).

#### Sec-Fetch-* fallback policy (round-6 Invariant-1 H6)

<!-- Round-6 Invariant-1 H6: older browsers omit Sec-Fetch-*; we previously failed open. Now we fail closed and document the UX impact. -->

If a request to `__zsbx_login` or `__zsbx_share` arrives WITHOUT any of `Sec-Fetch-Site`, `Sec-Fetch-Mode`, `Sec-Fetch-Dest` (i.e., the client is too old to ship Fetch Metadata — pre-Chrome-76 / pre-Firefox-90 / pre-Safari-16.4), the handler **fails closed** with:

```
HTTP/1.1 400 Bad Request
Content-Type: application/json
{ "error": "client too old; Fetch Metadata required for cookie-conversion",
  "code": "client_too_old",
  "min_browser_versions": { "chrome": 76, "firefox": 90, "safari": 16.4, "edge": 79 } }
```

UX impact: a vanishingly small fraction of creators will hit this. The error has a `code` the AI builder UI can render specifically: "Your browser is too old for the security checks; please update."

**Fallback alternative considered.** We considered sniffing User-Agent for known-recent versions and falling back to an Origin-header check. Rejected: UA-string-based decisions are exactly the kind of inconsistency that creates parser-differential bugs across the platform. Failing closed is the simpler, auditable rule. If creator complaints accumulate, we revisit with a UA-allowlist *plus* Origin-header check for that allowlist only — never UA-sniff to relax security-relevant rules.

**Origin-header backup.** Even with Sec-Fetch-* present, POSTs to `__zsbx_login` STILL require `Origin: https://console.zeroship.ai` (or a configured per-region console origin); a request that ships Sec-Fetch-* but no Origin is rejected (`code: "missing_origin"`).

#### CSP at the preview origin

- `Content-Security-Policy: frame-ancestors 'self' https://console.zeroship.ai;` — preview pages may be iframed by the console, but not by random origins (mitigates clickjacking + cookie-conversion CSRF). This is the **single source of truth** for `frame-ancestors` in the design — earlier risk-table prose mentioning `'none'` (R-4 / R-13 in v2) was an inconsistency corrected in v3.
- `Referrer-Policy: no-referrer` — prevents the share-token URL leaking via Referer to third-party origins linked from the user's app.
- `X-Robots-Tag: noindex, nofollow, noarchive, nosnippet` (R-19).
- `Cross-Origin-Opener-Policy: same-origin` — isolates each preview from window.opener leakage.
- For the cookie-conversion handler responses specifically (`__zsbx_login`, `__zsbx_share`): additionally `Content-Security-Policy: default-src 'none'; frame-ancestors 'none'` (the response is a tiny redirect with no body — locking it down further costs nothing).
- `Vary: Cookie, Origin` on every authenticated response so caches don't bleed across creator/anonymous.

### II.7 Idle-GC interaction with long-lived previews

<!-- Added in round 1: addressing Missing H — registry's idle GC reaps by last_used updated on /exec, /files; long-lived HMR WS holds the socket for hours without touching last_used. -->

The registry's `start_idle_gc` (registry.rs:163) reaps sandboxes by `(now - last_used) > idle_timeout_secs`. The proxy code path:

- Touches `last_used` on every `proxy.http` request (one bump per HTTP request through the proxy).
- For long-lived WebSocket connections, emits a `last_used` touch every 60 s while the WS is open; `proxy.ws.close` records the duration.

This means a creator with an HMR-connected browser tab keeps the sandbox alive at one touch / minute regardless of whether the creator types code. To prevent runaway "open my preview, walk away for a week" cases:

- A separate `max_lifetime_secs` cap (already in the registry, default 8 h) applies regardless of activity.
- Creators see a "preview will expire in N minutes; click to extend" UI hint in the last 30 minutes of the lifetime.
- The cap is operator-tunable (`SANDBOX_MAX_LIFETIME_SECS`).

### II.8 Failure-mode summary

| Origin | Symptom | Status |
|---|---|---|
| Bad share token | tampered / expired / replayed | 401 |
| Wrong scope (dispatch path) | RO token + POST through `__Host-zsbx_share_*` cookie | 404 uniform on the public-edge / internal-route paths (port-deny + not-owner + scope-mismatch + auth-failure all collapse to one wire response; audit log records `scope-mismatch`) |
| Wrong scope (cookie-conversion path) | RO token + POST through `?t=<token>` first hit | 403 `code: "scope_forbidden"` — the conversion handler returns the friendlier 403 to help the AI-builder UI explain to a creator that they need a `rw` token. This path is only reachable on the very first request that carries `?t=`; subsequent cookie-bearing requests run through the dispatch path and see the uniform 404 above. |
| Sandbox not found (post-auth) | bad sandbox-id in subdomain | 404 (uniform; coalesced with auth-fail / not-owner / port-deny / scope-mismatch — see § II.2) |
| Port not in allow-set (controller) | `preview-foo-22.preview…` | 404 (uniform with the other authorize-layer reasons; oracle-safe) |
| Port not in allow-set (agent) | direct `/proxy/22/...` to agent | 400 (defense-in-depth at the agent; no public oracle concern — only controller is publicly addressable) |
| Body > 100 MiB | huge upload | 413 |
| Agent unreachable | sandbox crashed, slow | 502 |
| Upstream port not bound | user crashed dev server | 503 |
| Upstream slow | dev server hung | 504 |
| Agent draining | sandbox is shutting down | 503 |
| Controller draining | controller is shutting down | 503 |
| Host-header mismatch | non-canonical Host (DNS rebind attempt) | 421 |
| Circuit breaker open | sandbox unhealthy, breaker tripped | 503 (`code: "circuit_open"`) |
| Token format invalid | malformed token, oversize, unknown audience | 401 (`code: "malformed" \| "wrong_audience"`) |
| Per-IP / per-sandbox / per-creator quota | abuse / DDoS | 429 (`code: "rate_limited"`) |

Full machine-readable error code surface in § XI.4.

### II.9 AI builder integration

<!-- Added in round 5 (C5-4): consolidate scattered references into one section. -->

How the AI builder uses preview URLs end-to-end:

1. **Sandbox boot — env-var threat model (round-6 Invariant-2 CRITICAL-5).** The user-app inside the VM is **hostile** for security purposes — a creator's app may be wholly under attacker control (creator account compromise; supply-chain attack on a creator's npm dep; deliberately malicious creator). The env vars the platform injects MUST be the minimum set the dev tooling actually needs, and MUST NOT carry creator-identifying or fleet-identifying state.

   **Allowed env vars (whitelist):**

   | Var | Value | Why |
   |---|---|---|
   | `ZEROSHIP_PREVIEW_MODE` | `console` \| `public` | Vite plugin needs to pick the right `clientPort`/`host`. |
   | `ZEROSHIP_PREVIEW_HOST_PUBLIC` | `preview-{slug}-{port}.preview.zeroship.dev` for THIS sandbox + port only | Vite plugin's `hmr.host` for public mode. |
   | `ZEROSHIP_PREVIEW_HOST_CONSOLE` | `console.zeroship.ai` | Vite plugin's `hmr.host` for console mode. Single value, no per-creator info. |
   | `ZEROSHIP_PREVIEW_PATH_CONSOLE` | `/sandboxes/{sandbox_id}/preview/{port}` for THIS sandbox + port only | Vite plugin's `hmr.path`. The sandbox-id IS in the path; the user-app's process can read its own sandbox-id by other means anyway (e.g., systemd unit name). Not a leak. |
   | `ZEROSHIP_SANDBOX_ID` | this sandbox's typed-id | The user-app already runs inside this sandbox; not a leak. |

   **Explicitly NOT exposed:**
   - **Creator typed-id (`usr_…`).** The user-app must NOT learn who the creator is — a malicious app could use this to fingerprint sandboxes across creators.
   - **List of the creator's other sandbox IDs.** A malicious app must not be able to enumerate the creator's fleet.
   - **The console's API base URL beyond the path-prefix the Vite plugin needs.** No `https://console.zeroship.ai/api/...` URLs that would let the user-app POST to console internals (which would be CORS-blocked anyway, but defense-in-depth).
   - **Any signing key, AEAD key, or token.** Tokens are minted by the controller and reach the browser via the cookie-conversion flow; the user-app never sees them.
   - **The controller's API endpoint.** The user-app must not have an outbound path to the controller's preview-mint API.
   - **The host's `vm_index`.** The mapping from `sandbox_id` to `vm_index` is operator-internal.

   **Implementation point.** Env vars are filtered by the controller in `crates/sandbox/src/backend/{docker,k8s,nomad_ch}.rs::spawn_user_proc`; the filter is whitelist-only, hardcoded. A regression test asserts that `printenv` from inside a fresh sandbox prints exactly the whitelisted vars and nothing else (modulo PATH and friends). The controller's own env vars (which may include AEAD key paths, DB connection strings) are never inherited; the spawn helper passes a fresh `envp` array.

2. **Creator iterating in the AI builder.** The creator hits the **console preview** at `https://console.zeroship.ai/sandboxes/{id}/preview/{port}/...`. This goes through the console UI's iframe; the controller's controller-internal route handles it. Cookie auth is the creator's existing `console.zeroship.ai` session.

3. **Vite plugin (`@zeroship/vite-preview`)** auto-detects which mode is active (via the env vars above) and configures `server.allowedHosts`, `server.hmr.host`, `server.hmr.protocol`, `server.hmr.path`, `server.hmr.clientPort` accordingly. The creator never edits Vite config by hand. Plugin source: `sdks/vite-preview/` (one file, ~80 lines).

4. **AI builder UI error surface.** On 4xx/5xx from preview, the UI reads the JSON error body's `code` field (§ XI.4) and renders a specific user-facing message + suggested action. Top-5 codes (`agent_unreachable`, `port_not_listening`, `circuit_open`, `rate_limited`, `expired`) are required for Phase-5 GA; remaining codes can show a generic "Preview unavailable" with the code in fine print.

5. **Share button.** The AI builder's "Share" button POSTs to the controller's mint-share endpoint (default scope `ro`, TTL 1 h) and shows the resulting URL with a "copy" affordance. UX detail: the share URL contains the token in the query string; the share button warns "anyone with this URL can view your preview for 1 hour."

6. **Make public flow.** Today's preview URLs are creator-authenticated by default (`__Host-zsbx_preview_<sbx>` cookie). Some creators want a no-auth public URL during Twitter / Show HN demos. The "Make public" toggle in the AI builder mints a long-lived `rw` token with TTL = sandbox lifetime; the controller flips into `ZEROSHIP_PREVIEW_MODE=public` (Vite plugin re-applies for the public host).

7. **AI system-prompt edits** (`docs/research/ai-builder-features.md` will reference these): the model is instructed to (a) bind dev servers on `0.0.0.0`, (b) NOT pass `--inspect`/`--inspect-brk` (port 9229 is denied; the controller's response on a denied port is the same uniform 404 it returns for unauthenticated / not-owned / unknown sandbox — failing fast in the prompt avoids a creator-confusing "404 not found" with no obvious cause), (c) use `@zeroship/vite-preview` if Vite is the framework, (d) avoid hard-coding ports below 1024 and avoid the deny-listed service ports.

8. **Console iframe sandbox attributes (round-6 Invariant-2 I5).** The console wraps preview iframes with `sandbox="allow-scripts allow-forms allow-same-origin"` — **`allow-popups` removed**. Rationale: a hostile user-app inside the iframe with `allow-popups` could open a popup at any URL; combined with `allow-same-origin` this gives the popup full access to the preview origin's cookies. v1 builder UX has no requirement for popups (the AI-builder dispatch is via the console UI's chrome, not the user-app). If a creator app later legitimately needs popups (e.g., OAuth dialogs to a third party), the upgrade is `allow-popups-to-escape-sandbox` paired with origin-locking via `target=`; gated on a per-creator opt-in, NOT a default. Together with the preview origin's CSP `frame-ancestors 'self' https://console.zeroship.ai`, the iframe is bidirectionally constrained. Tracked as a UX follow-up (Q-18).

9. **Console → preview JS bridge.** A small `postMessage` API lets the console interrogate the preview ("Is your dev server up? What's your current route?") without the preview making cross-origin fetches itself. **Protocol spec is § II.9.x (round-6 Invariant-2 CRITICAL-1).**

### II.9.x postMessage bridge protocol

<!-- Round-6 Invariant-2 CRITICAL-1: previously "not part of this spec" — but the bridge is the natural conduit for sandbox-A → sandbox-B information leakage if mis-implemented. Spec it. -->

The console UI iframes preview origins side-by-side (one per port the creator has bound). A naïve `window.postMessage` listener that accepts any incoming message would let any sandbox post to the console with claimed `sandbox_id`/`port` and either spoof state or trick the console into routing a follow-up action to the wrong sandbox. The protocol below closes that gap.

#### Console-side state

```ts
// In the console UI's frame manager.
type IframeRef = HTMLIFrameElement;

const iframeOrigins: Map<IframeRef, string> = new Map();
// Populated when the console creates an iframe:
const slug = lowercaseTypedIdToSlug(sandboxId);    // sbx_… → sbx-…
const expectedOrigin = `https://preview-${slug}-${port}.preview.zeroship.dev`;
iframeOrigins.set(iframeEl, expectedOrigin);
```

Every iframe has a single, immutable expected origin recorded at creation time. The map is keyed on the DOM element reference — stable for the lifetime of the iframe; not on a string the message could spoof.

#### Message envelope

```ts
type BridgeMessage = {
  v: 1;                              // protocol version
  sandbox_id: string;                // typed-id of the sender; informational only
  port: number;
  kind: "ready" | "route_changed" | "log" | "error" | "ack";
  payload: unknown;                  // schema is per-kind; console validates
  nonce: string;                     // 128-bit base64url; included in any reply's `payload.in_reply_to`
};
```

`kind` is a closed enum; any unknown value is dropped. `payload` is parsed per-kind by a JSON schema; rejection emits `audit.kind = "bridge.bad_payload"` and drops.

#### Console-side receive handler

```ts
window.addEventListener("message", (event: MessageEvent) => {
  // 1. Find the iframe element that sourced this message.
  const iframe = findIframeBySource(event.source);    // checks iframeEl.contentWindow === event.source
  if (!iframe) {
    auditDrop("bridge.unknown_source", { origin: event.origin });
    return;
  }

  // 2. Validate origin EXACTLY against the per-iframe expected origin.
  const expected = iframeOrigins.get(iframe);
  if (event.origin !== expected) {
    auditDrop("bridge.origin_mismatch", { origin: event.origin, expected });
    return;
  }

  // 3. Validate envelope shape.
  if (!isBridgeMessage(event.data)) {
    auditDrop("bridge.bad_envelope", { origin: event.origin });
    return;
  }

  // 4. Validate sandbox_id/port match the iframe's expected origin.
  //    (defense-in-depth: origin already binds these, but a console-side bug
  //    could otherwise route the action by sandbox_id rather than iframe ref.)
  const slug = lowercaseTypedIdToSlug(event.data.sandbox_id);
  if (expected !== `https://preview-${slug}-${event.data.port}.preview.zeroship.dev`) {
    auditDrop("bridge.id_origin_skew", { origin: event.origin, claimed: event.data });
    return;
  }

  // 5. Audit-log the accepted message (round-6 Invariant-2 I14).
  auditAccept("bridge.accept", {
    sandbox_id: event.data.sandbox_id,
    port:       event.data.port,
    kind:       event.data.kind,
    origin_validated: true,
  });

  // 6. Dispatch.
  handleBridgeMessage(iframe, event.data);
});
```

**Console NEVER acts on a postMessage without first verifying `event.origin === iframeOrigins.get(iframe)`.** Reviewers: any code in `apps/zeroship-builder/src/client/builder/` that reads `event.data` without first running this validation pipeline is a regression.

#### Console-side send

The console only sends **to a specific iframe** via `iframe.contentWindow.postMessage(msg, expectedOrigin)` — using the SECOND argument as a targetOrigin filter. A typo that passes `"*"` is rejected at code review (lint rule + CI grep).

#### Sandbox-side (preview-injected JS, optional)

The platform optionally injects a tiny shim into the preview origin (via the Vite plugin) that registers a postMessage listener and replies with `kind: "ready"` etc. when the user-app's HMR client connects. The shim is the platform's code, not the user-app's; it's not addressable from user code. **The user-app itself is hostile** (§ II.9 env-var threat model below) and may post arbitrary messages to its parent window — the console's origin check ensures these reach the console as the *sender's* origin, where they're routed to the correct iframe based on the iframe-element-ref binding.

#### Audit log (round-6 I14)

Every postMessage event the console processes — accepted, dropped, malformed — is sent to the controller's audit pipe via a periodic batch POST. Schema:

```json
{
  "ts": 1746139200,
  "kind": "bridge.accept" | "bridge.origin_mismatch" | "bridge.bad_envelope" | ...,
  "iframe_sandbox_id": "sbx_01HF…",      // from iframeOrigins map (trusted)
  "claimed_sandbox_id": "sbx_01HX…",     // from event.data; may differ on attack
  "claimed_port": 5173,
  "actual_origin": "https://attacker.example",
  "expected_origin": "https://preview-sbx-…-5173.preview.zeroship.dev",
  "decision": "accept" | "drop"
}
```

The controller's audit pipe alerts on a sustained `bridge.origin_mismatch > 0` rate (likely indicates a content-script or browser-extension attempting cross-iframe injection, or a console UI bug).

---

## III. API specification

All routes JSON unless noted. Errors use the structured shape:

```json
{ "error": "<reason>", "code": "<machine-readable>", "sandbox_id": "<uuid>", "port": 5173 }
```

### POST /sandboxes/{id}/preview/{port}/share

Mint a share token.

```
POST /sandboxes/sbx_01HF…/preview/5173/share
Authorization: Bearer <creator-session>
Content-Type: application/json
{
  "expires_in_secs": 3600,
  "scope": "ro"
}
```

**Response 200:**
```json
{
  "token_id": "shr_<22-char-base64url>",
  "share_url": "https://preview-sbx_01HF…-5173.preview.zeroship.dev/__zsbx_share?t=<base64url>",
  "expires_at_unix": 1746142800,
  "scope": "ro"
}
```

`token_id` is `shr_` + the raw 22-char base64url `tid` claim. Wire-stable; presentation-only prefix (the audit table stores the raw `tid`).

**Errors:**
- `400` — `expires_in_secs` < 60 or > 604800 (1 week max).
- `400` — unknown `scope`.
- `404` — sandbox not in registry.
- `403` — caller is not the sandbox owner.

### GET /sandboxes/{id}/preview/{port}/share

<!-- Reworked in round 1: addressing MAJOR #12 — the previous draft said tokens are stateless ("never stored") yet listed them. We now persist token METADATA (not token bytes) in the audit table. -->

List the issue-audit metadata for this (sandbox, port). The token bytes themselves are NOT retrievable — they live only in the holder's URL. The metadata table is a side-effect of `POST .../share` (one row per mint) and lets the creator see "I've issued 4 tokens; the one expiring at 14:00 is from yesterday".

```
GET /sandboxes/sbx_01HF…/preview/5173/share
Authorization: Bearer <creator-session>
```

**Response 200:**
```json
{
  "tokens": [
    { "token_id": "shr_<22-char-base64url>", "issued_at_unix": 1746139200,
      "expires_at_unix": 1746142800, "scope": "ro",
      "secret_version": 1, "token_index": 0,
      "last_used_at_unix": 1746140100, "use_count": 17 }
  ],
  "secret_version_current": 1
}
```

`secret_version_current` is the rotation counter; bumping it invalidates every token issued at a previous version.

**Storage cost.** A 64-byte audit row × N tokens × per-sandbox. Bounded by per-sandbox token-mint rate-limit (see § VI R-14 → 100 tokens / sandbox / day default). Memory cost is trivial; persistence shares the sealed-record file from § II.0.

### DELETE /sandboxes/{id}/preview/{port}/share?token_id=...

Revoke. v1 supports `token_id=*` (rotate secret = nuke all). Per-token revoke is § VII.

```
DELETE /sandboxes/sbx_01HF…/preview/5173/share?token_id=*
Authorization: Bearer <creator-session>
```

**Response 200:**
```json
{
  "revoked": "all",
  "secret_version_current": 2,
  "grace": "none",
  "note": "explicit DELETE is zero-grace; all tokens issued at sv<2 are now invalid."
}
```

<!-- Round-1 fix: MINOR #24 — consistent snake_case fields. -->
<!-- Round-2 fix: C2-19 — explicit DELETE is now documented as zero-grace (no 60s window for the rotated-out version); organic rotation keeps the grace, manual revoke does not. -->

### GET /sandboxes/{id}/preview/{port}/probe

Status probe. Issues `HEAD /` to the agent.

```
GET /sandboxes/sbx_01HF…/preview/5173/probe
Authorization: Bearer <creator-session>
```

**Response 200:**
```json
{
  "listening": true,
  "status": 200,
  "ws_capable": true,
  "headers": { "server": "vite/4.5.0", "x-powered-by": null }
}
```

**Errors:**
- `502` — port not listening (`{ "listening": false, "error": "ECONNREFUSED" }`)
- `504` — agent timeout

### Forwarding endpoints (transparent pass-through)

Internal: `ANY /sandboxes/{id}/preview/{port}/{path*}` (auth-gated, console-side use)
Public:   `ANY https://preview-{id}-{port}.preview.zeroship.dev/{path*}` (cookie or share-token auth)

These are not JSON; bytes pass through. Errors at the controller layer use the JSON error shape; errors from the user's app are passed through unchanged (whatever Vite/Node returned).

**Public-edge auth contract** (round-5 C5-13). The public `*.preview.zeroship.dev` route accepts any of:

- A `Cookie: __Host-zsbx_preview_<sbx>=<jwt>` set via the creator login flow (§ II.3 POST handshake).
- A `Cookie: __Host-zsbx_share_<sbx>=<jwt>` set via the share-token cookie-conversion (§ II.4).
- A first-hit `?t=<token>` query parameter on `/__zsbx_share` (cookie-conversion endpoint), which then 303-redirects to a sanitized `next` and the cookie carries subsequent requests.
- A first-hit POST to `/__zsbx_login` with the creator's short-lived JWT in the form body (sets the creator cookie).

<!-- Round-6 Invariant-1 H4: 401-vs-404 oracle on the public edge. Anonymous requests must NOT differentiate between "sandbox exists" and "sandbox does not exist". -->

**Anonymous response invariant (round-6 H4 — public edge).** Anonymous requests to the public preview origin (no cookie, no `?t=`) MUST receive the SAME 401 response regardless of whether the host's slug corresponds to an existing sandbox, an expired sandbox, a sandbox owned by a different creator, or a wholly fabricated slug. The `login_url` is **invariant** with respect to sandbox existence: it always points at `https://console.zeroship.ai/login?next=<encoded-original-url>`; the encoded original URL leaks no more than the URL the client already had. There is no path that lets an anonymous client probe sandbox existence by triggering a different status, body, or `login_url`.

```http
HTTP/1.1 401 Unauthorized
Cache-Control: no-store
Vary: Cookie, Origin
Content-Type: application/json
{ "error": "authentication required",
  "code": "unauthorized",
  "login_url": "https://console.zeroship.ai/login?next=https%3A%2F%2Fpreview-{slug}-{port}.preview.zeroship.dev%2F" }
```

<!-- Round-6 LOW-1: Vary on 401 anonymous-fallback. -->

Behind this 401, the controller still runs the sandbox lookup AND audit-logs the actual reason (`sandbox-not-found | sandbox-suspended | not-owner | stale-cookie | etc.`); the wire response is invariant. The audit log is the place to triage abuse, NOT the wire surface.

The `login_url` lets the creator flow through `console.zeroship.ai` and back via the POST handshake. Some operators may configure "anonymous-by-default" for a sandbox (Q-2); in that mode, the route accepts unauthenticated requests provided the sandbox's `preview_anon_allowed` flag is set — but anonymous-by-default still goes through the same 401-or-200 dispatch, never leaks "exists/doesn't-exist".

### Capabilities (`GET /version`)

Two new capability strings, added to `crates/sandbox-agent/src/version.rs::CAPABILITIES`:

```
"proxy.http-v1"   // ANY /proxy/{port}/{path*} for HTTP methods
"proxy.ws-v1"     // WebSocket Upgrade through /proxy
```

Controllers feature-detect at session-create. An agent without `proxy.http-v1` causes the controller's preview endpoints to return:

```
501 Not Implemented
{ "error": "agent does not support preview proxy",
  "code": "agent_capability_missing",
  "missing": ["proxy.http-v1"] }
```

---

## IV. Implementation phases

Each phase is independently shippable, end-to-end testable, and rollback-safe.

### Phase 0: Registry lift + sealed-record persistence + Docker-agent path (~14h)

<!-- Added in round 1: addressing CRITICAL #1 + #3 — the previous phase plan assumed the registry already had agent_url + signing_key, which it doesn't. -->

**Scope:**
- `crates/sandbox/src/registry.rs` + `backend/mod.rs`: add `SandboxAuth` struct (Arc'd), `Backend::session_auth(...)` method, `SandboxRegistry::get_auth(...)` lookup.
- `crates/sandbox/src/backend/nomad_ch.rs`: route the existing `signing_key` through `session_auth`; no behavior change.
- `crates/sandbox/src/backend/docker.rs`: introduce the agent-launch path (mount agent binary into the container, configure `auth.ed25519-v1`, mint per-container keypair).
- `crates/sandbox/src/backend/k8s.rs`: switch from `Bearer` token to Ed25519 (mount controller pubkey via ConfigMap, retire the token mount). Coordinated wire change; gated by capability.
- `crates/sandbox/src/persist.rs` (new): sealed-record codec (XChaCha20-Poly1305 over a controller-wide key sourced from env or KMS); `restore_auth_records_at_startup` boot-path.
- Tests: round-trip seal/unseal; controller-restart e2e (kill controller, restart, hit existing sandbox via signed RPC).

**Test plan:**
- Unit: seal + unseal round-trip; tampered ciphertext → fail; wrong key → fail.
- Integration: bring up controller + nomad-ch sandbox, kill controller pid, restart, verify signed `/exec` succeeds.
- Negative: mid-restart sandbox-recycle → controller detects pubkey_fp mismatch and deletes record.

**Rollback:** the lift is additive on top of the existing path; if `session_auth` returns the same key it always did, no behavior change. The persistence layer is feature-flagged (`SANDBOX_PERSIST_AUTH=1`) so we can disable it if AEAD key management is not in place yet.

### Phase 1: HTTP-only agent proxy + creator-authed forwarder (~10h)

**Scope:**
- New file: `crates/sandbox-agent/src/proxy.rs` — `proxy_http()` handler, port allow-list, body cap.
- Wire into `crates/sandbox-agent/src/handlers.rs` route registration.
- `version.rs::CAPABILITIES` += `"proxy.http-v1"`.
- New file: `crates/sandbox/src/preview.rs` — `preview_proxy()` handler, agent-side signing wrapper.
- Wire into `crates/sandbox/src/lib.rs` route registration.
- Console-side bearer-token auth only (share tokens are phase 3).
- No WebSocket support (phase 2).
- No public DNS yet — only `console.zeroship.ai/sandboxes/{id}/preview/{port}/{path*}` (auth: creator session).

**Test plan:**
- Unit: `proxy.rs` handler handles 200 / 503 / 504 / 413 cases against a fixture upstream.
- Integration (`tests/sandbox_e2e.rs`): boot a fixture HTTP server inside the workspace via `/exec`, hit it via the controller-side preview endpoint, assert response bytes match.
- Auth: `unsigned-request → 401`, `tampered-body → 401` (already covered by the agent's existing test scaffold; add a `#[ntex::test]` for the new path).
- Body-cap: `PUT 101 MiB → 413`.
- **Round-6 additions:**
  - **CRITICAL-4 (Invariant-1) dispatcher byte-equality:** signed `GET /proxy/5173/%2e%2e%2fexec` (with v1.1 canonical hashing the percent-encoded path) MUST land on the proxy handler, MUST NEVER reach `/exec`. Negative: a v1 canonical (no domain-separator) sent to `/proxy/...` → 401.
  - **CRITICAL-4 (Invariant-2) iptables FORWARD-DROP:** boot two sandboxes A=100, B=101; from VM-A `curl --max-time 2 http://10.99.<B>.2:7777/livez` MUST timeout. From VM-A `curl --max-time 2 http://169.254.169.254/latest/meta-data/` MUST timeout. From VM-A to controller management IP MUST be DROPped or refused. From host: `curl http://10.99.<idx>.2:7777/livez` succeeds.
  - **CRITICAL-3 (Invariant-2) sealed-record path traversal:** `seal(sandbox_id="../../etc/passwd", ...)` writes `sealed-records/<hex>.sealed`, NOT a file outside the sealed-records dir.
  - **I7 (Invariant-2) virtio-fs symlink containment:** VM-A creates symlink in workspace pointing to `/run/keys/...`; host-side workspace-share root resolution MUST NOT escape.
  - **Port-deny — controller (oracle-safe):** authenticated request to `preview-<creator-A-slug>-22.preview.zeroship.dev` (creator A owns the sandbox; port 22 is denied) → controller returns 404 uniform (port-denied is one of four reasons aggregated into the oracle-safe response per § II.2; audit log records `not-owner-or-port-denied`). MUST NOT differentiate from "sandbox-not-found" or "not-owner" on the wire.
  - **I9 (Invariant-2) defense-in-depth port deny — agent layer:** with controller patched to accept `9229`, agent still returns `400 port not allowed`. The agent's own port-deny stays at 400 (no public oracle concern at the agent layer — only the controller is publicly addressable; this is a defense-in-depth bounce visible only to a misconfigured controller).
  - **I12 (Invariant-2) agent listener:** verify `ss -tlnp` inside VM shows `0.0.0.0:7777` and only one interface (tap).
  - **I2 (Invariant-2) empty-vs-absent query corner cases:** Phase-1 unit tests for the canonical-string emitter cover all 6 rows in the table.
  - **D-12 verification (existing):** the regression test that boots a fixture Vite and confirms agent loopback proxy works for both bind variants (`127.0.0.1` and `0.0.0.0`) lands here.
  - **§ II.1.x response-header rewrites (D-17):**
    - **Set-Cookie Domain strip:** upstream returns `Set-Cookie: foo=bar; Domain=localhost; Path=/`; browser sees `Set-Cookie: foo=bar; Path=/` (no `Domain=` attribute). Variant: `Set-Cookie: a=1; Domain="localhost"; HttpOnly` (quoted) → `Set-Cookie: a=1; HttpOnly`.
    - **Location host rewrite:** upstream returns `Location: http://127.0.0.1:5173/login`; browser sees `Location: https://preview-{slug}-5173.preview.zeroship.dev/login`. Path / query / fragment preservation: `Location: http://localhost:5173/foo?bar=baz#frag` → `Location: https://preview-…/foo?bar=baz#frag` (byte-exact tail).
    - **Relative Location pass-through:** upstream returns `Location: /login`; browser sees identical bytes (no rewrite).
    - **Multiple Set-Cookie:** upstream returns two `Set-Cookie` headers, both with `Domain=`; browser sees both with `Domain=` stripped, in the same emission order (verifies the multi-header iteration + order-preservation invariant).
    - **Refresh header:** upstream returns `Refresh: 0; url=http://localhost:5173/x`; browser sees `Refresh: 0; url=https://preview-…/x`.

**Rollback:** `git revert` the two crate diffs. The new endpoints are additive — no other path changes.

### Phase 2: WebSocket Upgrade + `auth.ed25519-v1.1` (~10h)

<!-- Re-estimated in round 1: ntex Upgrade hijacking + WebSocket-Key binding + the v1.1 capability flip is more than 6h. -->

**Scope:**
- `proxy.rs::proxy_ws()` handler — splice TCP after a 101.
- ntex Upgrade-handling glue (or compio raw-socket if ntex's surface is awkward).
- Capability `"proxy.ws-v1"`.
- Controller-side streaming forwarder for WS (don't buffer).

**Test plan:**
- Boot a fixture `ws-echo` server inside the sandbox; open a WebSocket from the test harness through the controller; assert echo round-trip.
- HMR-realistic test: boot Vite via `/exec`; have the test write a file (`/files`); assert HMR fires within 2 s.
- Drain test: open WS, fire `/shutdown`, assert client receives 1001 within 5 s.

**Rollback:** drop the capability bit; the agent route returns the existing `unauthorized` for the WS path. The `proxy.http-v1` Phase 1 work continues to function.

### Phase 3: Share tokens + token metadata + `__zsbx_share` cookie-conversion CSRF guard (~8h)

<!-- Re-estimated in round 1: HMAC validation with secret_version, JSON token format, audit metadata, Sec-Fetch-Site enforcement, plus per-sandbox token-mint rate-limit is closer to 8h than 4h. -->

**Scope:**
- `preview_secret` storage in the registry (per-sandbox).
- `POST /sandboxes/{id}/preview/{port}/share` + GET / DELETE handlers.
- Public-edge `__zsbx_share?t=...` cookie-conversion handler.
- HMAC validation, scope enforcement, expiry.

**Test plan:**
- Mint, exchange, fetch — green path.
- Tamper: flip a byte in payload → 401.
- Expiry: `expires_in_secs=2`, sleep 3 s, fetch → 401 with `code: "expired"`.
- Scope: `ro` token + `POST` through the dispatch path → 404 uniform; `ro` token + `POST` through the cookie-conversion endpoint (`?t=<token>` first hit) → 403 `code: "scope_forbidden"` (UX-helpful). Both behaviors are pinned with regression tests.
- Cross-sandbox: `share token for sbx-A` against `preview-sbx-B-...` → 401.
- **Path scope (D-5):** `share token` against `/sandboxes/{id}/exec` (the shell endpoint) → 401 — the validator only runs on the preview public route.
- **Round-6 additions:**
  - **CRITICAL-3 (Invariant-1) cookie-after-revoke:** mint share token, convert to cookie via `__zsbx_share`, hit preview (200), explicit-DELETE all share tokens, hit preview with the still-fresh-in-browser cookie → 401 with `code: "revoked"`.
  - **CRITICAL-2 (Invariant-2) authorize ownership gate:** boot two sandboxes (creator-A's and creator-B's). With creator-A's bearer/session cookie, `GET /sandboxes/<creator-B-sandbox-id>/preview/5173/foo` → 404 (not 403 not 401). Same on the public-edge `preview-<creator-B-slug>-5173.preview.zeroship.dev` with creator-A's cookie → 404. Audit log records `not-owner-or-port-denied`.
  - **H4 (Invariant-1) 401-vs-404 oracle:** anonymous request against `preview-<existing-slug>-5173...` and `preview-<fabricated-slug>-5173...` produce byte-identical 401 bodies (modulo the slug echoed in the `login_url`'s `next`).
  - **H5 (Invariant-1) HMAC-input invariant:** mint two tokens with different field orders inside the JSON; both verify if and only if the validator hashes the on-wire bytes (positive); a validator that re-encodes the JSON before hashing fails (negative).
  - **H7 (Invariant-1) iss claim:** v7 audit-only — token without `iss` warns in audit, still validates; token with `iss` records the value.
  - **LOW-3 separator:** parser accepts `payload~sig` (success), rejects `payload.sig` (round-6 separator change), rejects `payload~sig~extra` (multiple separators).

**Rollback:** drop the share-token routes; existing creator-cookie auth keeps working.

### Phase 4: Edge TLS + wildcard DNS (~1d ops + 2h code)

**Scope:**
- Let's Encrypt DNS-01 client wiring for `*.preview.zeroship.dev`.
- Cert renewal cron in the controller (or via cert-manager if k8s).
- TLS listener on the controller (port 443).
- Subdomain → `(sandbox_id, port)` extraction in the public route.
- DNS records (operator: provision wildcard A/AAAA → controller LB).

**Test plan:**
- ALPN + cert chain validates.
- HTTPS request to a real preview URL works in Chrome / Safari / Firefox.
- HMR via wss:// works (this is the actual bar).
- **Round-6 additions:**
  - **LOW-2 CAA records:** `dig +short CAA preview.zeroship.dev` returns the operator's allowlist; attempt to issue from a different ACME account fails with CA-side CAA error.
  - **I11 DNSSEC:** `dig +dnssec preview.zeroship.dev` returns `AD` flag set; tampered NXDOMAIN response from off-path resolver is rejected by validator.
  - **CRITICAL-1 cookie SameSite consistency:** Browser smoke-test: login cookie carries `SameSite=Lax`; share cookie carries `SameSite=Strict`. Programmatic check via Set-Cookie header inspection in test harness.
  - **H6 Sec-Fetch-* fail-closed:** test request to `__zsbx_login` without any `Sec-Fetch-*` headers → 400 `code: "client_too_old"`.
  - **CRITICAL-2 login-JWT replay:** POST a valid login JWT once → 303; replay the same JWT → 401 (jti-LRU). Replay with a substituted `cv_h` → 401. JWT older than 30 s → 401.
  - **postMessage bridge (Invariant-2 CRITICAL-1):** spawn a malicious page on `https://attacker.example` that postMessages to the console with claimed `sandbox_id` of a real sandbox; console MUST drop, audit-log `bridge.origin_mismatch`, take no action.

**Rollback:** keep the controller-internal route working; hide the public route behind a feature flag (`SANDBOX_PREVIEW_PUBLIC_ENABLED`) until the cert + DNS land.

### Phase 5: Operability gates before public launch

<!-- Added in round 3 (C3-1, C3-3, C3-12); renamed from Phase 4.1 in round 5 for chronological clarity. -->

Phase 4 is gated by Phase 5 (runbook + alert wiring) before flipping the feature flag from canary to GA. Specifically, before flipping `SANDBOX_PREVIEW_FEATURE=v6` from canary to GA:

- The Phase-XI golden-signals dashboard exists in the operator's observability tool, with all listed metrics + alert rules deployed.
- A successful drain test on a staging controller (5-phase sequence completes in < 60 s, no aborted creators).
- A successful AEAD-key rotation test on staging (re-seal pass succeeds; controller restart with new key reads records).
- A successful cert-renewal test (force-renew, SIGHUP reload, traffic continues).
- A successful sealed-record corruption recovery test (intentionally corrupt one file, run `verify --repair`, controller logs and proceeds).
- The error-code contract (§ XI.4) is implemented in the AI-builder UI for at least the top-5 codes (`agent_unreachable`, `port_not_listening`, `circuit_open`, `rate_limited`, `expired`).

Estimated 1 day of ops + 0.5 day of UI + 0.5 day of test scripting ≈ 2 days total.

### Phase 6 (later): Option B migration path

**Scope:**
- New gateway manifest entry kind `PreviewProxy { sandbox_id, port, agent_addr }`.
- Per-operator config flag `SANDBOX_PREVIEW_VIA_GATEWAY=true` toggles between A and B.
- Gateway needs L3 reach to the tap subnets (operator-provisioned).
- Auth: gateway uses a short-lived JWT minted by the controller (so the agent's Ed25519 surface stays unchanged).

**Test plan:**
- Same e2e suite, run twice — once via A, once via B.
- Latency benchmark at p50 / p99 — verify B saves ~1–3 ms / hop.

**Rollback:** flip the flag back to A.

---

## V. Migration / backward compatibility

- **Existing sandboxes (post-deploy of phase 1) — preview URLs work automatically.** No schema migration. The registry already holds `(agent_url, signing_key)`; preview is a derivation of that.
- **Older agents (built before this feature) — controller refuses to expose the preview UI.** The feature-detect via `/version`'s `capabilities` list is already in place (see how the K8s/nomad-ch backends call `wait_for_agent_livez` — there's an existing `pubkey_fingerprint` read). We add a similar read for the `capabilities` list and gate the preview endpoints on `proxy.http-v1`.
- **Wire-protocol stability.** No new headers, no new canonical-string format. The Ed25519 v1 contract is unchanged. We add capability strings (additive — never removed).
- **API surface:** new endpoints; nothing modified or removed.
- **Console UI:** the "Preview" button is hidden when the agent's capabilities don't include `proxy.http-v1`, with a tooltip "preview not supported by this sandbox version (agent < X.Y)".

---

## VI. Risks & mitigations

**Severity scale:** `Low` (operational nuisance) · `Medium` (creator-impacting) · `High` (platform-impacting) · `Critical` (security-impacting). All entries below use this convention; capital-C `Critical` is reserved for findings that could compromise tenant isolation or auth integrity.

| # | Risk | Severity | Mitigation |
|---|---|---|---|
| **R-1** | **Controller as proxy bottleneck.** Every preview byte is on the controller's hot path. | Medium | Measure with `zerobench` (HTTP, SSE, WS); set a per-sandbox bandwidth fairness budget; if bottleneck materializes, fast-track Option B for high-throughput operators. Per-sandbox p99 budget: 100 MiB/s creator-side at controller saturation = ~10 GiB/s. |
| **R-2** | **Body buffering for signature.** Large uploads (multi-GB media, ML datasets) exceed memory cap. | High (UX) | Default 100 MiB cap with explicit 413; document; long-term ship `auth.ed25519-v2-streaming` (chunk-HMAC) so we can stream uploads. Until then, point creators at `zeroship.storage` (object store with presigned URLs). |
| **R-3** | **WebSocket abuse.** A malicious creator opens 1000s of WS connections to drain the controller's fd budget or the agent's. | Medium | Per-sandbox WS connection cap (default 64); per-creator sandbox count cap already exists; document the budget. |
| **R-4** | **DNS rebinding attack** against `preview-*.preview.zeroship.dev`. An attacker tricks a victim browser into resolving `evil.preview.zeroship.dev` to an internal IP after first-load. | Medium | Strict Host-header validation in the controller's public route (rule defined in § II.3 "Host-header parsing"); only the canonical `preview-{slug}-{port}.preview.zeroship.dev` form is accepted, others get 421. CSP `frame-ancestors 'self' https://console.zeroship.ai` blocks third-party iframes (single source of truth in § II.7). |
| **R-5** | **CORS / CSRF.** A preview page making `fetch('https://console.zeroship.ai/...')` must NOT acquire creator credentials via inherited cookies. | Medium | Subdomain isolation (D-2): preview domain is `*.preview.zeroship.dev`, console is `console.zeroship.ai` — different eTLD+1 in `.dev` vs `.ai`. SameSite=Lax or Strict on the console session cookie. Strip `Cookie: zsbx_console_session=...` if it ever leaks (defense-in-depth). |
| **R-6** | **Auth bleed: share token → /exec.** The HMAC validator must enforce that share tokens only authorize the preview path family, never `/exec` or `/files`. | **CRITICAL** | D-5 — preview share-token validator lives in the preview-only route handler; the `/exec` and `/files` routes never see preview tokens. Add a regression test that posts a valid share token to `/sandboxes/{id}/exec` and asserts 401. |
| **R-7** | **Persistent secret leak.** If we ever serialize `preview_secret` to disk (planned for v2 to survive controller restart), encrypt-at-rest. | Low (v1 in-memory only) | Document; v1 ships in-memory; v2 ADR will cover the at-rest story. |
| **R-8** | **Vite `allowedHosts` misconfig.** A creator without our recommended config gets `403 Invalid Host header`. | Low (UX) | Ship a Vite plugin `@zeroship/vite-preview` that auto-applies the right `server.allowedHosts` and `server.hmr.clientPort` based on env hints. AI builder's templates default to using the plugin. |
| **R-9** | **127.0.0.1 ≠ user-bound interface.** D-12 assumes same-netns; if a future backend (e.g. firecracker with multi-vNIC, or k8s with sidecar containers) splits the netns, the agent loopback breaks. | Low | Add a regression test (§ IV.1); document the assumption in `crates/sandbox-agent/README.md`; if it ever changes, the proxy gets a `--upstream-bind` flag to dial something other than `127.0.0.1`. |
| **R-10** | **Unauthenticated preview by URL leak.** Phase 4 ships subdomain-routed preview; if the URL leaks (Discord paste, Slack), strangers can hit it. | Medium | Default to creator-cookie auth (anonymous reaches a 401 page with a "log in" CTA); share tokens are explicit opt-in; recommend short TTL (default 1 h, max 1 week). |
| **R-11** | **Edge TLS rate limits** (Let's Encrypt). The relevant LE limits are 300 new orders / 3 h per registered domain and 5 duplicate certs / week — a single `*.preview.zeroship.dev` wildcard is unaffected at any plausible scale. | Low | Single wildcard; renew well before expiry (we already do for `*.zeroship.ai`). |
| **R-12** | **CORS preflight from console origin.** The console (`console.zeroship.ai`) needs to fetch logs / call internal preview APIs from the preview origin (`*.preview.zeroship.dev`). Without explicit CORS, these requests fail. | Medium | The preview controller's edge route honours `Origin: https://console.zeroship.ai` (and the per-region console origin) for `OPTIONS` preflights, returning `Access-Control-Allow-Origin`, `Allow-Credentials: true`, `Allow-Headers: authorization, content-type`, `Allow-Methods: GET, POST, PUT, DELETE`. Other origins get `Access-Control-Allow-Origin: null`. Documented in § II.7 below. |
| **R-13** | **Cookie-conversion CSRF.** A malicious page links to `https://preview-victim-…/__zsbx_share?t=stolen-token`; the victim's browser auto-fetches; the controller installs a cookie scoped to the victim's preview origin. | High | The `__zsbx_share` and `__zsbx_login` handlers REQUIRE `Sec-Fetch-Site: same-origin` OR `Sec-Fetch-Site: cross-site` paired with `Sec-Fetch-Dest: document` AND `Sec-Fetch-Mode: navigate` (i.e., a top-level navigation, not a JS fetch). They also require an `Origin` header on POSTs. They REJECT `Sec-Fetch-Site: cross-site` for fetch/iframe. CSP `frame-ancestors 'self' https://console.zeroship.ai` (defined in § II.7) blocks third-party iframe exploits while still permitting the console's iframe. Rate-limit per source IP at the edge (R-14). |
| **R-14** | **No rate limiting at the public edge.** Phase-4 ships a public URL with no per-IP, per-sandbox, or per-creator rate limit. Trivial DoS. | High | Edge rate limits at three layers: (a) per-IP global cap (60 RPS sustained, 600 burst); (b) per-sandbox cap (200 RPS sustained, 2000 burst — covers a creator's HMR storm but blocks abuse); (c) per-creator cap (1000 RPS aggregate across all the creator's sandboxes). Token-bucket. Specific endpoints have stricter caps: token-mint = 10/min/sandbox, 100/day/sandbox, **and 1000/day/creator aggregate** (round-2 addition, C2-18); cookie-conversion = 30/min/IP. Defaults documented in § X. |
| **R-15** | **Idle GC reaps a sandbox with active HMR connections.** The sandbox registry's `start_idle_gc` reaps by `last_used` updated on `/exec`/`/files`. A long-lived HMR WebSocket holds the socket for hours but doesn't bump `last_used`; the sandbox is reaped, the WS dies, the creator sees a confusing "preview broken" mid-edit. | High | `proxy.http` and `proxy.ws.upgrade` audit events touch `last_used`. Active WebSockets emit a periodic touch (every 60 s, while open). Documented in § II.5. |
| **R-16** | **Concurrent uploads OOM the agent VM.** N concurrent 100 MiB signed uploads = N × 100 MiB agent RAM (the existing `verify_signed` buffers). At N=21, agent VM (default 2 GiB) OOMs. | High | Per-sandbox `MAX_INFLIGHT_LARGE` = 2 + per-controller global cap (see § II.1 Body streaming). Hardcoded ceiling of 2 concurrent large bodies per sandbox; queue beyond yields 503 with `Retry-After`. |
| **R-17** | **`zeroship.storage` not provisioned for pre-publish creators.** R-2's mitigation says "use the object store for big uploads" but the creator hasn't published yet; no app-scoped storage exists. | Medium | Phase-3 introduces a *preview-mode storage shim* — a per-sandbox bucket auto-provisioned at sandbox-create, lifetime-bound to the sandbox, accessible via the `zeroship.storage` SDK with the sandbox's preview secret as the credential. § IX cross-cutting. |
| **R-18** | **Capability-skew during rollout.** A controller deploy of phase-1 lands; in-flight sandboxes still run the old agent (no `proxy.http-v1`). Creators see "preview broken" with no path to recovery short of recreating the sandbox. | Medium | The console UI reads the agent's `capabilities` list (already reported via `/version`); when `proxy.http-v1` is missing the UI surfaces "preview requires agent ≥ X.Y; restart sandbox to upgrade" with a one-click restart button. Documented in § V Migration. |
| **R-19** | **Search engines / scrapers indexing preview URLs.** Leaked URLs (Discord, Slack) end up in Google's index. | Low | Edge always emits `X-Robots-Tag: noindex, nofollow, noarchive, nosnippet`; preview URLs return a `robots.txt` of `Disallow: /` if the path is `/robots.txt`; CSP `referrer 'no-referrer'`. |
| **R-20** | **Audit-log injection.** Audit fields include `path`, which the creator's own app controls. A path of `/foo\n2026-05-01 admin login from 1.2.3.4` could forge log lines if the audit pipeline renders text. | Medium | All audit events are emitted as **JSON-encoded** records (round-2: `crates/sandbox-agent/src/audit.rs` already structures events; this risk is about the *pipeline*). Path is escaped per JSON; the operator's log-shipper MUST treat the JSON line as a structured record and not as a free-text format string. Documented in the operator runbook (§ IX). Retention default 30 days, operator-tunable. |
| **R-21** | **TLS private key for `*.preview.zeroship.dev` compromise.** Pulled from a controller host = ability to stand up `evil.preview.zeroship.dev` on attacker IP and phish creators' cookies. | High | Key is held only in memory (loaded at boot from KMS or a secret-store-mounted file); never written to disk in plaintext; never logged; rotated by controller restart. ACME-DNS-01 renewal happens out-of-process and the key is reloaded via SIGHUP. CT-monitoring (Certificate Transparency) on `*.preview.zeroship.dev` to catch unauthorized cert issuance. See § IX "Operational secrets". |
| **R-22** | **Token-in-URL leakage to browser history / Referer / extensions.** `__zsbx_login?token=` and `__zsbx_share?t=` both put a secret in the URL. Even with `Referrer-Policy: no-referrer`, the URL is in `history.pushState`, addressbar, possible tab-sync. | High | The creator-login flow uses **POST form-submit** (round-2 rewrite, § II.3) — the token is in the request body, not the URL; only `__zsbx_share?t=` retains the URL form. For shares we (a) recommend short TTL (default 1 h, max 24 h for chat-pasted shares), (b) emit `Cache-Control: no-store` + `Clear-Site-Data: "cache"` on the conversion redirect, (c) document the residual risk in operator + creator-facing docs, (d) offer a console-side "POST-share" alternate flow for high-value shares (a one-click "open in new tab" that POSTs the token; phase 4+ UX). |

---

## VII. Alternatives considered

### B. Gateway-direct routing into tap subnets

Discussed in § I — viable migration target post-v1, not the v1 path.

#### B.1 Migration story (Option A → Option B)

<!-- Round-5 (C5-5): the v3 doc said "we preserve a clean migration path" without specifying what the migration looked like. -->

**User-visible URL is unchanged.** Creators continue to hit `https://preview-{slug}-{port}.preview.zeroship.dev/...`. Only the routing inside the platform changes; no code in the AI builder, no creator-side config edits, no DNS changes.

**Per-operator flag.** `SANDBOX_PREVIEW_VIA_GATEWAY=true` (set on the controller) tells the controller to register the preview routes in the **gateway's** route registry instead of serving them itself. Default `false` (Option A). Operators who run on a single host (gateway colocated with sandbox-host, common in nomad-ch) flip to true; operators with split-rack k8s deployments stay on A.

**Per-sandbox override (graceful rollout).** A sandbox's `SandboxAuth.preview_via_gateway` field can override the operator default per-sandbox; allows canary rollout: 1% → 10% → 100% of newly-created sandboxes flipped to gateway-direct, observe golden signals, ratchet up. Existing sandboxes are NOT migrated mid-flight; they finish their lifetime on whatever path they were created on.

**Auth at the gateway.** The gateway uses a short-lived JWT minted by the controller — the agent's Ed25519 surface is unchanged. JWT carries `(sandbox_id, port, exp)` and the gateway HMACs against a controller-shared key. Agent verifies the JWT identically to today's Ed25519 (the JWT is the *gateway's* auth to the agent; the existing `proxy.http-v1` capability covers it because the wire format is unchanged from the agent's POV).

**Rollback.** Flip `SANDBOX_PREVIEW_VIA_GATEWAY=false` and restart the controller; gateway route registrations drain naturally (5 s pull cycle). New requests revert to controller-mediated. In-flight WS see Close 1001 from the gateway with `reason: "rollback-to-option-a"`; clients reconnect.

**Compatibility floor.** Both modes require the agent advertise `proxy.http-v1` and `proxy.ws-v1`. There is no "old mode" of routing that bypasses these capability checks.

**What changes for whom:**
- **Creators:** nothing visible.
- **SREs:** new gateway dashboard panel (gateway-side preview routing latency, error rates); Option-A panels keep ticking until 0% of sandboxes use it.
- **Gateway maintainers:** new manifest entry kind `PreviewProxy { sandbox_id, port, agent_addr, gateway_jwt_kid }`; route registry pulls every 5 s from the controller as today.
- **sandbox-agent maintainers:** unchanged.

### C. Reverse-tunnel from agent to a public edge

Discussed in § I — reinvents Cloudflare Tunnel; tunnel server is a SPOF.

### D. Per-sandbox CNI + Ingress controller

k8s-only — kills the unified-interface promise. Skip.

### E. "Deploy and publish at every save"

Treat the builder as deploy-on-save: every file edit produces a `.zsapp` and pushes through the worker tier. Rejected:

- HMR is impossible — the worker runtime is per-request, no long-lived process bound to port 5173.
- `.zsapp` build + upload + worker reload is ~5–10 s; unusable for an interactive builder UX.
- Multiplies platform load by ~1000× (every keystroke).

### F. Browser → agent direct via WebRTC data channels

The browser opens a STUN-traversed WebRTC connection to the agent inside the VM; agent multiplexes that as HTTP frames.

- Pros: bypasses the controller; multi-region native; works behind NAT.
- Cons: massive complexity (DataChannel framing, ICE candidate exchange, signaling protocol); WebRTC stacks are huge; user-facing browsers all have WebRTC, but UX is fragile (corp networks block UDP, fallback TURN servers needed).
- Verdict: research-grade; not v1.

### G. Cloudflare Tunnel / ngrok / Tailscale Funnel

Off-the-shelf vendor for the public-edge tunnel.

- Pros: zero code; production-grade.
- Cons: Goal 6 violation (third-party dep); per-tunnel cost at scale; vendor lock-in for a critical platform path; doesn't compose with the per-sandbox secret model.

### H. Single shared preview subdomain + path routing (`preview.zeroship.dev/{id}/{port}/...`)

- Pros: one DNS record, one cert.
- Cons: cookie-bleed across sandboxes (Domain=preview.zeroship.dev cookies visible to all); Vite's `base` needs path rewriting; CSP `connect-src` complications; collisions with the subdomain encode the sandbox-id only (no port).
- Verdict: **rejected.** D-2 + D-3 chose subdomain routing.

---

## VIII. Open questions

The questions below are NOT design choices the doc has settled — they need a human decision before phase-4 ship.

- **Q-1: Wildcard cert vs per-subdomain.** Wildcard is simpler ops; per-subdomain `(sandbox_id, port)` certs (dynamically issued) give us tighter pin-per-sandbox HSTS but blow past LE rate limits at scale. **Recommend wildcard;** decision needed: any ops/security folks blocked on per-cert pinning?
- **Q-2: Default access mode.** Should the bare preview URL require login (default 401 → console login), OR be reachable by anyone with the URL (default 200 if URL is well-formed)? Argument for the former: leaked URLs are common; argument for the latter: it matches Vercel/Netlify preview URLs that creators expect. **Recommend "creator-only by default; share-token explicit opt-in"** but the AI builder may want anonymous-by-default for in-flight demos.
- **Q-3: Port encoding.** Subdomain (`preview-{id}-{port}.…`) vs path (`preview-{id}.…/{port}/…`)? **D-3 chose subdomain;** flagging as open in case the cookie-isolation argument loses to the operational simplicity of one cert + one DNS record.
- **Q-4: Per-sandbox preview budget.** Should a creator be allowed to keep an HMR connection open indefinitely, or do we time-bound (e.g. 8 h / sandbox session, then force a tab refresh)? Argument for time-bound: long-lived WS connections eat fd budget. **Recommend 8 h soft cap with auto-reconnect grace.**
- **Q-5: Share-token TTL ceiling.** Currently bounded at 1 week. Long enough for a "demo this to my collaborator" flow, short enough that leaks have a natural decay. Confirm 1 week is right; legal/ToS may want lower or higher.
- **Q-6: Body cap default.** 100 MiB is comfortable for most dev iteration but not for AI workloads (training data, model weights). Recommend keeping 100 MiB and pointing creators at `zeroship.storage` for big uploads.
- **Q-7: Should preview support HTTP/2 to the upstream?** Some Vite versions and Node 20+ servers respond HTTP/2 by default when given a TLS cert. Our agent dials plaintext HTTP/1.1 to `127.0.0.1`; HTTP/2 over plaintext requires an Upgrade dance. **Recommend HTTP/1.1 only in v1;** force the user to disable HTTP/2 on their dev server, or document that HTTP/2-over-cleartext (h2c) goes through fine if the user uses it.
- **Q-8: Should we expose a `zeroship.preview.*` SDK primitive** to the creator's app code (e.g. `zeroship.preview.url(port)` returns the public URL)? Convenient for debug; couples app code to non-production state. **Recommend NO** for v1; preview URLs are a builder concept, not an app concept.
- **Q-9: Audit log fields for preview access.** Today the agent emits audit events for `/exec`, `/files`. Preview proxy events (especially WS Upgrade) — what fields? Recommend: `(sandbox_id, port, method, path, status, principal_kind ∈ {creator, share}, principal_id, byte_count, duration_ms)`. **Confirm fields with security review.**
- **Q-10: Regional binding.** A sandbox is sticky to a region (the controller that created it). The preview URL therefore CNAMEs to that region's controller. Multi-region failover for previews is out-of-scope for v1. Confirm the regional model with platform-ops; we may want a `region` label baked into the slug to surface routing failures earlier.
- **Q-11: HTTP/2 ALPN at the public edge.** Should the controller advertise `h2` in TLS ALPN? Modern browsers strongly prefer h2 for asset-heavy pages (Vite serves dozens of small modules per page-load). Internally we proxy as h1.1 to the agent regardless; the bridge is a minor h2-frontend / h1-backend translation that ntex supports. **Recommend YES;** the cost is small, the win is meaningful.
- **Q-12: Idempotency of share-token mint.** Should `POST .../share` accept an `Idempotency-Key` header (Stripe pattern)? Two clicks from a flaky network shouldn't mint two distinct tokens. **Recommend YES;** key cached for 24 h, returns the original token on duplicate.
- **Q-13: `preview-mode storage shim` lifetime.** When the sandbox dies the shim bucket is wiped — what about a creator with a 500 MiB upload they want to keep? **Recommend transparent migration to a real storage bucket on app-publish;** UX detail; flag for product review.
- **Q-14: cross-controller share-token revocation.** v1 makes sandboxes sticky to the creating controller, so revocation is local. If we ever go active-active for the same sandbox, we need a coordinated KV (compio-redis) for the per-sandbox `secret_version`, otherwise a `DELETE token_id=*` on controller A leaves controller B serving the previous secret. **Recommend deferring** until the multi-controller use-case lands; v1 is single-controller-per-sandbox.
- **Q-15: cert-renewal failure budget.** The controller publishes `proxy_tls_cert_expiry_seconds` and pages at < 7 days. Some operators want a hard fail: refuse to serve preview if cert expiry < 24 h, redirect to a status page. **Recommend NO hard-fail in v1;** the alert is sufficient.
- **Q-16: when to enable `auth.ed25519-v2-streaming`.** v1 buffers ≤ 100 MiB; large uploads cap-out as documented (C4-2 latency budget). When does the streaming-mode wire format become urgent? Triggers: (a) `proxy_inflight_large` saturation > 90% for 2+ weeks running, (b) creator complaints about 100 MiB cap exceed N/week, (c) the AI builder ships a feature that needs > 100 MiB uploads (training data, model weights). Recommend deferring until at least one of these fires; track signals on the dashboard.
- **Q-19: CORS allowlist for `console.zeroship.ai` against preview origins.** When an app legitimately makes a cross-origin fetch from `preview-{slug}-{port}.preview.zeroship.dev` to `console.zeroship.ai` (e.g., the AI builder's iframe-hosted preview wants to call back to a console-side endpoint), the console must respond with `Access-Control-Allow-Origin: https://preview-{slug}-{port}.preview.zeroship.dev` (echoing the request `Origin`) and **validate** that the origin matches the preview-host pattern (the same `^preview-(sbx-[a-z0-9]{20,40})-([1-9][0-9]{0,4})\.preview\.zeroship\.dev$` regex enforced by § II.3 host-header parsing). The exact allowlist regex needs an audit before it ships; **defer to Phase 4** (alongside the public-edge TLS work, since both touch the cross-origin posture). Risk if loose: cross-origin info leak from console to a maliciously-named preview slug; risk if tight: legitimate AI-builder integrations break. Cross-references: § II.1.x "Edge cases #3", § II.6 "CORS".
- **Q-20: Service API for long-lived processes inside the sandbox.** `/exec`'s `killpg(SIGKILL)` on return (`crates/sandbox-agent/src/exec.rs:199`, anti-DoS by design — see "Running long-lived processes inside the sandbox" in `docs/runbooks/sandbox-nomad-ch.md`) forces every dev server (Vite, Node, Python) to use a `setsid sh -c '… &'` launcher to escape the process group. End-to-end testing surfaced this with three failed attempts (plain `&`, `nohup`, `nohup & disown`) before the working pattern. AI-builder system prompts will need to know this verbatim, OR we add an explicit `POST /sandboxes/{id}/services { name, cmd, port }` API that runs the command in a controller-managed pgroup separate from `/exec`'s. The service API would also let us track (start, stop, restart, healthcheck, logs) per service rather than ad-hoc PID files in `/tmp`. **Recommend deferring to post-Phase-5;** until then, document the `setsid` wrapper in the AI builder's system prompt and in the runbook (already done — see runbook section linked above). Cross-references: `docs/runbooks/sandbox-nomad-ch.md` § "Running long-lived processes inside the sandbox".

---

## IX. Cross-cutting follow-ups

Things outside this design's direct scope but flagged for related work:

- **`crates/gateway/src/router.rs`** — phase 6 (Option B migration) needs a new route kind. Not v1.
- **`crates/sandbox/src/registry.rs`** — needs a `preview_secret: [u8; 32]` field per sandbox + `secret_version: u32`. Add to `SandboxState` (in-memory). Confirm no serialization breakage.
- **`crates/sandbox-agent/src/audit.rs`** — add event kinds `proxy.http`, `proxy.ws.upgrade`, `proxy.ws.close`, `proxy.body.too-large`, **`proxy.share.use`** (a share-token-authed access — distinct from a creator-authed one for abuse triage). Audit-event delivery (round-3 C3-14): agent ring-buffers up to **10 000 events** in-memory; on overflow oldest are dropped and `audit_dropped_total` increments. Controller pulls via long-poll (`GET /audit?since=<id>`, signed). Operator alert: `audit_dropped_total > 0 in 5m` → ticket. Logs schema is JSON-per-line with stable field names (round-3 C3-7): `{ts, event, sandbox_id, port, principal_kind, principal_id, method, path, status, duration_ms, body_in, body_out}`. PII fields (`principal_id`, source IP) are SHA-256-hashed for retention beyond 30 days; raw values kept ≤ 30 days.
- **`crates/sandbox-agent/src/metrics.rs`** — counters AND histograms (round-3 C3-6 expansion):
  - Counters: `proxy_requests_total{port_class,status}`, `proxy_bytes_in_total`, `proxy_bytes_out_total`, `proxy_share_token_use_total{port_class,status}`, `proxy_aead_seal_failures_total`, `proxy_aead_unseal_failures_total`, `proxy_token_mint_total{result}`, `proxy_token_validate_total{result}` (result ∈ ok/expired/bad_sig/wrong_audience/wrong_sandbox/wrong_port/scope_forbidden/malformed/revoked), `preview_rate_limited_total{bucket}` (no per-sandbox label — cardinality), `proxy_drain_aborted_ws_total{reason}`, `proxy_circuit_breaker_transitions_total{from,to}`, `proxy_ws_bandwidth_throttled_total{direction}` (round-4 C4-12).
  - Histograms: `proxy_request_duration_seconds{port_class,status,phase}` (phase ∈ verify/sign/upstream_connect/upstream_first_byte/full), `proxy_upstream_connect_duration_seconds`, `proxy_signature_verify_duration_seconds`. Buckets default `[5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms, 1s, 2.5s, 5s, 10s, 30s]`. **`port_class ∈ {3000, 5173, 8080, other}`** (round-4 C4-18: avoids per-port high-cardinality blowup).
  - Gauges: `proxy_ws_active`, `proxy_inflight_large` (no per-sandbox label; per-sandbox state via logs), `proxy_circuit_breaker_state{state_class}` (open/half/closed counts; not labeled per-sandbox), `proxy_drain_active`, `proxy_drain_phase`, `proxy_tls_cert_expiry_seconds`, `proxy_ws_bytes_per_second` (sampled gauge).
- **Preview-mode storage shim** — see § VI R-17. New crate or feature in `crates/plugin-storage/` to provision a per-sandbox bucket at sandbox-create, key on the preview secret, TTL = sandbox lifetime. Lets pre-publish creators test upload flows without provisioning real storage. Tracked as a follow-up; phase-3-or-later work.
- **AI builder system prompts** — in addition to "bind 0.0.0.0", instruct the model to never add `--inspect` or `--inspect-brk` to dev commands (proxy denies 9229 anyway, but failing fast in the prompt avoids a confusing error). Add a Vite plugin auto-detect that warns if `host: 'localhost'` is in `vite.config.ts` (would 403 with `Invalid Host header`).
- **Metering integration** — `crates/control/src/metering.rs` already supports byte-counting metrics. Preview egress (controller → browser bytes) is metered per-sandbox under metric `sandbox_preview_bytes_out` for capacity-planning. NOT charged to the creator in v1 — preview is part of the platform-side cost; documented in § X.
- **`crates/runtime/src/init.rs`** (worker) — confirm preview URLs don't appear inside the worker's TCB. They shouldn't; flagged for double-check.
- **AI builder system prompts** — instruct the model to bind `0.0.0.0` (not `127.0.0.1` only) and to use the recommended Vite config. Prompt-level change, not code, but easy to forget.
- **`@zeroship/vite-preview`** — new npm package. One file. Auto-detects `ZEROSHIP_PREVIEW_HOST` env var (controller-injected when the sandbox starts) and applies `server.allowedHosts` + `server.hmr.clientPort` + `server.hmr.protocol`.
- **`docs/runbooks/sandbox-preview.md`** — operator runbook (TLS cert renewal, share-token operational rotation, capacity planning). Phase 4 deliverable.

### IX.a Operational secrets

<!-- Added in round 2: addressing C2-15 (TLS private key handling) and C2-29 (AEAD key sourcing was a one-liner). -->

The preview surface introduces three new pieces of secret material the platform must manage:

| Secret | Where used | Source | Rotation | Storage at rest |
|---|---|---|---|---|
| **TLS private key for `*.preview.zeroship.dev`** | Controller TLS listener (Phase 4). | At boot, controller loads from `$SANDBOX_PREVIEW_TLS_KEY_PATH` (a secret-store mount) OR `$SANDBOX_PREVIEW_TLS_KEY` env (HashiCorp Vault / KMS-injected). | ACME DNS-01 every 60 days; controller reloads on SIGHUP. Compromise rotation: revoke old cert via Let's Encrypt, issue new, ban old fingerprint via OCSP staple. | Held in process memory only; never logged; never written to controller disk. CT-monitoring on the FQDN catches unauthorized issuance. |
| **Controller-wide AEAD key (XChaCha20-Poly1305)** for sealed records (§ II.0 §4). | `crates/sandbox/src/persist.rs`. | At boot from **`$SANDBOX_AEAD_KEY_PATH` only** (file mount; round-6 H8 — `$SANDBOX_AEAD_KEY` env-var source DEPRECATED + REMOVED, see note below). Recommended source: KMS-decrypted via init-container writing a tmpfs-mounted file, or vault-agent rendering to disk in nomad. | Manual; key change requires a one-shot re-seal pass over `*.sealed` files (operator runbook ships a tool). Compromise rotation: rotate, re-seal, restart. | Never written to controller disk in plaintext; never in logs; never in metrics. |
| **Per-sandbox `signing_key` (Ed25519 SK) and `preview_secret` (32 bytes)** | Per-VM. | Generated at sandbox-create. | Whole-sandbox replacement on `Backend::stop`/`start`; preview_secret rotates within a sandbox via `DELETE token_id=*`. | Sealed-on-disk via the controller AEAD key; in-memory via `Arc<SigningKey>`. |

**Invariants:**

1. None of these secrets ever appear in audit logs, metrics, traces, error responses, or stack traces.
2. The TLS key is the highest-value secret — compromise impacts every preview, not one sandbox.
3. The AEAD key gates *every* sandbox's sealed records — compromise means every sandbox's `signing_key` and `preview_secret` are recoverable. Documented escalation procedure: rotate AEAD key, re-seal, then rotate every per-sandbox key by force-recreating sandboxes (creators see "preview unavailable; restart sandbox").
4. CT-log monitoring on `*.preview.zeroship.dev` is operator-provisioned.
5. **Boot-time AEAD invariant** (round-3 C3-5; updated round-6): if `SANDBOX_PERSIST_AUTH=1` the controller refuses to start without `SANDBOX_AEAD_KEY_PATH` set AND pointing to a readable file with mode `0400` owned by the controller's runtime UID; logs `FATAL: AEAD key path required when persistence is enabled` and exits 1. A boot that *succeeds* is committed to that key; a subsequent boot with a different key (without an explicit re-seal pass) refuses to read existing records (decrypt fails) and surfaces a per-record warning, then leaves the records in place (the operator runbook covers re-seal vs. wipe).

6. **Why file-mount only (round-6 H8).** Environment variables are exposed via `/proc/<pid>/environ` to anything that can read the procfs of the controller's process; on a shared host this is broader than the file-system permission boundary on a 0400-mode file. Containers also routinely log `printenv` output during diagnostics. The file-mount narrows the read surface to processes with explicit FS access; the controller reads the key once at boot and zeroes the buffer after deriving the AEAD subkeys. Removed env-var support intentionally so a future contributor cannot regress.

7. **Host-swap encryption (round-6 I13).** The controller VM's host MUST either disable swap entirely or use an encrypted swap (LUKS-on-swap, or `swapon` against a tmpfs-backed file). Otherwise: kernel pages out a controller process page containing the AEAD key (or a sealed record's plaintext immediately after unseal) to disk, and a host disk-image leak exposes the secret. Operator runbook treats this as a deploy precondition.

8. **CAA records (round-6 LOW-2).** The `*.preview.zeroship.dev` zone publishes CAA records limiting cert issuance to the operator's Let's Encrypt account ID:
   ```
   preview.zeroship.dev. CAA 0 issue "letsencrypt.org; accounturi=https://acme-v02.api.letsencrypt.org/acme/acct/<id>"
   preview.zeroship.dev. CAA 0 issuewild "letsencrypt.org; accounturi=https://acme-v02.api.letsencrypt.org/acme/acct/<id>"
   preview.zeroship.dev. CAA 0 iodef "mailto:security@zeroship.dev"
   ```
   This blocks any other CA from issuing certs for the zone (or any subdomain) even if an attacker compromises a different ACME account. Pair with CT-monitoring for defense-in-depth.

9. **DNSSEC (round-6 I11).** The `*.preview.zeroship.dev` zone is DNSSEC-signed at the authoritative DNS provider. NXDOMAIN as kill switch (e.g., for an abusive sandbox) is therefore authenticated; an off-path attacker cannot forge a positive answer to bypass an NXDOMAIN. Operator runbook covers KSK rollover.

#### Cert renewal failure handling

<!-- Added in round 3 (C3-3): silent renewal failure expires the cert and breaks every preview. -->

- The controller's ACME client publishes `proxy_tls_cert_expiry_seconds` (gauge: seconds until expiry).
- Alert thresholds: `< 14 days` → ticket; `< 7 days` → page on-call; `< 24h` → page + automatic page-2 escalation.
- On renewal failure (DNS provider down, LE rate-limit hit, ACME client bug), the controller logs at ERROR; the existing cert keeps serving until expiry. Manual override: `zeroship-control admin cert renew-now` forces an out-of-band renewal attempt (idempotent — no-op if not yet expired and last attempt was < 1 h).
- Cert reload is via SIGHUP; a successful reload emits `proxy_tls_cert_reloaded_total` and resets the expiry gauge.

#### Sealed-record corruption recovery

<!-- Added in round 3 (C3-11): the v3 doc said "log and proceed" but didn't tell the operator what to do. -->

The controller ships a CLI subcommand `zeroship-control admin sealed-record verify [--all | <sandbox_id>]` that:

1. Reads `{sandbox_id}.json` from the sealed-record directory.
2. Attempts AEAD-unseal with the current AEAD key.
3. Reports `OK | corrupt | wrong-key | unknown-sandbox`.
4. With `--repair`, moves corrupt files to `${SEALED_DIR}/corrupt/<ts>/<sandbox_id>.json` (preserved for forensics) and continues.

Boot-time integrity check: the controller MAY be configured (`SANDBOX_SEALED_VERIFY_ON_BOOT=1`) to scan all records at startup, log corrupt ones, and refuse to start if > 5% are corrupt (catastrophic disk failure or wrong key). Default: scan + log, do not refuse.

Documented in the operator runbook (§ XI / `docs/runbooks/sandbox-preview.md`).

#### Multi-controller HA in v1

<!-- Added in round 3 (C3-4): cross-controller share-token revocation propagation needed clarification. -->

In v1, **a sandbox is sticky to the controller that created it** (Q-10 confirmed). The controller fronting the sandbox's preview URL is the only one with a sealed record for it; rotation of `secret_version` is local to that controller. Cross-controller rotation propagation is therefore not a concern.

If/when a sandbox is migrated between controllers (Q-10 evolution), the sealed record is migrated with the sandbox. Multi-controller HA *for the same sandbox* (active-active) is out of scope until a coordinated KV (e.g., compio-redis) backs the sealed-record store; tracked in Q-14.

Implication for ops: a controller failure makes preview URLs for that controller's sandboxes unreachable until the controller is restored (or sandboxes are migrated). The published-app surface (gateway → worker tier) is unaffected — only previews. Document MTTR target: < 5 min (controller restart from sealed records).

#### Sealed-record durability

Sealed records are **intentionally ephemeral** (round-3 C3-15). Loss of the controller's disk loses preview state for that controller's sandboxes; creators are notified ("preview unavailable; restart sandbox"). No backup procedure required:

- Sealed records hold ≤ `SANDBOX_MAX_LIFETIME_SECS` (default 8 h) of recoverable state per sandbox.
- The published-app surface is durable (DB-backed); only ephemeral preview state is lost.
- This is a deliberate operational simplification; revisit if preview-state loss becomes a recurring complaint.

#### TLS protocol + cipher policy

<!-- Added in round 3 (C3-19). -->

- TLS 1.3 only. TLS 1.2 disabled at the listener.
- ALPN: `h2, http/1.1` (Q-11 recommends YES on h2; here it lands).
- HSTS: `Strict-Transport-Security: max-age=15768000; includeSubDomains; preload` (the `*.preview.zeroship.dev` zone is a candidate for the HSTS preload list; coordinate with the security team before submitting).
- OCSP stapling enabled (must-staple is a long-term goal; enables compromise-detection via stapled responses).

---

## Sequence diagrams

### Creator-authed preview (HTTP)

```mermaid
sequenceDiagram
    participant B as Browser (creator)
    participant E as Edge / Controller :443
    participant R as Registry
    participant A as Agent (in microVM) :7777
    participant U as User app :5173

    B->>E: GET https://preview-sbx_…-5173.preview.zeroship.dev/index.html<br/>Cookie: __Host-zsbx_preview_sbx_…=<jwt>
    E->>E: validate JWT, lookup sandbox_id from subdomain
    E->>R: registry.get(sbx_…)
    R-->>E: SandboxState{agent_url, signing_key}
    E->>E: sign canonical("GET", "/proxy/5173/index.html", body=[], ts, nonce)
    E->>A: GET http://10.99.103.2:7777/proxy/5173/index.html<br/>X-Sbx-Timestamp/Nonce/Signature
    A->>A: verify_signed → ok; is_proxyable_port(5173) → ok
    A->>U: GET http://127.0.0.1:5173/index.html
    U-->>A: 200 OK + body
    A-->>E: 200 OK + body
    E-->>B: 200 OK + body (streamed)
```

### Share-token preview (HTTP, first hit)

```mermaid
sequenceDiagram
    participant V as Visitor (no zeroship account)
    participant E as Edge / Controller :443
    participant R as Registry

    V->>E: GET https://preview-sbx_…-5173.preview.zeroship.dev/__zsbx_share?t=<base64url>
    E->>E: parse token; lookup sandbox preview_secret
    E->>R: registry.get(sbx_…).preview_secret
    R-->>E: secret_v0
    E->>E: HMAC verify, expiry, scope
    E->>V: 303 Set-Cookie: __Host-zsbx_share_sbx_…=<jwt>; HttpOnly; Secure; SameSite=Strict; Path=/<br/>Location: /
    V->>E: GET / (with cookie)
    Note over E: same as creator-authed flow from here
```

### WebSocket Upgrade (Vite HMR)

```mermaid
sequenceDiagram
    participant B as Browser
    participant E as Controller
    participant A as Agent
    participant V as Vite :5173

    B->>E: GET /@vite/client<br/>Upgrade: websocket
    E->>E: sign + forward (signature on the Upgrade request only)
    E->>A: GET /proxy/5173/@vite/client<br/>Upgrade + X-Sbx-* signed
    A->>A: verify_signed → ok
    A->>V: open TCP 127.0.0.1:5173<br/>GET /@vite/client Upgrade: websocket
    V-->>A: 101 Switching Protocols
    A-->>E: 101 (relay headers)
    E-->>B: 101 (relay headers)
    Note over B,V: now the two TCP sockets are spliced;<br/>frames flow B↔E↔A↔V without further auth checks
    B->>E: <ws frame>
    E->>A: <ws frame>
    A->>V: <ws frame>
    V->>A: <ws frame>
    A->>E: <ws frame>
    E->>B: <ws frame>
```

### Sandbox teardown mid-WS

```mermaid
sequenceDiagram
    participant B as Browser
    participant E as Controller
    participant A as Agent
    participant V as Vite

    Note over B,V: WS already established
    B->>E: <frame>
    E->>A: <frame>
    A->>V: <frame>

    Note over E: DELETE /sandboxes/{id} arrives on a different connection
    E->>A: POST /shutdown (signed)
    A->>A: mark_draining
    A--xV: close upstream socket (RST)
    A-->>E: WS close 1001 Going Away
    E-->>B: WS close 1001 Going Away
    Note over B: client auto-reconnect logic (Vite) sees the close,<br/>retries; gets 503 from the controller (sandbox no longer in registry);<br/>UI surfaces "sandbox stopped"
```

---

## Appendix A: recommended Vite config

<!-- Round 5 (C5-3): the v3 single-config example assumed the public-edge form, but during AI building creators hit the console-side route (different host, path-prefixed). We document both modes and ship `@zeroship/vite-preview` to handle the dispatch. -->

The recommended path is the **`@zeroship/vite-preview` plugin**, which inspects controller-injected env vars and applies the right config automatically. Creators install one package, no manual config edits.

### A.1 `@zeroship/vite-preview` plugin (recommended)

```ts
// vite.config.ts
import { defineConfig } from 'vite';
import zeroshipPreview from '@zeroship/vite-preview';

export default defineConfig({
  plugins: [zeroshipPreview()],   // reads env, applies the right server.* config
  server: {
    host: '0.0.0.0',
    port: 5173,
    strictPort: true,
  },
});
```

The plugin sets `server.allowedHosts`, `server.hmr.host`, `server.hmr.protocol`, `server.hmr.path`, and `server.hmr.clientPort` based on `ZEROSHIP_PREVIEW_MODE` (`console` or `public`):

- **`console` mode (default during AI building):**
  - `allowedHosts`: `['console.zeroship.ai']` plus regional console origins.
  - `hmr.protocol`: `'wss'`, `hmr.clientPort`: `443`.
  - `hmr.host`: `'console.zeroship.ai'`.
  - `hmr.path`: `'/sandboxes/{sandbox_id}/preview/{port}/@vite/client'` (path-prefixed because the console fronts the controller-internal route).
- **`public` mode (when `ZEROSHIP_PREVIEW_MODE=public`):**
  - `allowedHosts`: `['.preview.zeroship.dev']`.
  - `hmr.protocol`: `'wss'`, `hmr.clientPort`: `443`.
  - `hmr.host`: `'preview-{slug}-{port}.preview.zeroship.dev'` (from `ZEROSHIP_PREVIEW_HOST_PUBLIC`).
  - `hmr.path`: `'/@vite/client'` (subdomain routing; no path prefix).

### A.2 Manual config (for reference / non-Vite frameworks)

If a creator can't use the plugin (e.g., another framework), the equivalent values:

| Env var | Value | Use |
|---|---|---|
| `ZEROSHIP_PREVIEW_MODE` | `console` \| `public` | Which mode the controller has the sandbox in. |
| `ZEROSHIP_PREVIEW_HOST_PUBLIC` | `preview-sbx-01HF…-5173.preview.zeroship.dev` | The public-edge hostname (one per port). |
| `ZEROSHIP_PREVIEW_HOST_CONSOLE` | `console.zeroship.ai` | The console-mode hostname. |
| `ZEROSHIP_PREVIEW_PATH_CONSOLE` | `/sandboxes/{sandbox_id}/preview/{port}` | The console-mode path prefix. |
| `ZEROSHIP_SANDBOX_ID` | `sbx_01HF…` | The sandbox-id (typed-id form). |

The dev server should accept connections from both `console.zeroship.ai` AND `*.preview.zeroship.dev`; HMR should connect back to whichever host matches the page's origin.

## Appendix B: example agent test fixture

```rust
// crates/sandbox-agent/src/proxy.rs (test mod)
#[ntex::test]
async fn proxy_http_get_roundtrips() {
    let upstream = spawn_test_http_server(|req| async move {
        assert_eq!(req.uri().path(), "/foo");
        HttpResponse::Ok().body("hello from upstream")
    }).await;   // listens on a fresh ephemeral port, returns the port

    let (state, _d) = make_state("proxy_get");
    let app = make_app!(state);
    let path = format!("/proxy/{}/foo", upstream.port);
    let req = signed("GET", &path).to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    assert_eq!(&body[..], b"hello from upstream");
}

#[ntex::test]
async fn proxy_refuses_disallowed_port() {
    let (state, _d) = make_state("proxy_bad_port");
    let app = make_app!(state);
    let req = signed("GET", "/proxy/22/anything").to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[ntex::test]
async fn proxy_body_over_cap_returns_413() {
    let (state, _d) = make_state("proxy_too_big");
    let app = make_app!(state);
    let big = vec![0u8; 101 * 1024 * 1024];   // 101 MiB
    let req = signed_with_body("PUT", "/proxy/3000/big",
        std::str::from_utf8(&big).unwrap()).to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[ntex::test]
async fn share_token_cannot_access_exec() {
    // R-6 regression: a preview share token for sbx-A must fail
    // against /sandboxes/sbx-A/exec — the share-token validator
    // is preview-only. Note that /exec is signed via X-Sbx-* headers
    // (Ed25519), NOT Authorization: Bearer; the share token
    // therefore can't even reach the /exec validator. We assert
    // both surfaces explicitly:
    let token = mint_share_token("sbx-A", 5173, "ro", 3600);

    // (a) Public-edge route: passing the share token via ?t= against
    // /exec should not match the route at all (404) — /exec lives on
    // the controller-internal API, not the public preview origin.
    let req_public = test::TestRequest::post()
        .uri(&format!(
            "https://preview-sbx-A-5173.preview.zeroship.dev/__zsbx_share?t={token}&next=/exec"
        ))
        .to_request();
    let resp_public = test::call_service(&app, req_public).await;
    assert_ne!(resp_public.status(), StatusCode::OK, "share-token cookie-conversion must not redirect to /exec");

    // (b) Internal route: hitting /exec with the share token in any header
    // must 401 because the canonical signature won't match (no Ed25519 sig).
    let req_internal = test::TestRequest::post()
        .uri("/sandboxes/sbx-A/exec")
        .header("x-sbx-timestamp", "1746000000")
        .header("x-sbx-nonce", "n1")
        .header("x-sbx-signature", token)   // garbage; not a sig
        .set_payload(r#"{"cmd": "echo pwned"}"#)
        .to_request();
    let resp_internal = test::call_service(&app, req_internal).await;
    assert_eq!(resp_internal.status(), StatusCode::UNAUTHORIZED);
}
```

## X. Rate-limit defaults

<!-- Added in round 1: addressing Missing C — phase-4 ships a public URL with no documented rate-limit story. -->

Token-bucket. All values are operator-tunable; ship-time defaults:

| Bucket | Sustain | Burst | Refill rate | Notes |
|---|---|---|---|---|
| Per-IP global | 60 RPS | 600 | 10 RPS / s | Across all preview origins, all sandboxes. |
| Per-sandbox total | 200 RPS | 2000 | 50 RPS / s | All requests to one `preview-{sandbox-slug}-{port}.…` origin. |
| Per-creator aggregate | 1000 RPS | 4000 | 100 RPS / s | Across all the creator's sandboxes. |
| Token-mint per sandbox | 10 / min | 30 | 10 / min | Caps abuse of `POST .../share`. |
| Token-mint per sandbox per day | 100 / day | — | reset 00:00 UTC | Hard-bound; exceeds → 429. |
| Token-mint per creator per day | 1000 / day | — | reset 00:00 UTC | Round-2 (C2-18). Caps creator-with-100-sandboxes abuse: 100 sandboxes × 100/day = 10k tokens/day; cap is 1k aggregate. |
| Cookie-conversion (`__zsbx_*`) per IP | 30 / min | 60 | 30 / min | Mitigates CSRF spamming. |
| Probe (`.../preview/.../probe`) per sandbox | 5 / min | 5 | 5 / min | Console-side; cheap but pollutes audit log if abused. |

429 responses include a `Retry-After` header (seconds) and a `code: "rate_limited"` JSON body. Metric: `preview_rate_limited_total{bucket,sandbox}`.

## XI. Operator runbook outline + golden signals

<!-- Added in round 3 (C3-1, C3-12, C3-18): the v3 doc named a runbook file but didn't sketch its contents or define paging thresholds. -->

The runbook (`docs/runbooks/sandbox-preview.md`, Phase-4 deliverable) contains the following sections; this design pre-defines their content so the runbook is a faithful expansion, not an open question.

### XI.1 Golden signals

Latency, traffic, errors, saturation — measured per (controller, sandbox-fleet aggregate):

| Signal | Metric | Target | Alert (page) | Alert (ticket) |
|---|---|---|---|---|
| **Latency** (HTTP p99) | `proxy_request_duration_seconds:p99` | < 500 ms in-region | > 2 s for 5 min | > 1 s for 15 min |
| **Latency** (Upstream connect p99) | `proxy_upstream_connect_duration_seconds:p99` | < 50 ms | > 500 ms for 5 min | > 200 ms for 15 min |
| **Traffic** (RPS) | `rate(proxy_requests_total[1m])` | informational | — | — |
| **Errors** (5xx rate) | `rate(proxy_requests_total{status=~"5.."}[5m]) / rate(proxy_requests_total[5m])` | < 0.5 % | > 5 % for 5 min | > 1 % for 30 min |
| **Errors** (signature verify failures) | `rate(proxy_token_validate_total{result=~"bad_sig\|wrong_audience"}[5m])` | < 0.1 % | > 1 % for 5 min | > 0.5 % for 30 min |
| **Saturation** (inflight large) | `proxy_inflight_large` | < 80 % of cap | > 95 % for 2 min | > 80 % for 15 min |
| **Saturation** (WS active) | `proxy_ws_active` | < 80 % of fd budget | > 95 % for 2 min | > 80 % for 15 min |
| **Saturation** (rate-limited) | `rate(preview_rate_limited_total[1m])` | < 10/s fleet | > 100/s for 5 min (likely DDoS) | > 50/s for 30 min |
| **Cert expiry** | `proxy_tls_cert_expiry_seconds` | > 30 days | < 7 days | < 14 days |
| **Circuit-breaker** | `proxy_circuit_breaker_state` | mostly 0 | open > 5 min | flapping > 10/h |
| **AEAD failures** | `rate(proxy_aead_unseal_failures_total[5m])` | 0 | > 0 for 1 min (page) | — |
| **Audit dropped** | `rate(audit_dropped_total[5m])` | 0 | > 0 for 5 min | — |

### XI.2 First-cut diagnostic decision tree

3 AM page: "preview p99 > 2s". First commands:

1. `kubectl logs -l app=zeroship-control --since=10m | grep proxy_circuit` — circuit breaker tripped? Which sandbox?
2. `curl -s controller:9091/metrics | grep proxy_upstream_connect_duration_seconds:p99` — upstream slow? Then likely user-app is hung; not platform fault. Surface to creator UI.
3. `curl -s controller:9091/metrics | grep proxy_inflight_large` — saturation? If yes, check per-sandbox label; one greedy sandbox?
4. `kubectl exec controller -- zeroship-control admin sealed-record verify --all` — corruption?
5. Cert expiry: `proxy_tls_cert_expiry_seconds < 86400` → cert is the problem; force-renew.
6. AEAD failures non-zero: secret-store outage or wrong key after restart; escalate.

### XI.3 Routine operations

| Operation | Command / procedure | Frequency |
|---|---|---|
| Cert force-renewal | `zeroship-control admin cert renew-now` | Manual (alerted) |
| Sealed-record verify | `zeroship-control admin sealed-record verify --all` | Weekly cron + on alert |
| Sealed-record corruption repair | `zeroship-control admin sealed-record verify --all --repair` | On alert |
| Fleet-wide share-token revoke (panic button) | `zeroship-control admin sandbox-preview revoke-all` | Emergency (R-22 leak) |
| Drain controller for restart | `zeroship-control admin drain --timeout=45s` | Per deploy |
| Disable preview entirely (kill switch) | `SANDBOX_PREVIEW_DISABLED=1` (env on controller) + restart | Emergency |
| Per-sandbox revoke + secret bump | `DELETE /sandboxes/{id}/preview/{port}/share?token_id=*` | Creator-initiated; operator escalation |
| Re-seal records after AEAD rotation | `zeroship-control admin sealed-record reseal --old-key=<...> --new-key=<...>` | Coordinated rollover |

### XI.4 Error code contract for AI-builder UI

<!-- Added in round 3 (C3-13): the UI must surface specific failure modes, not "preview broken". -->

Every error response from the preview surface includes a stable machine-readable `code`:

| `code` | HTTP status | Meaning | Suggested UI message |
|---|---|---|---|
| `agent_unreachable` | 502 | Sandbox VM not responding (likely crashed or rebooting). | "Sandbox is restarting; preview will resume shortly." |
| `port_not_listening` | 503 | Loopback `connect()` refused — user app not bound to that port. | "No server listening on port {port}. Did your dev server crash?" |
| `agent_capability_missing` | 501 | Older agent without `proxy.http-v1` / `proxy.ws-v1`. | "Preview requires agent ≥ X.Y. Click here to restart sandbox." |
| `circuit_open` | 503 | Per-(sandbox, port) breaker is open. | "Preview is temporarily unavailable; retrying in {retry_after_ms}ms." |
| `rate_limited` | 429 | Per-IP / per-sandbox / per-creator quota hit. | "Too many requests. Try again in {retry_after} seconds." |
| `expired` | 401 | Share token past `exp`. | "Share link expired. Ask the creator for a new link." |
| `revoked` | 401 | `secret_version` no longer honoured. | "Share link was revoked." |
| `wrong_audience` | 401 | `aud != "preview"`. | "Invalid preview link." |
| `scope_forbidden` | 403 | RO token + non-GET. | "Read-only preview link cannot perform this action." |
| `body_too_large` | 413 | > 100 MiB upload. | "File too large; max 100 MiB. Use the storage SDK for large files." |
| `port_not_allowed` | 400 | Port in deny-list. | "Port {port} cannot be exposed via preview." |
| `host_misdirected` | 421 | Host header doesn't match the canonical preview format. | (no UI; non-creator-reachable case) |
| `agent_drain` | 503 | Agent shutting down. | "Sandbox is shutting down." |
| `controller_drain` | 503 | Controller shutting down. | "Platform is updating; reconnecting…" |

UI error rendering is the AI-builder's responsibility but the codes are stable contract.

### XI.5 Rollout / rollback

<!-- Added in round 3 (C3-8): without a rollout plan, deploying controller v(N+1) before the agent fleet supports it is a fleet-wide outage. -->

- **Feature flag:** `SANDBOX_PREVIEW_FEATURE=v4` enables the preview surface. Default off until phase 4 GA.
- **Kill switch:** `SANDBOX_PREVIEW_DISABLED=1` (set on the controller, requires restart) flips every preview route to 503 with `code: "preview_disabled"`. Emergency use only.
- **Per-environment rollout:** `dev` (1% creator opt-in) → `staging` (10%) → `prod canary` (1% sandboxes) → `prod GA` (100%). Each step holds for 1 week with the golden-signals dashboard green.
- **Rollback:** flip `SANDBOX_PREVIEW_FEATURE=v3` (turn the surface off; existing sealed records remain on disk untouched) and restart. Sandboxes mid-flight see preview 503; creators retry; published apps unaffected. Full revert = `git revert` of the phase commits + `SANDBOX_PERSIST_AUTH=0` (drop sealed-record dependency).

### XI.6 Capacity planning

<!-- Added in round 3 (C3-9), reworked in round 4 (C4-3): every WS holds 3 fds (browser→ctrl, ctrl→agent, agent→user-app) and ~10 MiB of kernel TCP buffer at default tunings; previous "100k WS" cap was unbacked. -->

Per controller, default Linux file-descriptor budget (`ulimit -n`) is 1 048 576. **Each WebSocket holds 3 fds end-to-end** (browser→controller, controller→agent, agent→user-app); HTTP requests free fds at end-of-request. Allocations:

| Bucket | Reservation | Notes |
|---|---|---|
| Preview WebSocket connections (default) | 30 000 WS × 3 = 90 000 fds | `SANDBOX_PREVIEW_MAX_WS = 30_000` default; documented tuning recipe below to scale up. |
| Preview HTTP backend pool (controller→agent) | 32 768 fds | Per-sandbox ≤ 64; global cap `SANDBOX_PREVIEW_BACKEND_POOL_MAX = 32_768`. |
| Preview HTTP frontend in-flight | ~50 000 fds | Bursty; bounded by the per-sandbox 200 RPS × p50 200 ms in-flight window. |
| Control-plane internal | 50 000 | Existing reservation. |
| OS + headroom | rest (~700 k) | |

**Per-WS memory.** At Linux kernel defaults (`net.ipv4.tcp_rmem = "4096 131072 6291456"`, `tcp_wmem = "4096 16384 4194304"`), a single TCP socket high-watermarks at ~10 MiB of kernel buffer (rmem 6 MiB + wmem 4 MiB). 30 000 WS × 3 fds = 90 000 sockets × 10 MiB high-watermark = 900 GiB *virtual*; actual resident is far smaller (kernel allocates lazily based on traffic). For an HMR workload (small frames, idle most of the time) a per-socket working set of ~64 KiB is realistic ⇒ ~5.5 GiB resident at saturation.

**Tuning recipe to scale to 100k WS** (operator runbook):

```sh
# /etc/sysctl.d/90-zeroship-preview.conf
net.ipv4.tcp_rmem = 4096  16384  1048576    # cap rmem high-watermark at 1 MiB
net.ipv4.tcp_wmem = 4096  16384  1048576    # same for wmem
net.core.somaxconn = 65535
net.ipv4.tcp_max_syn_backlog = 65535
fs.file-max = 4194304
# /etc/security/limits.conf
*  soft  nofile  4194304
*  hard  nofile  4194304
```

With this tuning, 100k WS × 3 fds = 300k fds (within the bumped fs.file-max) and ~2 MiB peak per socket = 600 GiB virtual / ~10 GiB resident at saturation. Document on the controller VM sizing.

**Memory.** 64 × 100 MiB inflight large = 6.4 GiB peak; 1 GiB controller heap (Rust binary + working set); 200 MiB sealed-record cache (100k sandboxes × ~2 KiB realistic overhead); ~10 GiB resident WS kernel buffers at full tuning. **Recommend controller VM ≥ 32 GiB** at full-tuning capacity (was 16 GiB in v3 — undersized).

**Bandwidth.** 10 GbE = 1.25 GB/s. HMR is bursty but small (< 1 KB/frame); large uploads are bandwidth-bound at 100 MiB / 100 ms = 1 GB/s ⇒ one upload near-saturates a controller. Per-sandbox cap of 2 inflight-large keeps this bounded; 64 global = 8 GB/s aggregate burst, within 10 GbE for a brief window.

**Threshold to add a controller:** any of (a) fd usage > 70% of budget for 1 h, (b) memory > 70% of budget for 1 h, (c) p99 latency > 1 s for 4 h, (d) `proxy_inflight_large` saturation > 90% for 30 min, (e) WS active > 70% of `SANDBOX_PREVIEW_MAX_WS` for 1 h.

---

## XII. Latency budget

<!-- Added in round 4 (C4-2, C4-4): the v3 doc said "1 ms p50 in-region" without derivation; this section makes the math explicit. -->

End-to-end p50 latency per request type, with components broken out. Numbers are budget targets (load-test gates in § IV.4); regressions trigger a perf review.

### XII.1 Small HTTP request (HMR poll, asset fetch < 64 KiB)

| Component | Cost (p50) | Notes |
|---|---|---|
| Browser TLS handshake (resumed) | 5 ms | First request only; amortized over keep-alive. |
| Browser → controller TCP RTT (in-region) | 0.5 ms | |
| Host parse + AuthN (cookie) + AuthZ + sandbox lookup | 50 µs | Hot path; HashMap + JWT verify. |
| Hash empty/small body + Ed25519 sign | 100 µs | Body < 64 KiB hashes inline in < 1 ms. |
| Controller → agent connect (pooled) | 10 µs | Reuse from pool. |
| Controller → agent network (loopback or 10G) | 0.2–0.5 ms | |
| Agent verify_signed (Ed25519 + nonce LRU) | 200 µs | |
| Agent → user-app loopback connect | 50 µs | Fresh per request. |
| User-app TTFB (Vite asset) | 2–5 ms | Workload-dependent. |
| Return path (mirrors above) | ~0.5 ms | Streaming; no buffering. |
| **End-to-end p50** | **~10 ms** | Plus TLS handshake on first hit. |

Anything beyond ~25 ms p99 for small requests indicates a regression — either profile the controller (signing? lookup?) or check the user-app TTFB.

### XII.2 WebSocket Upgrade + first frame (Vite HMR)

| Component | Cost (p50) | Notes |
|---|---|---|
| Browser TLS handshake (resumed) | 5 ms | Once per WS session. |
| Upgrade signed-canonical (`v1.1-ws`) sign + verify | 350 µs | Ed25519 + sec-websocket-key sha256. |
| Controller → agent network + agent → user-app | ~1 ms | |
| User-app 101 response | 1 ms | Vite is fast. |
| Splice setup (compio) | 50 µs | |
| **Time to first WS frame** | **~10 ms** | |

After Upgrade, frames flow with no per-frame signing — pure proxy latency dominates (~0.5 ms agent + ~0.2 ms ctrl).

### XII.3 100 MiB upload (PUT)

| Component | Cost (p50) | Notes |
|---|---|---|
| Browser → controller body upload | 800 ms | 100 MiB at 1 Gbps. |
| Controller buffer + SHA-256 (offloaded) | 200 ms | Blocking-pool worker; runs in parallel with ingress. |
| Controller → agent body upload | 800 ms | Same bandwidth assumption. |
| Agent verify hash + buffer | 200 ms | |
| Agent → user-app loopback upload | 100 ms | Loopback is faster (~10 GB/s memcpy-bound). |
| User-app processing | varies | |
| **Time to user-app first byte (TTFB)** | **~2.1 s** | 100 MiB upload is NOT streaming-friendly — see C4-2. |

For workloads needing large uploads, use `zeroship.storage` presigned URLs (R-2) — the platform proxies a small redirect, the actual bytes go direct to object storage. Streaming-mode (`auth.ed25519-v2-streaming`) is the long-term fix; tracked in Q-16.

### XII.4 Retry budget

The controller retries failed forwards to the agent up to **3 times** with exponential backoff: 100 ms / 300 ms / 900 ms (jittered ±20%). Total worst-case ≤ 1.5 s before surfacing 502 to the client. Each retry mints a fresh `(ts, nonce, signature)` (round-2 C2-4). Documented so SREs know upper-bound latency on a flaky agent.

---

## Appendix B-G: glossary

<!-- Added in round 5 (C5-11): some acronyms benefit from a single-place expansion for newer contributors. -->

- **AEAD** — Authenticated Encryption with Associated Data; XChaCha20-Poly1305 here.
- **ALPN** — Application-Layer Protocol Negotiation; TLS extension that lets client + server agree on `h2` or `http/1.1`.
- **Canonical (signing)** — the byte-string fed into Ed25519 sign/verify; built from `(method, path, ts, nonce, body-hash)` plus a domain-separator tag in v1.1+.
- **CHWBL** — Consistent Hash With Bounded Loads; the gateway's worker-routing algorithm. Not used by the preview surface; mentioned only for context.
- **CT-monitoring** — Certificate Transparency log monitoring; alerts on unauthorized cert issuance for a domain.
- **h2** — HTTP/2; multiplexed binary protocol over TLS.
- **h2c** — HTTP/2 over cleartext; HTTP/1.1-Upgrade dance to h2 without TLS. Not used here.
- **HMR** — Hot Module Replacement; Vite's mechanism for live-reloading edited modules without a full page refresh.
- **HPACK** — HTTP/2 header compression algorithm.
- **HSTS** — HTTP Strict Transport Security; tells browsers to use HTTPS only.
- **OCSP** — Online Certificate Status Protocol; mechanism for checking cert revocation. *Stapling* embeds the response in the TLS handshake.
- **typed_id** — `<prefix>_<base62-uuidv7>`; e.g. `sbx_01HF…`.
- **WPT** — Web Platform Tests; spec-conformance test suite vendored at `tests/wpt`.

## Appendix C: cross-references

- Backend enum: `crates/sandbox/src/backend/mod.rs:117-255`
- Nomad-CH backend (network model): `crates/sandbox/src/backend/nomad_ch.rs:38-62`
- Agent handlers (existing): `crates/sandbox-agent/src/handlers.rs:1-23`
- Agent signature verification: `crates/sandbox-agent/src/sig.rs:1-69`
- Capability list: `crates/sandbox-agent/src/version.rs:35-45`
- Agent port constant: `crates/sandbox-agent/src/lib.rs` (`AGENT_PORT = 7777`)
- Controller registry: `crates/sandbox/src/registry.rs`
- Controller handlers: `crates/sandbox/src/handlers.rs`
- Operator runbook (network model): `docs/runbooks/sandbox-nomad-ch.md:36-43`
- AGENTS.md invariants: `AGENTS.md` (zero tokio, gateway is dumb, native primitives kernel)

— end of design —
