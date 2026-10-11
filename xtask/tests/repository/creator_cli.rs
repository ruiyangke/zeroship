//! The creator CLI's closure carries nothing only the services need.
//!
//! The `zeroship` binary is what creators install on Linux, macOS and Windows,
//! so a native library only a service uses is a build that can break on a
//! creator's machine for nothing the CLI does. The usage-event stream client is
//! the one such library in reach: librdkafka, behind `zeroship-metering`'s
//! `outbox` feature, which only the services that publish usage enable.

use crate::architecture::repo;
use std::collections::BTreeSet;
use std::process::Command;

/// The package names in `package`'s normal and build closure on every target,
/// as cargo resolves that package alone, the way its binary is built.
fn shipped_closure(package: &str) -> BTreeSet<String> {
    let output = Command::new(env!("CARGO"))
        .current_dir(repo::root())
        .args([
            "tree", "--locked", "-p", package, "-e", "normal,build", "--target", "all",
            "--prefix", "none", "--format", "{p}", "--color", "never",
        ])
        .output()
        .expect("run cargo tree");
    assert!(
        output.status.success(),
        "cargo tree -p {package} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("cargo tree prints UTF-8")
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_creator_cli_links_no_stream_client() {
    let cli = shipped_closure("zeroship-cli");
    assert!(
        cli.contains("zeroship-runtime") && cli.contains("zeroship-metering"),
        "the scan must see the CLI's runtime and the meter it records into: {cli:?}"
    );
    for server_only in ["rdkafka", "rdkafka-sys", "zeroship-stream"] {
        assert!(
            !cli.contains(server_only),
            "the creator CLI's closure reaches {server_only}; `cargo tree -p zeroship-cli -i \
             {server_only}` names the edge"
        );
    }

    // The control: a service that publishes usage does reach the client, so
    // the scan can see it when it is there.
    let worker = shipped_closure("zeroship-worker");
    assert!(
        worker.contains("rdkafka-sys") && worker.contains("zeroship-stream"),
        "the worker publishes usage, so its closure must show the stream client"
    );
}
