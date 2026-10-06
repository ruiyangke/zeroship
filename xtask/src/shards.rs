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
            "zeroship-memlock",
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
