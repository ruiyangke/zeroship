# Worker join signers runbook

How to provision the trust anchor a worker joins under, mint a token that lets
a worker join, and revoke a signer. The design is the "Enrollment bootstrap
and revocation" subsection of `docs/proposals/2026-09-11-workflow-worker.md`.

## What a join signer is

A worker joins with a signed JOIN TOKEN it was handed, not with a standing
credential of its own. The trust anchor is a set of JOIN SIGNERS Control
records in advance: an Ed25519 keypair with a `wjs_` id, and the execution
zones that signer may mint for. One signer covers every deployment unit in its
zones, so bringing up more workers is not a Control-side operation, and the
signing key sits with whoever decides a worker should exist rather than on the
machine that runs creator code.

- The signer's **private** half stays with the operator, or - on a single-host
  deployment where nobody is present when a container restarts - with Control
  itself, configured as this deployment's own minter.
- Control records the signer's **public** key and id in
  `zeroship.worker_join_signers`, and its permitted zones in
  `zeroship.worker_join_signer_zones`, imported together at every boot from
  the **join signer import file**.
- No worker holds a `svc/worker` role key, and no worker holds signer material
  either. A worker draws a fresh instance keypair in memory at boot, presents
  a join token as a bearer plus its public key, its listening port and a
  signature over all three, and from then on mints only under its own instance
  identity. The join token authenticates the join request and nothing else.

| Setting | Environment name | Holds |
| --- | --- | --- |
| `control.join_signers_file` | `ZEROSHIP_CONTROL_JOIN_SIGNERS_FILE` | The join signer import file. Public keys only. |
| `control.join_token_signer_file` | `ZEROSHIP_CONTROL_JOIN_TOKEN_SIGNER_FILE` | The signer credential Control mints its own zone's tokens with (single-host deployments only). |
| `control.join_token_file` | `ZEROSHIP_CONTROL_JOIN_TOKEN_FILE` | Where Control writes the token it mints, for the worker containers to read. |
| `control.join_token_zone` | `ZEROSHIP_CONTROL_JOIN_TOKEN_ZONE` | The execution zone Control's minted tokens admit into. |
| `worker.join_token_file` | `ZEROSHIP_WORKER_JOIN_TOKEN_FILE` | The join token this worker presents at boot. Mode 0600, or the worker refuses to start. |

Joining also needs the enrolment envelope: `control.worker_enrolment_networks`
and `control.worker_enrolment_ports` must cover every worker's advertised
address and listening port. A join signer does not widen it; the envelope and
the signer are independent checks and both must pass.

The two documents:

```json
{ "signer_id": "wjs_...",
  "private_key": "-----BEGIN PRIVATE KEY-----\n...\n-----END PRIVATE KEY-----\n" }
```

```json
{ "signers": [
    { "id": "wjs_...", "zones": ["default"], "public_key": "<base64url raw Ed25519 key>" }
] }
```

`zones` names execution zones (`zeroship.execution_zones.name`). Every
deployment declares the zone `default`.

## What the import does at every Control boot

The import only ever ADDS:

- A signer Control has not recorded is inserted `active`, with a row per
  permitted zone.
- A signer already recorded with the same key and the same zone set is left
  exactly as it is. A **revoked** signer stays revoked, however long its line
  stays in the file.
- A file that disagrees with a recorded signer - its id under another key, its
  key under another id, or a different set of permitted zones - **refuses the
  whole file** and writes nothing. The message names every disagreeing entry.
  A signer's zones are its authority, so widening them is provisioning a new
  signer, never editing a recorded one.
- Recorded signers the file no longer names are left alone. Removing a line
  revokes nothing; `zeroship.rotate_worker_join_signer` does.

A key names exactly one signer for its life. Control logs
`control: join signers imported from config` with what it inserted, left
unchanged, and found still revoked; an empty (default) `control.join_signers_file`
logs that no signer file is configured, and only signers a previous boot
already recorded can admit a worker.

## Single host: Control mints its own tokens

`zeroship dev init` provisions the OPERATOR SIGNER and nothing worker-side: it
writes `join-signer.json` (the private credential, mode 0600) and
`join-signers.json` (the public import document, naming that signer for the
`default` zone). `deploy/compose/docker-compose.yml` mounts both into `control`
and configures `control.join_token_signer_file` / `control.join_token_file` /
`control.join_token_zone` so Control mints its own zone's join tokens at
startup and again on a rotation interval, before it binds. Every `worker`
replica mounts the resulting `join-tokens` volume read-only and reads the
CURRENT token at boot from `worker.join_token_file`, so a container restarted
long after provisioning still gets a token minted minutes ago rather than one
minted at install time. `--scale worker=N` replicas each consume one use of
whatever token is currently in the volume; a fleet large enough to exhaust a
token's use budget between rotations gets the next one minutes later.
`deploy/scripts/deploy-remote.sh` runs the image's own `zeroship dev init` on
the host, so a server deploy needs no extra step.

Three related quantities hold rotation together, named rather than quoted
here because their values can move: `ROTATION_INTERVAL`, `TOKEN_TTL` and
`BOOT_MARGIN` in `crates/zeroship-control/src/join_minter.rs`. The TTL must
exceed the rotation interval by at least the boot margin, or a worker that
reads the file an instant before a rotation presents an expired token; a test
beside the constants pins that relationship rather than the numbers. Minting
is a leased role elected with a database advisory lock, so exactly one Control
replica rotates the file and the others stand by.

**What the shared token file is, stated plainly.** It is a bearer artifact:
whoever can read that volume can join a worker in that zone, for the token's
remaining life, up to the uses it has left. Rotation bounds the window and the
use cap bounds the blast radius, but the trust boundary is "whatever can read
the volume", which is not the same boundary as "the worker". The stronger
anchor - Control reading the peer's credentials off a Unix domain socket and
minting nothing at all - removes the artifact rather than shortening its life;
it is a named follow-up, not something this deployment does today.

`zeroship dev init` never rotates the signer credential. It adds this
deployment's entry to an existing import file and keeps every other entry. If
the import file already names this deployment's signer id under a different
key, it refuses and changes nothing.

## Multi-host: mint a token per provisioning

A multi-host deployment leaves `control.join_token_signer_file` /
`control.join_token_file` / `control.join_token_zone` unset, so its Control
verifies join tokens without holding any key that can mint one. The operator
mints instead, per provisioning, with the signer credential held wherever it
was generated:

```bash
zeroship join-token --credential=/path/to/join-signer.json \
  --zone=default --ttl=SECONDS --uses=N --audience=URI
```

`--zone`, `--ttl`, `--uses` and `--audience` are all optional; the token
defaults to a short lifetime measured in minutes rather than days
(`DEFAULT_JOIN_TOKEN_TTL_SECONDS` and `DEFAULT_JOIN_TOKEN_USES` in
`crates/zeroship-cli/src/dev.rs`), because an operator running this is
present and what is left over should expire before anyone could carry it
anywhere. A scaled service shares ONE token and each worker consumes a use of
its own, so `--uses` is "how many workers am I bringing up now", not a fleet
size. The token is printed on stdout and nothing else, so
`zeroship join-token ... > token` is the whole of the plumbing; every
diagnostic (which signer, which zone, how many uses, how long it is valid)
goes to stderr.

Distribute the printed token to the new workers by writing it to the path each
one reads as `worker.join_token_file`, mode 0600.

## Bringing up more workers

One signer already covers every deployment unit in its zones, so adding
workers is not a Control-side operation at all: mint a token with enough uses
(or, on a single host, let the next scheduled rotation supply one) and point
the new workers' `ZEROSHIP_WORKER_JOIN_TOKEN_FILE` at it. Nothing needs to be
imported, and Control needs no restart.

There is no separate command to mint an ADDITIONAL signer; the trust anchor is
written once by `zeroship dev init`. A deployment that genuinely needs a
second signer - a second, independently rotatable trust anchor for the same
zone - runs `zeroship dev init` for that signer on its own, then merges the
resulting entry from its `join-signers.json` into this deployment's import
file: the import format is a JSON array of entries and the import itself is
additive, so adding one more entry and restarting Control inserts only what is
new.

## Check what Control recorded

```sql
SELECT id, status, created_at FROM zeroship.worker_join_signers;
SELECT sz.signer_id, z.name
  FROM zeroship.worker_join_signer_zones sz
  JOIN zeroship.execution_zones z ON z.id = sz.execution_zone_id;
SELECT id, join_signer_id, status, advertise_host, advertise_port,
       expires_at, registered_at
  FROM zeroship.worker_instances WHERE join_signer_id = 'wjs_...';
```

## Revoke a signer

Revocation is an explicit operator database operation. Neither function has a
runtime `EXECUTE` grant, so run them as the platform owner or the superuser.
The two verbs do NOT imply each other:

### Hygiene: rotate

```bash
docker compose -f deploy/compose/docker-compose.yml exec postgres \
  psql -U postgres -d zeroship \
  -c "SELECT zeroship.rotate_worker_join_signer('wjs_...')"
```

Marks the signer `revoked`. No further token can be minted under it and every
join naming it is refused `signer_unknown`; a token already minted under it
dies at its own expiry, which is what a short `--ttl` buys and a long one gives
up. The fleet this signer already admitted KEEPS RUNNING - instances do not
depend on their signer after they join, only on their own renewed lease - so
this is the path for a signer that is merely old or being replaced on a
schedule, not one you believe has leaked.

### Incident: purge

```bash
docker compose -f deploy/compose/docker-compose.yml exec postgres \
  psql -U postgres -d zeroship \
  -c "SELECT zeroship.purge_worker_join_signer('wjs_...')"
```

Does everything rotate does, AND in the same transaction retires (`status =
'gone'`) every instance that signer admitted. Use this when the signer's
private key is believed to have leaked: an attacker who could still mint under
it would otherwise keep admitting new workers while you retired the old ones
one at a time.

What follows either verb: Control refuses the signer on its next join check,
because the resolving query filters on `status = 'active'`. Leases already
issued keep their original deadlines. After a rotate, the recorded signer id
on each admitted instance is what makes "everything this signer admitted"
enumerable later, including for a purge issued afterward against the same id.

## Retiring one instance

A worker stopped gracefully retires its own instance through `POST
/internal/workers/retire`, authenticated by its own instance key, once its
server has drained: the row becomes `gone` and its key stops authenticating
immediately. A repeated call cannot succeed twice. The compose file gives
`worker` a `stop_grace_period` that covers `worker.shutdown_timeout` and the
retirement call; give the worker the same allowance under any other
orchestrator, and raise it together with `worker.shutdown_timeout`.

## Instances expire, and a worker renews its own

An admitted instance identity is not live forever on its own say-so: Control
refuses an expired instance exactly as it refuses a retired or revoked one.
A worker renews through `POST /internal/workers/renew`, authenticated by its
OWN instance key and nothing else - no join token is involved, and none would
help, because demanding a fresh one would defeat the point of a use-capped
token. Renewal extends the expiry only for an instance that is still active
and not already expired; a lapsed identity is terminal the same way a retired
one is; a worker in that state must rejoin, which needs a token, rather than
reviving its old row. The renewal interval is derived from the lease rather
than chosen beside it (`INSTANCE_LEASE_TTL` divided by
`INSTANCE_RENEWALS_PER_LEASE` in `crates/zeroship-core/src/worker_join.rs`),
so several renewal attempts fit inside one lease and a failed one is retried
well before the identity lapses.

Nothing observes liveness to make either ending happen: a crashed worker's row
simply stops satisfying Control's read once its lease runs out, the same way a
retired or revoked one already did. Retired, revoked-admitter and lapsed rows
are all kept, never deleted, for attribution.

## Restoring a Control database snapshot

A restored snapshot can hold signers and instances as `active` that were
purged after the snapshot was taken - rotation alone changes nothing an
instance depends on, but a purge's retirement is exactly the kind of write a
restore can lose. After a restore, replay every `purge_worker_join_signer`
issued since the snapshot before workers reconnect. Instances an ordinary
lease expiry would have retired since the snapshot need no replay: the clock
that governs them is `expires_at`, which the restore brings back honestly.

## Troubleshooting

- **Control refuses to start, naming the join signers file.** An entry
  disagrees with a recorded signer, the file is malformed, or it names a zone
  this deployment does not declare. Fix the entry named in the message. A
  changed key is a new signer with a new id.
- **The worker refuses to start, naming `worker.join_token_file`.** The token
  file is missing, not mode 0600, or malformed.
- **Join answers 403 `signer_unknown`.** No active signer resolves for the
  id the token claims - it was never imported, was revoked or purged, or
  Control has not been restarted since it was added.
- **Join answers 403 `signer_inactive`.** The signer was active when the token
  was verified but was revoked or purged in the moment before the instance
  insert; rare, and the fix is the same as `signer_unknown` - mint under a
  currently active signer.
- **Join answers 403 `zone_not_permitted` or `zone_not_permitted_by_registry`.**
  The token's `zone` claim is not one this signer may mint for.
- **Join answers 403 `token_exhausted`.** The token's `uses` are spent. Mint a
  new one, or wait for the single-host minter's next rotation.
- **Join answers 403 with a `token_*` reason** (`token_malformed`,
  `token_signer_malformed`, `token_signature`, `token_audience`,
  `token_expired`, `token_lifetime`, `token_id`, `token_zone`, `token_uses`,
  `token_confirmation`). The token itself did not verify; reissue it rather
  than retrying the same string.
- **Join answers 403 `proof_invalid` or `confirmation_mismatch`.** The
  request was not signed by the public key it presented, or the presented key
  is not the one a `cnf`-carrying token named in advance.
- **Join answers 409 `public_key_conflict`.** That public key already joined
  under a different signer or token.
- **Join answers 403 or 503 with an address reason**
  (`peer_outside_envelope`, `port_outside_envelope`, `envelope_unset`,
  `proxy_fronted`, `peer_address_unobservable`, `peer_is_unspecified`). The
  enrolment envelope does not cover the worker; the signer is not involved.
- **Renewal answers 403 `instance_not_live`.** The instance is retired,
  purged, or its lease already lapsed. The worker must rejoin with a token.
