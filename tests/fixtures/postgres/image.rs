use testcontainers::{runners::SyncBuilder, GenericBuildableImage, GenericImage};

pub fn build() -> Result<GenericImage, Box<dyn std::error::Error + Send + Sync>> {
    Ok(
        GenericBuildableImage::new("zeroship-data-tests-postgres", "local")
            .with_dockerfile_string(include_str!("Dockerfile"))
            .build_image()?,
    )
}
