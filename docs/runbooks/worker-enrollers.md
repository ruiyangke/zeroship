# Worker enrollers runbook

How to provision the credential a worker enrols with, add a deployment unit,
and revoke one. The design is the "Enrollment bootstrap and revocation"
subsection of `docs/proposals/2026-09-11-workflow-worker.md`.

## What an enroller is

A worker enrols with the credential of its **deployment unit**: a host, or a
pool of workers, in exactly one execution zone. That credential is the unit's
**enroller**, an Ed25519 keypair with a `wen_` id.

- Every worker of the unit mounts the same **enroller credential**
  (`worker-enroller.json`, private). At boot the worker draws a fresh instance
  key in memory, enrols its public half with Control under the enroller's
  issuer `svc/worker-enroller/<wen_id>`, and from then on mints only under
  `svc/worker/<wkr_id>`. The enroller key authenticates the enrolment call and
  nothing else.
- Control records the unit's **public** key, id and zone in
  `zeroship.worker_enrollers`, imported at startup from the **enroller import
  file** (`worker-enrollers.json`, public).
- No worker holds a `svc/worker` role key, and the peer document publishes
  none. Control refuses a `svc/worker` or `svc/worker-enroller` assertion
  minted under the bare role, even when a stale peer document still carries a
  key for it.

| Setting | Environment name | Holds |
| --- | --- | --- |
| `worker.enroller_file` | `ZEROSHIP_WORKER_ENROLLER_FILE` | The unit's enroller credential. Mode 0600, or the worker refuses to start. |
| `control.worker_enrollers_file` | `ZEROSHIP_CONTROL_WORKER_ENROLLERS_FILE` | The enroller import file. Public keys only. |

Enrolment also needs the enrolment envelope: `control.worker_enrolment_networks`
and `control.worker_enrolment_ports` must cover every unit's worker addresses
and listening port. An enroller does not widen it.

The two documents:

```json
{ "enroller_id": "wen_...",
  "private_key": "-----BEGIN PRIVATE KEY-----\n...\n-----END PRIVATE KEY-----\n" }
```

```json
{ "enrollers": [
    { "id": "wen_...", "zone": "default", "public_key": "<base64url raw Ed25519 key>" }
] }
```

`zone` is the NAME of an execution zone (`zeroship.execution_zones.name`).
Every deployment declares the zone `default`.

## What the import does at every Control boot

The import only ever ADDS:

- An enroller Control has not recorded is inserted `active`.
- An enroller already recorded with the same key and zone is left exactly as
  it is. A **revoked** enroller stays revoked, however long its line stays in
  the file.
- A file that disagrees with a recorded enroller - its id under another key or
  zone, or its key under another id - **refuses the boot** and writes nothing.
  The message names every disagreeing entry.
- Removing a line revokes nothing.

A key names exactly one enroller for its life. Re-keying a unit is provisioning
a new enroller, never editing a recorded one.

## Single host (compose, `deploy-remote.sh`)

`zeroship dev init` provisions everything. It creates `worker-enroller.json`
for the host's one unit and writes `worker-enrollers.json` with its entry in
zone `default`. `deploy/compose/docker-compose.yml` mounts the credential into
every `worker` replica and the import file into `control`. `--scale worker=N`
replicas are one unit and share the credential. `deploy/scripts/deploy-remote.sh`
runs the image's own `zeroship dev init` on the host, so a server deploy needs
no extra step.

`zeroship dev init` never rotates the credential. It adds the host's entry to
an existing import file and keeps every other entry. If the import file names
the host's enroller id under a different key, it refuses and changes nothing.

## Add a deployment unit

Run this where the import file lives:

```bash
zeroship dev enroller \
  --credential=/path/to/unit-b/worker-enroller.json \
  --import-file=/path/to/secrets/worker-enrollers.json \
  --zone=default
```

It mints a new enroller, writes its credential (mode 0600, never replacing an
existing file), and adds its entry to the import file, keeping every other
entry. Then:

1. Copy the credential to the unit's host, keep it mode 0600, and point
   `ZEROSHIP_WORKER_ENROLLER_FILE` at it on that unit's workers.
2. Make sure the enrolment envelope covers the unit's worker addresses.
3. Restart Control so it imports the new enroller. Its log line
   `control: worker enrollers imported from config` reports what was inserted.
4. Start the unit's workers.

Check what Control recorded:

```sql
SELECT id, execution_zone_id, status, created_at FROM zeroship.worker_enrollers;
SELECT id, enroller_id, status, advertise_host, advertise_port, registered_at
  FROM zeroship.worker_instances WHERE enroller_id = 'wen_...';
```

## Revoke a unit

Revocation is an explicit operator database operation. No runtime role holds
EXECUTE on the function, so run it as the platform owner or the superuser. On
the compose stack:

```bash
docker compose -f deploy/compose/docker-compose.yml exec postgres \
  psql -U postgres -d zeroship \
  -c "SELECT zeroship.revoke_worker_enroller('wen_...')"
```

In one transaction it marks the enroller `revoked` and every instance it
enrolled that is still `active` as `gone`. It serializes with enrolments in
flight: an enrolment that holds the enroller row finishes first and its
instance is retired too, and one that arrives after is refused.

What follows:

- Control refuses the unit's instances on their next internal request, the
  workflow manager at its next enrolment check, and the CDC relay at its next
  session recheck.
- Every new enrolment under the enroller is refused (`enroller_inactive`).
- Leases and deliveries already issued keep their original deadlines; the
  creator fences stay authoritative. This is an admission fence, not
  quiescence.
- Revocation is terminal. The unit's healthy workers need a NEW enroller:
  provision one, replace the credential on the unit, restart Control, then
  restart the unit's workers.

Revocation targets the unit, never one process: a compromised process still
holds its unit's key, so retiring only its instance would let it enrol again.
Revoke the enroller, and re-provision the unit's healthy peers.

## Rotate a unit's key

1. Provision a new enroller for the unit with `zeroship dev enroller`.
2. Restart Control to import it.
3. Roll the unit's workers onto the new credential.
4. Revoke the old enroller. Doing this last matters: revocation retires every
   worker still enrolled under the old one.

## Instances end on their own

A worker stopped gracefully (SIGTERM) retires its own instance after its server
drains: the row becomes `gone` and its key stops authenticating. A worker that
crashes leaves its row `active` with no process behind it, and so does one the
orchestrator kills before its drain finishes - give the worker a stop grace
period longer than `worker.shutdown_timeout` if retirement on every stop
matters. Nothing reaps those rows, and no liveness observation ever writes
`status`. Retired and revoked rows are kept for attribution.

## Restoring a Control database snapshot

A restored snapshot can hold enrollers and instances as `active` that were
revoked after the snapshot was taken. After a restore, replay every revocation
issued since the snapshot with `zeroship.revoke_worker_enroller` before workers
reconnect.

## Troubleshooting

- **Control refuses to start, naming the enroller file.** An entry disagrees
  with a recorded enroller, the file is malformed, or it names a zone the
  deployment does not declare. Fix the entry named in the message. A changed
  key is a new enroller with a new id.
- **The worker refuses to start, naming `worker.enroller_file`.** The
  credential is missing, not mode 0600, malformed, or its key is published in
  the peer document under another service's issuer.
- **Enrolment answers 401.** No active enroller resolves for the credential's
  id, or the key does not match the one recorded for it. Check that the
  credential's id is in the import file and that Control was restarted after
  it was added.
- **Enrolment answers 403 `enroller_inactive`.** The enroller was revoked.
- **Enrolment answers 403 or 503 with an address reason**
  (`peer_outside_envelope`, `port_outside_envelope`, `envelope_unset`,
  `proxy_fronted`). The enrolment envelope does not cover the worker; the
  enroller is not involved.
