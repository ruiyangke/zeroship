//! The PostgreSQL PROJECT LOCK: who holds it, what a peer sees, and what a
//! cancelled acquisition leaves behind.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.
//!
//! `pg_project_lock_grant` aims a peer release at the instant a short `lock_timeout`
//! fires. That window is measured against a wall clock, so how much other work hits
//! the shared server changes how often an attempt lands in it, which the test's
//! control arm measures rather than assumes. The budget is a `lock_timeout`, which
//! only a lock wait can reach, so a busy server cannot cancel the harness's own
//! statements between attempts.


mod pg_project_lock_grant;
mod pg_status_project_lock;
