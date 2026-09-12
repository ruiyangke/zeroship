use crate::{cargo, checked, root, Result};
use std::process::Command;

pub fn run() -> Result<()> {
    checked(
        cargo().args([
            "test",
            "-p",
            "zeroship-storage",
            "-p",
            "zeroship-storage-v8",
            "-p",
            "compio-s3",
            "--all-features",
        ]),
        "Rust storage, V8 binding and S3 driver tests",
    )?;
    for example in ["storage-probe", "storage-gallery"] {
        checked(
            Command::new("pnpm")
                .current_dir(root().join("examples").join(example))
                .arg("test"),
            &format!("{example} Vitest and Playwright tests"),
        )?;
    }
    Ok(())
}
