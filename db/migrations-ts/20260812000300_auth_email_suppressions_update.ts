import { grant } from "zero-migrate";

// Bounce and complaint suppression never recorded anything.
//
// crates/mailer/src/suppressions.rs:57 (the whole file is production -- it has
// no #[cfg(test)] module) is
//   INSERT INTO zeroship.email_suppressions (email, reason, provider_msg)
//   VALUES ($1::citext,$2,$3)
//   ON CONFLICT (email) DO UPDATE SET reason = ..., provider_msg = ...
// and it is reached from crates/auth/src/ui/webhooks.rs:118 and :164, the
// provider bounce/complaint webhook handlers, via suppressions::add(). Both call
// sites swallow the error into a `tracing` line reading "suppression add
// failed", so every webhook reported success while the suppression list stayed
// empty. The platform therefore kept sending to bouncing and complaining
// addresses.
//
// zeroship_auth held INSERT but not UPDATE, and PostgreSQL requires UPDATE to
// PLAN an ON CONFLICT DO UPDATE, so the statement failed on every call rather
// than only on a repeat suppression of the same address.
//
// WHICH ROLE RUNS IT was settled by elimination, not assumption: the mailer is
// referenced from both auth's and control's main.rs, but on the migrated schema
//   zeroship_auth    ins=t upd=f
//   zeroship_control ins=f upd=f
//   zeroship_gateway ins=f upd=f
//   zeroship_worker  ins=f upd=f
// so auth is the only role that could execute the INSERT half at all.
//
// MEASURED 2026-08-12 as zeroship_auth, one variable between the arms:
//   INSERT ... VALUES (...)                        -> INSERT 0 1
//   INSERT ... VALUES (...) ON CONFLICT DO UPDATE  -> ERROR: permission denied
//
// Scoped to UPDATE on this one table. Whether alias-level suppression should
// exist at all is a separate open question (#126) and is NOT decided here; this
// only makes the suppression the code already tries to write actually land.
export default {
  name: "auth_email_suppressions_update",
  schema() {
    grant({
      privileges: ["update"],
      on: { kind: "table", schema: "zeroship", names: ["email_suppressions"] },
      to: ["zeroship_auth"],
    });
  },
};
