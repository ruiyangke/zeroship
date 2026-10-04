# zeroship-workflow-client

Authenticated workflow metadata transport for native hosts. Worker clients
validate delivery and receipt identity; Control clients validate schedule and
management receipts. Both mint fresh service assertions, bound requests and
responses, and use compio HTTP connections.

This crate depends on shared wire types, not the customer engine, manager, ORM
or V8. Remote origins require HTTPS, with two exceptions: literal loopback, and
exact origins an operator lists in `plaintext_peers`, which is empty by default.
Mutation callers retain their logical request identity across uncertain replies.

`WorkerCoordinator` is an enrolled worker's one client: it claims jobs,
heartbeats live grants, settles creator-committed executions, gives deliveries
back, and carries the creator-facing run calls and task payload operations.
`claim_jobs` sends a `ClaimJobs` naming no app - the free execution slots, the
wait the caller will give the reply, the cursor the previous reply returned and
the apps this host failed to prepare recently - and refuses a reply holding more
deliveries than it asked for, a delivery naming another worker, or a journal
acceptance that does not match the operation claimed. The service decides the
apps from the zone frozen on the instance row that signed the request.
`LeasedJob` holds local monotonic deadlines for both the lease and the attempt,
derived from the remaining durations the reply carried and charging the full
request exchange. A late heartbeat cannot revive an expired grant. The executor
must also enforce its original hard execution deadline. Exact settlement receipts
remain retryable after local lease expiry.

`settle_execution` reports an execution for the journal to commit and
`settle_committed` settles a delivery whose job the journal already holds a
receipt for; neither carries an outcome or successors. `release_job` gives a
delivery back with the journal task held under it, and `give_back_job` gives back
one the journal never accepted execution for; both name a closed
`GiveBackReason`, and the service returns the row to the queue as that reason
says: after a back-off that grows with each consecutive back-off for an app that
could not be prepared, at once with the attempt counted for an interrupted
attempt, and at once with nothing counted for a delivery the holder began
nothing of. `job_receipt` reads what an attempt
of a job committed, for the holder recovering an uncertain settlement.

`ControlCoordinator::register_schedules` prepares input-free metadata and checks
the complete accepted declaration, including the app's execution zone it names.
`activate_schedules` checks the returned job's app, deployment, operation and
activation revision. These publication methods require the exact Control service
signer. Callers preserve the original command and revision after an uncertain
reply; an activation receipt means durable manager acceptance, while creator
readiness is a separate job outcome.

`disable_schedules` checks the exact accepted app and revision. A historical
disable receipt remains valid after restore without disabling the newer revision.
This operation acknowledges calendar publication fencing, not creator policy or
executor quiescence.

Run `cargo test -p zeroship-workflow-client` for wire, cancellation, timeout and
TLS contracts.
