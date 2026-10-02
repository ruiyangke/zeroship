//! The PostgreSQL image the platform and the bare server share, and the readiness
//! waits both owe it.

use std::time::Duration;

use testcontainers::core::WaitFor;
use testcontainers::{
    runners::SyncBuilder, ContainerRequest, GenericBuildableImage, GenericImage, ImageExt,
};

/// The pgvector + PostGIS image the bare server and the platform server run.
///
/// Both fixtures start from one server build, so a test that passes on the bare
/// server describes the same server the platform migration runs against.
///
/// # Errors
/// When the Docker daemon cannot build the image.
pub fn build() -> Result<GenericImage, Box<dyn std::error::Error + Send + Sync>> {
    Ok(
        GenericBuildableImage::new("zeroship-data-tests-postgres", "local")
            .with_dockerfile_string(include_str!("Dockerfile"))
            .build_image()?,
    )
}

/// Add the readiness waits and the startup budget every `PostgreSQL` fixture owes.
///
/// `postgres` logs the init line once the entrypoint has finished and the ready line
/// once the final server accepts connections; waiting on both means a mapped port is
/// answered by the server the test talks to, not the temporary one the entrypoint
/// runs. Call this on the image before the first [`ImageExt`] method converts it to a
/// [`ContainerRequest`], then finish the request.
pub fn await_ready(image: GenericImage) -> ContainerRequest<GenericImage> {
    image
        .with_wait_for(WaitFor::message_on_stdout(
            "PostgreSQL init process complete; ready for start up.",
        ))
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_startup_timeout(Duration::from_secs(120))
}
