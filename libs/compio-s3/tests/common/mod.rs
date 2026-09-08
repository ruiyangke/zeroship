//! The refusals this crate's container-backed test uses in place of a skip.
//!
//! THIS FILE USED TO BE THE SKIP ANNOUNCER, and its header argued for staying
//! one. The argument was: what `minio_smoke.rs` reports absent is a Docker
//! daemon and a MinIO container, not a backend anything provisions ahead of the
//! run, so "a skip here is an honest absence, not a hidden pass". That
//! reasoning has been overruled by an operator decision covering every backend,
//! Docker and MinIO named among them. A test that cannot reach what it needs
//! now FAILS and says what to run.
//!
//! The argument was wrong on its own terms, and it is worth recording why
//! rather than just deleting it. An announcement is only "honest" while
//! something reads it. What read it was a shell census over a suite log, so the
//! honesty was conditional on running this crate through that suite - and the
//! command in this file's own header, `cargo test -p compio-s3 --test
//! minio_smoke`, is not that. Run the documented way, the announcement went to
//! a terminal nobody was diffing and the target reported a pass.
//!
//! NO `env` MODULE, and its absence is deliberate. This crate had a sealed
//! `tests/common/env.rs` whose enum carried exactly ONE variant,
//! `ZEROSHIP_REQUIRE_LIVE_BACKENDS`. With that flag deleted the enum had no
//! variants left, so the sealed accessor was reading nothing; the file went
//! rather than lingering as an empty frame that the next name to come along
//! would be dropped into unexamined. `libs/compio-s3` reads no environment at
//! all, in tests or in production, which is the strongest form of the rule
//! `crates/zeroship-core/tests/config_env_access_gate.rs` enforces - and it is
//! the reason there is no variable here to re-enable a skip with.

/// Fail the calling test because the Docker daemon cannot be reached.
///
/// The message answers WHICH dependency, HOW it was probed, and WHAT to run.
/// There is no provisioning script to name: this crate's test starts its own
/// container, so what is missing is the daemon itself and not a service some
/// other script would have brought up.
#[track_caller]
pub fn docker_unavailable() -> ! {
    panic!(
        "Docker is unavailable, and this test requires it.\n\
         \n\
         \x20 dependency: the Docker daemon\n\
         \x20 probed by:  `docker info`, which did not exit 0\n\
         \n\
         This test starts its own MinIO container, so no provisioning script\n\
         stands one up for it and there is nothing in tests/ to run first.\n\
         Install Docker and start the daemon, then check the probe by hand:\n\
         \x20 docker info\n\
         \n\
         There is no environment variable that makes this a skip. A backend\n\
         this suite cannot reach is a failed run, not a green one."
    )
}

/// Fail the calling test because MinIO itself could not be brought up.
///
/// `stage` names the step that failed, so the two ways this goes wrong stay
/// distinguishable: Docker answered but refused to start the container, versus
/// the container started and never became ready. They have different remedies
/// and a single message would send the reader to the wrong one.
#[track_caller]
pub fn minio_unavailable(stage: &str, container: &str, port: u16) -> ! {
    panic!(
        "MinIO could not be started, and this test requires it.\n\
         \n\
         \x20 backend: MinIO (started by this test, not by a provisioning script)\n\
         \x20 stage:   {stage}\n\
         \x20 container: {container}\n\
         \x20 port:      {port}\n\
         \n\
         Docker answered, so the daemon is not the problem. Look at what the\n\
         container did:\n\
         \x20 docker logs {container}\n\
         \x20 docker rm -f {container}      # clear a leftover from a killed run\n\
         \x20 ss -lptn 'sport = :{port}'    # something else may hold the port\n\
         \x20 docker pull minio/minio       # the image may not be cached\n\
         \n\
         There is no environment variable that makes this a skip. A backend\n\
         this suite cannot reach is a failed run, not a green one."
    )
}
