//! The test shards: which workspace packages each CI test job runs.
//!
//! This table is the one place shard membership is written. CI's test job reads
//! the shard names from `cargo xtask shards list` as its matrix and runs
//! `cargo xtask test <shard>` for each, and `cargo xtask test <shard>` is the
//! same command locally. `xtask/tests/repository/shards.rs` holds the table to
//! `cargo metadata`, `cargo tree` and the workflow, and CI's `verify` job holds
//! what each shard actually ran to the listing of what it builds, so each test
//! runs exactly once per CI run.
//!
//! A shard owns whole packages: every test target of each, and its doctests.
//! Packages are grouped by the fixtures and the heavy dependencies they share,
//! because each shard builds its own packages on its own runner.

/// One CI test job.
#[derive(Debug)]
pub struct Shard {
    /// The name CI's matrix and `cargo xtask test` use.
    pub name: &'static str,
    /// The workspace packages whose tests this shard runs.
    pub packages: &'static [&'static str],
    /// Features the shard's run enables, as `package/feature`.
    pub features: &'static [&'static str],
    /// Further runs of the same packages under other features.
    pub repeats: &'static [Repeat],
    /// Features of the shard's packages that no run enables, each with why.
    pub unexercised: &'static [Unexercised],
    /// Tests the shard runs before nextest, outside its timeout.
    pub untimed: &'static [Untimed],
    /// What has to exist before the tests start, in order.
    pub prepare: &'static [Prepare],
}

/// A further nextest run of a shard's packages with different features. It
/// repeats tests deliberately, against a different server or transport, which
/// is why it is declared here with its reason rather than being a second shard
/// owning the same packages.
#[derive(Debug)]
pub struct Repeat {
    /// The name its reports carry, `junit-<shard>-<label>.xml`.
    pub label: &'static str,
    /// Features the repeat enables, as `package/feature`.
    pub features: &'static [&'static str],
    /// Why the same tests run again.
    pub reason: &'static str,
}

/// A feature of a shard package that no run of the shard enables.
#[derive(Debug)]
pub struct Unexercised {
    /// The feature, as `package/feature`.
    pub feature: &'static str,
    /// Why no test needs it enabled.
    pub reason: &'static str,
}

/// Tests run by a plain `cargo test` before the shard's nextest run, which
/// excludes them.
///
/// For a test whose first run in a cold checkout is a build rather than a test:
/// its duration then measures the cache, not the code, and would trip the slow
/// timeout that exists to catch a stuck test.
#[derive(Debug)]
pub struct Untimed {
    /// The package that owns the test target.
    pub package: &'static str,
    /// The test target (`[[test]]` name).
    pub target: &'static str,
    /// The libtest name filter selecting the tests, matched as a substring as
    /// both libtest and nextest's `test()` predicate match it.
    pub filter: &'static str,
    /// Why the tests cannot run under the timeout.
    pub reason: &'static str,
}

impl Untimed {
    /// The arguments after `cargo` that run these tests.
    #[must_use]
    pub fn args(&self) -> Vec<&'static str> {
        vec!["test", "-p", self.package, "--test", self.target, self.filter]
    }

    /// The nextest binary id of the target.
    #[must_use]
    pub fn binary_id(&self) -> String {
        format!("{}::{}", self.package, self.target)
    }

    /// The nextest filter expression matching exactly the tests [`Self::args`]
    /// runs.
    #[must_use]
    pub fn expression(&self) -> String {
        format!("(binary_id({}) and test({}))", self.binary_id(), self.filter)
    }
}

/// One preparation step a shard's tests need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prepare {
    /// The ordered JavaScript chain that builds the migration host, the DB SDK
    /// and the V8 adapter (`build_chain::BUILD_CHAIN`).
    HostChain,
    /// The workspace executables Control's workflow process suites start as
    /// fleets of service processes (`zeroship_testkit::prebuilt::BUILD_ARGS`).
    ServiceBinaries,
    /// The real CDC relay executable the data suites start.
    CdcRelay,
    /// The data packages' database targets are ordinary tests, and the
    /// snapshot tests' `pg_dump`/`pg_restore` match the fixture server's major.
    DataPosture,
    /// Hold the bare PostgreSQL server every data test process clones from.
    BareServer,
    /// Hold the migrated platform server, so its first boot is paid once.
    PlatformServer,
    /// Hold the migration server's own migrated server, whose cases write
    /// cluster roles and execution zones.
    MigrateServerPlatform,
}

impl Prepare {
    /// Whether the step produces an artifact a build-only run also needs,
    /// rather than holding a server for the tests.
    #[must_use]
    pub const fn builds(self) -> bool {
        matches!(self, Self::HostChain | Self::ServiceBinaries | Self::CdcRelay)
    }
}

impl Shard {
    /// Whether the shard's tests start fleets of service processes, each
    /// opening many io_uring rings at once (see `memlock`).
    #[must_use]
    pub fn starts_services(&self) -> bool {
        self.prepare.contains(&Prepare::ServiceBinaries)
    }

    /// The label of the primary run's reports.
    pub const PRIMARY: &'static str = "tests";
}

/// The primary compio-postgres run: the TLS connector and its live suite, and
/// every codec, each of which compiles the conversions and the tests for one
/// optional type crate. Each is additive, so the default tests run under them
/// as well.
const COMPIO_POSTGRES_PRIMARY: &[&str] = &[
    "compio-postgres/tls",
    "compio-postgres/array-impls",
    "compio-postgres/with-bit-vec-0_8",
    "compio-postgres/with-bit-vec-0_9",
    "compio-postgres/with-chrono-0_4",
    "compio-postgres/with-cidr-0_3",
    "compio-postgres/with-eui48-1",
    "compio-postgres/with-geo-types-0_7",
    "compio-postgres/with-jiff-0_2",
    "compio-postgres/with-serde_json-1",
    "compio-postgres/with-smol_str-01",
    "compio-postgres/with-time-0_3",
    "compio-postgres/with-uuid-1",
];

/// Every shard, in the order CI lists them.
pub const SHARDS: &[Shard] = &[
    Shard {
        name: "auth",
        packages: &[
            "zeroship-auth",
            "zeroship-authn",
            "zeroship-authz",
            "zeroship-mailer",
            "zeroship-gateway",
        ],
        features: &[],
        repeats: &[],
        unexercised: &[],
        untimed: &[],
        prepare: &[Prepare::HostChain, Prepare::PlatformServer],
    },
    Shard {
        name: "billing",
        packages: &[
            "zeroship-control",
            "zeroship-migrate-server",
            "zeroship-metering",
            "zeroship-stream",
        ],
        features: &[],
        repeats: &[],
        unexercised: &[],
        untimed: &[],
        prepare: &[
            Prepare::HostChain,
            Prepare::ServiceBinaries,
            Prepare::PlatformServer,
            Prepare::MigrateServerPlatform,
        ],
    },
    Shard {
        name: "data",
        packages: &[
            "zeroship-data-macros",
            "zeroship-data-orm",
            "zeroship-data-v8",
            "zeroship-data-cdc-wire",
            "zeroship-data-cdc-server",
            "zeroship-data-testkit",
        ],
        features: &[],
        repeats: &[],
        unexercised: &[],
        untimed: &[Untimed {
            package: "zeroship-data-orm",
            target: "main",
            filter: "integration::derive_contract",
            reason: "trybuild compiles its generated project's whole dependency graph inside \
                     the test, into a target directory of its own, the first time a checkout \
                     runs it",
        }],
        prepare: &[
            Prepare::DataPosture,
            Prepare::CdcRelay,
            Prepare::HostChain,
            Prepare::BareServer,
            Prepare::PlatformServer,
        ],
    },
    Shard {
        name: "runtime",
        packages: &[
            "zeroship-runtime",
            "zeroship-runtime-macros",
            "zeroship-worker",
            "zeroship-cli",
            "zeroship-kv",
            "zeroship-kv-v8",
            "zeroship-storage",
            "zeroship-storage-v8",
        ],
        features: &[],
        repeats: &[],
        unexercised: &[Unexercised {
            feature: "zeroship-runtime/bench-bins",
            reason: "it builds the bench binaries, which carry no tests",
        }],
        untimed: &[],
        prepare: &[Prepare::HostChain, Prepare::PlatformServer],
    },
    Shard {
        name: "workflow",
        packages: &[
            "zeroship-workflow",
            "zeroship-workflow-calendar",
            "zeroship-workflow-client",
            "zeroship-workflow-schema",
            "zeroship-workflow-testkit",
            "zeroship-workflow-v8",
            "zeroship-workflow-manager",
            "zeroship-workflow-server",
            "zeroship-workflow-runner",
        ],
        features: &[],
        repeats: &[],
        unexercised: &[],
        untimed: &[],
        prepare: &[Prepare::HostChain],
    },
    Shard {
        name: "migrate",
        packages: &[
            "zeroship-migrate",
            "zeroship-migrate-core",
            "zeroship-migrate-backend",
            "zeroship-migrate-ir",
            "zeroship-migrate-policy",
            "zeroship-migrate-postgres",
            "zeroship-migrate-testkit",
            "zeroship-migrate-mysql",
            "zeroship-migrate-sqlite",
            "zeroship-migrate-node",
        ],
        features: &[],
        repeats: &[],
        unexercised: &[],
        untimed: &[],
        prepare: &[Prepare::HostChain],
    },
    Shard {
        name: "compio-postgres",
        packages: &["compio-postgres"],
        features: COMPIO_POSTGRES_PRIMARY,
        repeats: &[
            Repeat {
                label: "postgres-18",
                features: &["compio-postgres/suite-on-postgres-18"],
                reason: "the driver supports a second PostgreSQL major, and this run notices \
                         a test pinning behaviour only PostgreSQL 16 has",
            },
            Repeat {
                label: "over-tls",
                features: &["compio-postgres/suite-over-tls"],
                reason: "the suite's connections then negotiate TLS, so every protocol test \
                         also runs through the TLS transport",
            },
            Repeat {
                label: "statement-cache",
                features: &["compio-postgres/suite-with-statement-cache"],
                reason: "the suite's connections then cache prepared statements, so every \
                         query test also runs through the cache",
            },
        ],
        unexercised: &[],
        untimed: &[],
        prepare: &[],
    },
    Shard {
        name: "foundation",
        packages: &[
            "zeroship-testkit-server",
            "zeroship-testkit",
            "zeroship-core",
            "zeroship-id",
            "zeroship-bundle",
            "zeroship-config-contract",
            "zeroship-config-macros",
            "compio-redis",
            "compio-s3",
        ],
        features: &[],
        repeats: &[],
        unexercised: &[],
        untimed: &[],
        prepare: &[],
    },
];

/// The tests that also run on the creator platforms: CI's `portable` job.
///
/// Creators install the `zeroship` CLI on their own machines, so its package's
/// tests run on macOS and Windows as well as in the `runtime` shard on Linux,
/// and so do the tests of the local storage and deployment-cache backends it
/// serves from. That repeats them deliberately, on another operating system,
/// which is why it is declared here rather than being a shard owning the same
/// packages. The run on each platform is the packages' whole nextest run, with
/// nothing filtered out. A test that exists on some systems only, a container
/// suite among them, is compiled for those systems by its own `#[cfg]`, and is
/// listed in [`Portable::confined`] with why; CI's `verify` job holds every
/// platform's listing to the shards' listing and that table, so a test cannot
/// leave or join a platform's run without an entry saying so.
#[derive(Debug)]
pub struct Portable {
    /// The workspace packages whose tests run on every platform.
    pub packages: &'static [&'static str],
    /// The systems the packages' tests run on besides Linux.
    pub platforms: &'static [Platform],
    /// The packages' tests that are compiled for some systems only.
    pub confined: &'static [Confined],
}

/// One operating system CI's `portable` job runs on.
#[derive(Debug)]
pub struct Platform {
    /// Rust's `target_os` for the system; the job's matrix and the run's reports
    /// (`junit-portable-<name>.xml`, `list-portable-<name>.json`) carry it.
    pub name: &'static str,
    /// The GitHub-hosted runner image the job runs on.
    pub runner: &'static str,
}

/// Tests whose `#[cfg]` compiles them for some systems and not the others.
#[derive(Debug)]
pub struct Confined {
    /// The nextest binary id of the test target.
    pub binary: &'static str,
    /// The tests it covers: a module path ending in `::` covers every test under
    /// it, and anything else is one test's whole name.
    pub prefix: &'static str,
    /// The systems the tests exist on, by `target_os`: `linux` and the names of
    /// [`Portable::platforms`].
    pub on: &'static [&'static str],
    /// Why the tests cannot exist on the other systems.
    pub reason: &'static str,
}

impl Confined {
    /// Whether this entry covers the test `test` of `binary`. A whole-name entry
    /// covers that test alone, so a new test whose name merely starts the same
    /// way is not confined by it.
    #[must_use]
    pub fn covers(&self, binary: &str, test: &str) -> bool {
        binary == self.binary
            && if self.prefix.ends_with("::") {
                test.starts_with(self.prefix)
            } else {
                test == self.prefix
            }
    }
}

/// The system the shards run on.
pub const LINUX: &str = "linux";

/// The portable run, and the tests confined to some of its systems.
pub const PORTABLE: Portable = Portable {
    packages: &["zeroship-cli", "zeroship-storage", "zeroship-bundle"],
    platforms: &[
        Platform {
            name: "macos",
            runner: "macos-14",
        },
        Platform {
            name: "windows",
            runner: "windows-2022",
        },
    ],
    confined: &[
        Confined {
            binary: CLI_UNIT,
            prefix: "dev::tests::",
            on: UNIX,
            reason: "they set and read POSIX file modes, which Windows does not keep",
        },
        Confined {
            binary: CLI_UNIT,
            prefix: "migrate::tests::a_positional_file_is_refused_rather_than_posted",
            on: UNIX,
            reason: "it reads the recorded migration set from zeroship-testkit, which builds on \
                     Unix only: its container lease is an flock protocol",
        },
        Confined {
            binary: CLI_UNIT,
            prefix: "migrate::tests::recording_db_todos_reproduces_the_posted_ir",
            on: UNIX,
            reason: "it compares against the recorded migration set in zeroship-testkit, which \
                     builds on Unix only: its container lease is an flock protocol",
        },
        Confined {
            binary: CLI_UNIT,
            prefix: "project_config::tests::build_dist_cannot_symlink_to_the_project_root",
            on: UNIX,
            reason: "it creates a symbolic link, which Windows grants only to elevated or \
                     developer-mode accounts",
        },
        Confined {
            binary: CLI_UNIT,
            prefix: "project_keys::tests::public_keys_are_refused_and_never_replaced",
            on: UNIX,
            reason: "it widens the key's POSIX mode, which Windows does not keep",
        },
        Confined {
            binary: CLI_E2E,
            prefix: "e2e::parent_death_test::",
            on: &[LINUX],
            reason: "it tests the PR_SET_PDEATHSIG arm of the parent-death guard and reads \
                     liveness from /proc, both Linux's",
        },
        Confined {
            binary: CLI_E2E,
            prefix: "e2e::parent_death_unix_test::",
            on: UNIX,
            reason: "it runs the runtime under sh and kills the shell, the Unix arms of the \
                     parent-death guard",
        },
        Confined {
            binary: CLI_E2E,
            prefix: "e2e::parent_death_windows_test::",
            on: &["windows"],
            reason: "it tests the Windows arm of the parent-death guard, which waits on the \
                     process handle of the parent the environment names",
        },
        Confined {
            binary: CLI_E2E,
            prefix: "e2e::secrets_test::",
            on: UNIX,
            reason: "it stands in for curl with a shell script on PATH",
        },
        Confined {
            binary: CLI_E2E,
            prefix: "e2e::serve_test::serve_enforces_a_cpu_limit",
            on: &[LINUX],
            reason: "only Linux stops a running handler at its CPU limit; the other systems \
                     refuse the flag",
        },
        Confined {
            binary: CLI_E2E,
            prefix: "e2e::serve_test::serve_refuses_a_cpu_limit_it_cannot_enforce",
            on: &["macos", "windows"],
            reason: "Linux enforces a CPU limit, so only the other systems refuse the flag",
        },
        Confined {
            binary: CLI_E2E,
            prefix: "e2e::workflow_local::",
            on: &[LINUX],
            reason: "its host trusts a port only once /proc shows its own child holds the \
                     listening socket",
        },
        Confined {
            binary: "zeroship-bundle::main",
            prefix: "integration::blob_test::local_disk_a_directory_key_that_cannot_be_opened_is_refused_naming_it",
            on: UNIX,
            reason: "it closes a directory with a POSIX mode, which Windows does not keep",
        },
        Confined {
            binary: "zeroship-bundle::main",
            prefix: "integration::blob_test::local_disk_an_unreadable_manifest_key_is_refused_and_kept",
            on: UNIX,
            reason: "it withdraws read permission through a POSIX mode, which Windows does not \
                     keep",
        },
        Confined {
            binary: "zeroship-bundle::main",
            prefix: "integration::zstd_window_probe::zstd_does_not_allocate_the_declared_window_eagerly",
            on: &[LINUX],
            reason: "it reads the process's resident set from /proc, which only Linux has",
        },
        Confined {
            binary: "zeroship-bundle::main",
            prefix: "integration::s3_blob_store::",
            on: &[LINUX],
            reason: CONTAINER,
        },
        Confined {
            binary: "zeroship-storage",
            prefix: "backend::local::tests::a_key_naming_a_directory_that_cannot_be_opened_is_no_object",
            on: UNIX,
            reason: "it closes a directory with a POSIX mode, which Windows does not keep",
        },
        Confined {
            binary: "zeroship-storage::main",
            prefix: "integration::backend_parity::s3_parity_and_large_stream",
            on: &[LINUX],
            reason: CONTAINER,
        },
        Confined {
            binary: "zeroship-storage::main",
            prefix: "integration::backend_parity::s3_shared_gateway_isolates_multipart_uploads_by_prefix",
            on: &[LINUX],
            reason: CONTAINER,
        },
        Confined {
            binary: "zeroship-storage::main",
            prefix: "integration::store::s3_scoped_storage_contract",
            on: &[LINUX],
            reason: CONTAINER,
        },
    ],
};

/// Why an S3 suite runs on Linux only.
const CONTAINER: &str = "it joins the shared S3 gateway container, and the macOS and Windows \
                         runners have no Linux container engine";

/// The CLI's unit tests and its end-to-end target, as nextest names them.
const CLI_UNIT: &str = "zeroship-cli::bin/zeroship";
const CLI_E2E: &str = "zeroship-cli::main";

/// The systems with POSIX file modes, links and shells.
const UNIX: &[&str] = &[LINUX, "macos"];

impl Portable {
    /// The report label a platform's run carries, `portable-<platform>`.
    #[must_use]
    pub fn label(platform: &Platform) -> String {
        format!("portable-{}", platform.name)
    }

    /// The arguments after `cargo` for a platform's test run: the packages'
    /// whole nextest run, failing when it selects no test.
    #[must_use]
    pub fn nextest_args(&self) -> Vec<String> {
        nextest_args(self.packages, &[], None, false)
    }

    /// The arguments after `cargo` that list what a platform's run builds.
    #[must_use]
    pub fn list_args(&self) -> Vec<String> {
        list_args(self.packages, &[])
    }

    /// The platforms as the JSON array CI's `portable` matrix reads.
    #[must_use]
    pub fn matrix(&self) -> String {
        let entries: Vec<serde_json::Value> = self
            .platforms
            .iter()
            .map(|platform| serde_json::json!({ "name": platform.name, "runner": platform.runner }))
            .collect();
        serde_json::Value::from(entries).to_string()
    }
}

/// The shard named `name`.
#[must_use]
pub fn find(name: &str) -> Option<&'static Shard> {
    SHARDS.iter().find(|shard| shard.name == name)
}

/// The package a `package/feature` entry enables a feature of.
///
/// # Panics
/// When the entry is not `package/feature`, which the repository contract
/// rejects for every entry in [`SHARDS`].
#[must_use]
pub fn feature_package(entry: &str) -> &str {
    entry
        .split_once('/')
        .unwrap_or_else(|| panic!("{entry} is not package/feature"))
        .0
}

/// The arguments after `cargo` that select `packages` with `features`, as
/// every nextest, listing and doctest invocation of a run passes them.
#[must_use]
pub fn selection(packages: &[&str], features: &[&str]) -> Vec<String> {
    let mut args = Vec::new();
    for package in packages {
        args.extend(["-p".to_owned(), (*package).to_owned()]);
    }
    for feature in features {
        args.extend(["--features".to_owned(), (*feature).to_owned()]);
    }
    args
}

/// The nextest filter expression a shard's primary run uses: `filter` when one
/// is given, minus the shard's untimed tests.
#[must_use]
pub fn primary_expression(shard: &Shard, filter: Option<&str>) -> Option<String> {
    let untimed: Vec<String> = shard.untimed.iter().map(Untimed::expression).collect();
    match (filter, untimed.is_empty()) {
        (None, true) => None,
        (Some(filter), true) => Some(filter.to_owned()),
        (None, false) => Some(format!("not ({})", untimed.join(" or "))),
        (Some(filter), false) => Some(format!("({filter}) and not ({})", untimed.join(" or "))),
    }
}

/// The arguments after `cargo` for one nextest run over `packages` with
/// `features`, narrowed by `expression` when one is given.
#[must_use]
pub fn nextest_args(
    packages: &[&str],
    features: &[&str],
    expression: Option<&str>,
    build_only: bool,
) -> Vec<String> {
    let mut args: Vec<String> = ["nextest", "run", "--locked", "--profile", "ci"]
        .map(String::from)
        .to_vec();
    if build_only {
        args.push("--no-run".into());
    } else {
        args.extend(["--no-tests", "fail"].map(String::from));
    }
    args.extend(selection(packages, features));
    if let Some(expression) = expression {
        args.extend(["--filter-expr".into(), expression.to_owned()]);
    }
    args
}

/// The arguments after `cargo` that list what a run over `packages` with
/// `features` builds, unfiltered, as JSON.
#[must_use]
pub fn list_args(packages: &[&str], features: &[&str]) -> Vec<String> {
    let mut args: Vec<String> =
        ["nextest", "list", "--locked", "--profile", "ci", "--message-format", "json"]
            .map(String::from)
            .to_vec();
    args.extend(selection(packages, features));
    args
}

/// The arguments after `cargo` for the doctests of the shard's packages.
///
/// The selection is the whole shard's, so feature resolution matches its
/// nextest run; packages without a library contribute no doctests.
#[must_use]
pub fn doctest_args(packages: &[&str], features: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = ["test", "--locked", "--no-fail-fast", "--doc"]
        .map(String::from)
        .to_vec();
    args.extend(selection(packages, features));
    args
}
