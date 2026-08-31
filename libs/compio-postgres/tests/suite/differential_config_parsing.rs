//! Differential connection-string parsing against `tokio-postgres` 0.7.18.
//!
//! Both parsers receive every string. Accepted configs are reduced to owned,
//! semantic observations before comparison: port defaults and one-port
//! broadcast are expanded, Unix paths retain their bytes, and the two crates'
//! distinct enum types become their variant names.
//!
//! Tokio-postgres is the porting oracle, not the final authority. The explicit
//! divergence list below contains only behavior already documented in this
//! crate and ruled on by `PostgreSQL` 18.4's libpq. In particular, libpq accepts
//! terminal empty values (`fe-connect.c:6363-6425`), treats `@` hosts as Linux
//! abstract sockets (`pqcomm.h:62-70`), uses milliseconds for
//! `tcp_user_timeout` (`fe-connect.c:2661-2681`), accepts six SSL and target
//! session modes (`fe-connect.c:1764-1780,1989-2016`), and spells its probe
//! count `keepalives_count` (`fe-connect.c:269-271`). Any difference outside
//! that list is reported as a `FINDING`, with the exact input and both values.

use std::collections::BTreeSet;
use std::net::IpAddr;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
struct Case {
    name: &'static str,
    input: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum HostObservation {
    Tcp(String),
    Unix(Vec<u8>),
    Abstract(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ConfigObservation {
    user: Option<String>,
    password: Option<Vec<u8>>,
    dbname: Option<String>,
    options: Option<String>,
    application_name: Option<String>,
    ssl_mode: String,
    ssl_negotiation: String,
    hosts: Vec<HostObservation>,
    hostaddrs: Vec<Option<IpAddr>>,
    raw_ports: Vec<u16>,
    effective_ports: Vec<u16>,
    connect_timeout: Option<Duration>,
    tcp_user_timeout: Option<Duration>,
    keepalives: bool,
    keepalives_idle: Duration,
    keepalives_interval: Option<Duration>,
    keepalives_count: Option<u32>,
    target_session_attrs: String,
    channel_binding: String,
    load_balance_hosts: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ParseOutcome {
    Accepted(Box<ConfigObservation>),
    Rejected,
}

#[derive(Debug)]
struct CaseResult {
    case: Case,
    ours: ParseOutcome,
    theirs: ParseOutcome,
}

#[derive(Debug)]
struct FieldDifference {
    case: &'static str,
    input: &'static str,
    field: &'static str,
    ours: String,
    theirs: String,
}

#[allow(clippy::too_many_lines)]
fn cases() -> Vec<Case> {
    let mut cases = vec![
        Case {
            name: "equivalent keyword",
            input: "user=creator password='pa@/: ss' dbname='app/db' \
                    options='-c search_path=app' application_name='oracle case' \
                    sslmode=require sslnegotiation=direct host=2001:db8::1 \
                    hostaddr=2001:db8::1 port=6543 connect_timeout=9 \
                    keepalives=0 keepalives_idle=17 keepalives_interval=7 \
                    target_session_attrs=read-write channel_binding=require \
                    load_balance_hosts=random",
        },
        Case {
            name: "equivalent postgres URI",
            input: "postgres://creator:pa%40%2F%3A%20ss@[2001:db8::1]:6543/app%2Fdb\
                    ?options=-c%20search_path%3Dapp&application_name=oracle%20case\
                    &sslmode=require&sslnegotiation=direct&hostaddr=2001%3Adb8%3A%3A1\
                    &connect_timeout=9&keepalives=0&keepalives_idle=17\
                    &keepalives_interval=7&target_session_attrs=read-write\
                    &channel_binding=require&load_balance_hosts=random",
        },
        Case {
            name: "equivalent postgresql URI",
            input: "postgresql://creator:pa%40%2F%3A%20ss@[2001:db8::1]:6543/app%2Fdb\
                    ?options=-c%20search_path%3Dapp&application_name=oracle%20case\
                    &sslmode=require&sslnegotiation=direct&hostaddr=2001%3Adb8%3A%3A1\
                    &connect_timeout=9&keepalives=0&keepalives_idle=17\
                    &keepalives_interval=7&target_session_attrs=read-write\
                    &channel_binding=require&load_balance_hosts=random",
        },
        Case {
            name: "IPv6 without port",
            input: "postgres://[2001:db8::2]/db?keepalives_idle=17",
        },
        Case {
            name: "multiple hosts and ports",
            input: "postgres://one.invalid:5433,two.invalid:5434/db?keepalives_idle=17",
        },
        Case {
            name: "one port for three hosts",
            input: "host=one.invalid,two.invalid,three.invalid port=6000 keepalives_idle=17",
        },
        Case {
            name: "multiple hostaddrs",
            input: "host=one.invalid,two.invalid hostaddr=192.0.2.10,2001:db8::10 \
                    port=5433,5434 keepalives_idle=17",
        },
        Case {
            name: "filesystem socket",
            input: "host=/var/run/postgresql port=5432 keepalives_idle=17",
        },
        Case {
            name: "empty dbname",
            input: "host=empty.invalid keepalives_idle=17 dbname=",
        },
        Case {
            name: "missing value",
            input: "host=missing.invalid keepalives_idle=17 user",
        },
        Case {
            name: "repeated scalar",
            input: "host=repeat.invalid application_name=first application_name=last \
                    keepalives_idle=17",
        },
        Case {
            name: "quoted escaped and spaced",
            input: " \thost \n = \r quoted.invalid password = 'a\\'b\\\\c' \
                    options = -c\\ search_path=public application_name = slash\\ value \
                    keepalives_idle = 17 ",
        },
        Case {
            name: "unknown parameter",
            input: "host=unknown.invalid no_such_parameter=1",
        },
        Case {
            name: "malformed port",
            input: "host=port.invalid port=not-a-port",
        },
        Case {
            name: "unterminated quote",
            input: "host=quote.invalid password='unterminated",
        },
        Case {
            name: "sslmode disable",
            input: "host=ssl.invalid sslmode=disable keepalives_idle=17",
        },
        Case {
            name: "sslmode allow",
            input: "host=ssl.invalid sslmode=allow keepalives_idle=17",
        },
        Case {
            name: "sslmode prefer",
            input: "host=ssl.invalid sslmode=prefer keepalives_idle=17",
        },
        Case {
            name: "sslmode require",
            input: "host=ssl.invalid sslmode=require keepalives_idle=17",
        },
        Case {
            name: "sslmode verify-ca",
            input: "host=ssl.invalid sslmode=verify-ca keepalives_idle=17",
        },
        Case {
            name: "sslmode verify-full",
            input: "host=ssl.invalid sslmode=verify-full keepalives_idle=17",
        },
        Case {
            name: "target any",
            input: "host=target.invalid target_session_attrs=any keepalives_idle=17",
        },
        Case {
            name: "target read-write",
            input: "host=target.invalid target_session_attrs=read-write keepalives_idle=17",
        },
        Case {
            name: "target read-only",
            input: "host=target.invalid target_session_attrs=read-only keepalives_idle=17",
        },
        Case {
            name: "target primary",
            input: "host=target.invalid target_session_attrs=primary keepalives_idle=17",
        },
        Case {
            name: "target standby",
            input: "host=target.invalid target_session_attrs=standby keepalives_idle=17",
        },
        Case {
            name: "target prefer-standby",
            input: "host=target.invalid target_session_attrs=prefer-standby keepalives_idle=17",
        },
        Case {
            name: "channel binding disable",
            input: "host=channel.invalid channel_binding=disable keepalives_idle=17",
        },
        Case {
            name: "channel binding prefer",
            input: "host=channel.invalid channel_binding=prefer keepalives_idle=17",
        },
        Case {
            name: "channel binding require",
            input: "host=channel.invalid channel_binding=require keepalives_idle=17",
        },
        Case {
            name: "load balance disable",
            input: "host=load.invalid load_balance_hosts=disable keepalives_idle=17",
        },
        Case {
            name: "load balance random",
            input: "host=load.invalid load_balance_hosts=random keepalives_idle=17",
        },
        Case {
            name: "tcp user timeout units",
            input: "host=timeout.invalid tcp_user_timeout=5000 keepalives_idle=17",
        },
        Case {
            name: "libpq keepalive count spelling",
            input: "host=count.invalid keepalives_count=5 keepalives_idle=17",
        },
        Case {
            name: "tokio keepalive retries spelling",
            input: "host=count.invalid keepalives_retries=5 keepalives_idle=17",
        },
    ];

    #[cfg(target_os = "linux")]
    cases.push(Case {
        name: "abstract socket",
        input: "host=@zeroship-oracle port=5432 keepalives_idle=17",
    });

    cases
}

fn path_observation(bytes: &[u8]) -> HostObservation {
    bytes.strip_prefix(&[0]).map_or_else(
        || HostObservation::Unix(bytes.to_vec()),
        |name| HostObservation::Abstract(name.to_vec()),
    )
}

fn compio_hosts(config: &compio_postgres::Config) -> Vec<HostObservation> {
    config
        .get_hosts()
        .iter()
        .map(|host| match host {
            compio_postgres::config::Host::Tcp(host) => HostObservation::Tcp(host.clone()),
            #[cfg(unix)]
            compio_postgres::config::Host::Unix(path) => {
                path_observation(path.as_os_str().as_bytes())
            }
        })
        .collect()
}

fn tokio_hosts(config: &tokio_postgres::Config) -> Vec<HostObservation> {
    config
        .get_hosts()
        .iter()
        .map(|host| match host {
            tokio_postgres::config::Host::Tcp(host) => HostObservation::Tcp(host.clone()),
            #[cfg(unix)]
            tokio_postgres::config::Host::Unix(path) => {
                path_observation(path.as_os_str().as_bytes())
            }
        })
        .collect()
}

fn effective_ports(raw: &[u16], endpoint_count: usize) -> Vec<u16> {
    match raw {
        [] => vec![5432; endpoint_count],
        [port] if endpoint_count > 1 => vec![*port; endpoint_count],
        ports => ports.to_vec(),
    }
}

fn compio_observation(config: &compio_postgres::Config) -> ConfigObservation {
    let hosts = compio_hosts(config);
    let hostaddrs = config.get_hostaddrs().to_vec();
    let raw_ports = config.get_ports().to_vec();
    let endpoint_count = hosts.len().max(hostaddrs.len());
    ConfigObservation {
        user: config.get_user().map(str::to_owned),
        password: config.get_password().map(<[u8]>::to_vec),
        dbname: config.get_dbname().map(str::to_owned),
        options: config.get_options().map(str::to_owned),
        application_name: config.get_application_name().map(str::to_owned),
        ssl_mode: format!("{:?}", config.get_ssl_mode()),
        ssl_negotiation: format!("{:?}", config.get_ssl_negotiation()),
        hosts,
        hostaddrs,
        effective_ports: effective_ports(&raw_ports, endpoint_count),
        raw_ports,
        connect_timeout: config.get_connect_timeout().copied(),
        tcp_user_timeout: config.get_tcp_user_timeout().copied(),
        keepalives: config.get_keepalives(),
        keepalives_idle: config.get_keepalives_idle(),
        keepalives_interval: config.get_keepalives_interval(),
        keepalives_count: config.get_keepalives_count(),
        target_session_attrs: format!("{:?}", config.get_target_session_attrs()),
        channel_binding: format!("{:?}", config.get_channel_binding()),
        load_balance_hosts: format!("{:?}", config.get_load_balance_hosts()),
    }
}

fn tokio_observation(config: &tokio_postgres::Config) -> ConfigObservation {
    let hosts = tokio_hosts(config);
    let hostaddrs: Vec<_> = config.get_hostaddrs().iter().copied().map(Some).collect();
    let raw_ports = config.get_ports().to_vec();
    let endpoint_count = hosts.len().max(hostaddrs.len());
    ConfigObservation {
        user: config.get_user().map(str::to_owned),
        password: config.get_password().map(<[u8]>::to_vec),
        dbname: config.get_dbname().map(str::to_owned),
        options: config.get_options().map(str::to_owned),
        application_name: config.get_application_name().map(str::to_owned),
        ssl_mode: format!("{:?}", config.get_ssl_mode()),
        ssl_negotiation: format!("{:?}", config.get_ssl_negotiation()),
        hosts,
        hostaddrs,
        effective_ports: effective_ports(&raw_ports, endpoint_count),
        raw_ports,
        connect_timeout: config.get_connect_timeout().copied(),
        tcp_user_timeout: config.get_tcp_user_timeout().copied(),
        keepalives: config.get_keepalives(),
        keepalives_idle: config.get_keepalives_idle(),
        keepalives_interval: config.get_keepalives_interval(),
        keepalives_count: config.get_keepalives_retries(),
        target_session_attrs: format!("{:?}", config.get_target_session_attrs()),
        channel_binding: format!("{:?}", config.get_channel_binding()),
        load_balance_hosts: format!("{:?}", config.get_load_balance_hosts()),
    }
}

fn parse(case: Case) -> CaseResult {
    let ours = case
        .input
        .parse::<compio_postgres::Config>()
        .map_or(ParseOutcome::Rejected, |config| {
            ParseOutcome::Accepted(Box::new(compio_observation(&config)))
        });
    let theirs = case
        .input
        .parse::<tokio_postgres::Config>()
        .map_or(ParseOutcome::Rejected, |config| {
            ParseOutcome::Accepted(Box::new(tokio_observation(&config)))
        });
    CaseResult { case, ours, theirs }
}

fn field_differences(result: &CaseResult) -> Vec<FieldDifference> {
    let (ours, theirs) = match (&result.ours, &result.theirs) {
        (ParseOutcome::Rejected, ParseOutcome::Rejected) => return Vec::new(),
        (ParseOutcome::Accepted(_), ParseOutcome::Rejected) => {
            return vec![FieldDifference {
                case: result.case.name,
                input: result.case.input,
                field: "parse_outcome",
                ours: "accepted".to_owned(),
                theirs: "rejected".to_owned(),
            }];
        }
        (ParseOutcome::Rejected, ParseOutcome::Accepted(_)) => {
            return vec![FieldDifference {
                case: result.case.name,
                input: result.case.input,
                field: "parse_outcome",
                ours: "rejected".to_owned(),
                theirs: "accepted".to_owned(),
            }];
        }
        (ParseOutcome::Accepted(ours), ParseOutcome::Accepted(theirs)) => (ours, theirs),
    };

    let mut differences = Vec::new();
    macro_rules! compare_field {
        ($field:ident, $name:literal) => {
            if ours.$field != theirs.$field {
                differences.push(FieldDifference {
                    case: result.case.name,
                    input: result.case.input,
                    field: $name,
                    ours: format!("{:?}", ours.$field),
                    theirs: format!("{:?}", theirs.$field),
                });
            }
        };
    }

    compare_field!(user, "user");
    compare_field!(password, "password");
    compare_field!(dbname, "dbname");
    compare_field!(options, "options");
    compare_field!(application_name, "application_name");
    compare_field!(ssl_mode, "ssl_mode");
    compare_field!(ssl_negotiation, "ssl_negotiation");
    compare_field!(hosts, "hosts");
    compare_field!(hostaddrs, "hostaddrs");
    // Raw getters differ for an omitted URI port (`[]` versus `[5432]`), but
    // both dial 5432. The task requires meaning rather than representation.
    compare_field!(effective_ports, "ports");
    compare_field!(connect_timeout, "connect_timeout");
    compare_field!(tcp_user_timeout, "tcp_user_timeout");
    compare_field!(keepalives, "keepalives");
    compare_field!(keepalives_idle, "keepalives_idle");
    compare_field!(keepalives_interval, "keepalives_interval");
    compare_field!(keepalives_count, "keepalives_count");
    compare_field!(target_session_attrs, "target_session_attrs");
    compare_field!(channel_binding, "channel_binding");
    compare_field!(load_balance_hosts, "load_balance_hosts");
    differences
}

fn documented_divergences() -> BTreeSet<(&'static str, &'static str)> {
    let mut expected = BTreeSet::from([
        ("empty dbname", "parse_outcome"),
        ("sslmode allow", "parse_outcome"),
        ("sslmode verify-ca", "parse_outcome"),
        ("sslmode verify-full", "parse_outcome"),
        ("target primary", "parse_outcome"),
        ("target standby", "parse_outcome"),
        ("target prefer-standby", "parse_outcome"),
        ("tcp user timeout units", "tcp_user_timeout"),
        ("libpq keepalive count spelling", "parse_outcome"),
        ("tokio keepalive retries spelling", "parse_outcome"),
    ]);
    #[cfg(target_os = "linux")]
    expected.insert(("abstract socket", "hosts"));
    expected
}

fn result_named<'a>(results: &'a [CaseResult], name: &str) -> &'a CaseResult {
    results
        .iter()
        .find(|result| result.case.name == name)
        .unwrap_or_else(|| panic!("no config observation named {name}"))
}

fn accepted_ours<'a>(results: &'a [CaseResult], name: &str) -> &'a ConfigObservation {
    match &result_named(results, name).ours {
        ParseOutcome::Accepted(observation) => observation,
        ParseOutcome::Rejected => panic!("compio-postgres unexpectedly rejected {name}"),
    }
}

fn accepted_theirs<'a>(results: &'a [CaseResult], name: &str) -> &'a ConfigObservation {
    match &result_named(results, name).theirs {
        ParseOutcome::Accepted(observation) => observation,
        ParseOutcome::Rejected => panic!("tokio-postgres unexpectedly rejected {name}"),
    }
}

fn assert_acceptance(results: &[CaseResult], name: &str, ours_accepts: bool, theirs_accepts: bool) {
    assert_eq!(
        matches!(result_named(results, name).ours, ParseOutcome::Accepted(_)),
        ours_accepts,
        "compio-postgres outcome for {name}"
    );
    assert_eq!(
        matches!(
            result_named(results, name).theirs,
            ParseOutcome::Accepted(_)
        ),
        theirs_accepts,
        "tokio-postgres outcome for {name}"
    );
}

#[allow(clippy::too_many_lines)]
fn assert_field_space_was_exercised(results: &[CaseResult]) {
    let expected_case_count = if cfg!(target_os = "linux") { 36 } else { 35 };
    assert_eq!(
        results.len(),
        expected_case_count,
        "a connection-string case silently disappeared"
    );
    let names: BTreeSet<_> = results.iter().map(|result| result.case.name).collect();
    assert_eq!(
        names.len(),
        results.len(),
        "connection-string case names must be unique"
    );

    let (mut both_accepted, mut both_rejected, mut ours_only, mut theirs_only) = (0, 0, 0, 0);
    for result in results {
        match (&result.ours, &result.theirs) {
            (ParseOutcome::Accepted(_), ParseOutcome::Accepted(_)) => both_accepted += 1,
            (ParseOutcome::Rejected, ParseOutcome::Rejected) => both_rejected += 1,
            (ParseOutcome::Accepted(_), ParseOutcome::Rejected) => ours_only += 1,
            (ParseOutcome::Rejected, ParseOutcome::Accepted(_)) => theirs_only += 1,
        }
    }
    assert_eq!(
        (both_accepted, both_rejected, ours_only, theirs_only),
        (if cfg!(target_os = "linux") { 23 } else { 22 }, 4, 8, 1),
        "the corpus stopped straddling the accept/reject boundary"
    );

    let keyword = accepted_ours(results, "equivalent keyword");
    assert_eq!(
        keyword,
        accepted_ours(results, "equivalent postgres URI"),
        "compio-postgres parsed equivalent keyword and postgres URI forms differently"
    );
    assert_eq!(
        keyword,
        accepted_ours(results, "equivalent postgresql URI"),
        "compio-postgres parsed the two URI schemes differently"
    );
    let tokio_keyword = accepted_theirs(results, "equivalent keyword");
    assert_eq!(
        tokio_keyword,
        accepted_theirs(results, "equivalent postgres URI"),
        "tokio-postgres parsed equivalent keyword and postgres URI forms differently"
    );
    assert_eq!(
        tokio_keyword,
        accepted_theirs(results, "equivalent postgresql URI"),
        "tokio-postgres parsed the two URI schemes differently"
    );

    // One dense case proves every jointly-populatable field was nonempty or
    // nondefault. Exact decoded bytes make percent-decoding observable.
    assert_eq!(keyword.user.as_deref(), Some("creator"));
    assert_eq!(keyword.password.as_deref(), Some(&b"pa@/: ss"[..]));
    assert_eq!(keyword.dbname.as_deref(), Some("app/db"));
    assert_eq!(keyword.options.as_deref(), Some("-c search_path=app"));
    assert_eq!(keyword.application_name.as_deref(), Some("oracle case"));
    assert_eq!(keyword.ssl_mode, "Require");
    assert_eq!(keyword.ssl_negotiation, "Direct");
    assert_eq!(
        keyword.hosts,
        [HostObservation::Tcp("2001:db8::1".to_owned())]
    );
    assert_eq!(keyword.hostaddrs, [Some("2001:db8::1".parse().unwrap())]);
    assert_eq!(keyword.raw_ports, [6543]);
    assert_eq!(keyword.effective_ports, [6543]);
    assert_eq!(keyword.connect_timeout, Some(Duration::from_secs(9)));
    assert_eq!(keyword.tcp_user_timeout, None);
    assert!(!keyword.keepalives);
    assert_eq!(keyword.keepalives_idle, Duration::from_secs(17));
    assert_eq!(keyword.keepalives_interval, Some(Duration::from_secs(7)));
    assert_eq!(keyword.keepalives_count, None);
    assert_eq!(keyword.target_session_attrs, "ReadWrite");
    assert_eq!(keyword.channel_binding, "Require");
    assert_eq!(keyword.load_balance_hosts, "Random");

    let ipv6 = result_named(results, "IPv6 without port");
    let (ParseOutcome::Accepted(ours_ipv6), ParseOutcome::Accepted(theirs_ipv6)) =
        (&ipv6.ours, &ipv6.theirs)
    else {
        panic!("both drivers must accept bracketed IPv6 without a port");
    };
    assert_eq!(
        ours_ipv6.hosts,
        [HostObservation::Tcp("2001:db8::2".to_owned())]
    );
    assert!(ours_ipv6.raw_ports.is_empty());
    assert_eq!(theirs_ipv6.raw_ports, [5432]);
    assert_eq!(ours_ipv6.effective_ports, [5432]);
    assert_eq!(theirs_ipv6.effective_ports, [5432]);

    let multiple = accepted_ours(results, "multiple hosts and ports");
    assert_eq!(multiple.hosts.len(), 2);
    assert_eq!(multiple.raw_ports, [5433, 5434]);
    assert_eq!(multiple.effective_ports, [5433, 5434]);
    let broadcast = accepted_ours(results, "one port for three hosts");
    assert_eq!(broadcast.hosts.len(), 3);
    assert_eq!(broadcast.raw_ports, [6000]);
    assert_eq!(broadcast.effective_ports, [6000, 6000, 6000]);
    let hostaddrs = accepted_ours(results, "multiple hostaddrs");
    assert_eq!(
        hostaddrs.hostaddrs,
        [
            Some("192.0.2.10".parse().unwrap()),
            Some("2001:db8::10".parse().unwrap())
        ]
    );

    assert_eq!(
        accepted_ours(results, "filesystem socket").hosts,
        [HostObservation::Unix(b"/var/run/postgresql".to_vec())]
    );
    #[cfg(target_os = "linux")]
    {
        assert_eq!(
            accepted_ours(results, "abstract socket").hosts,
            [HostObservation::Abstract(b"zeroship-oracle".to_vec())]
        );
        assert_eq!(
            accepted_theirs(results, "abstract socket").hosts,
            [HostObservation::Tcp("@zeroship-oracle".to_owned())]
        );
    }

    assert_acceptance(results, "empty dbname", true, false);
    assert_eq!(accepted_ours(results, "empty dbname").dbname, None);
    for rejected in [
        "missing value",
        "unknown parameter",
        "malformed port",
        "unterminated quote",
    ] {
        assert_acceptance(results, rejected, false, false);
    }
    assert_eq!(
        accepted_ours(results, "repeated scalar")
            .application_name
            .as_deref(),
        Some("last"),
        "the last repeated scalar must win"
    );
    let quoted = accepted_ours(results, "quoted escaped and spaced");
    assert_eq!(quoted.password.as_deref(), Some(&b"a'b\\c"[..]));
    assert_eq!(quoted.options.as_deref(), Some("-c search_path=public"));
    assert_eq!(quoted.application_name.as_deref(), Some("slash value"));

    for (name, variant) in [
        ("sslmode disable", "Disable"),
        ("sslmode prefer", "Prefer"),
        ("sslmode require", "Require"),
    ] {
        assert_eq!(accepted_ours(results, name).ssl_mode, variant);
        assert_eq!(accepted_theirs(results, name).ssl_mode, variant);
    }
    for (name, variant) in [
        ("sslmode allow", "Allow"),
        ("sslmode verify-ca", "VerifyCa"),
        ("sslmode verify-full", "VerifyFull"),
    ] {
        assert_acceptance(results, name, true, false);
        assert_eq!(accepted_ours(results, name).ssl_mode, variant);
    }

    for (name, variant) in [
        ("target any", "Any"),
        ("target read-write", "ReadWrite"),
        ("target read-only", "ReadOnly"),
    ] {
        assert_eq!(accepted_ours(results, name).target_session_attrs, variant);
        assert_eq!(accepted_theirs(results, name).target_session_attrs, variant);
    }
    for (name, variant) in [
        ("target primary", "Primary"),
        ("target standby", "Standby"),
        ("target prefer-standby", "PreferStandby"),
    ] {
        assert_acceptance(results, name, true, false);
        assert_eq!(accepted_ours(results, name).target_session_attrs, variant);
    }

    for (name, variant) in [
        ("channel binding disable", "Disable"),
        ("channel binding prefer", "Prefer"),
        ("channel binding require", "Require"),
    ] {
        assert_eq!(accepted_ours(results, name).channel_binding, variant);
        assert_eq!(accepted_theirs(results, name).channel_binding, variant);
    }
    for (name, variant) in [
        ("load balance disable", "Disable"),
        ("load balance random", "Random"),
    ] {
        assert_eq!(accepted_ours(results, name).load_balance_hosts, variant);
        assert_eq!(accepted_theirs(results, name).load_balance_hosts, variant);
    }

    let timeout = result_named(results, "tcp user timeout units");
    let (ParseOutcome::Accepted(ours_timeout), ParseOutcome::Accepted(theirs_timeout)) =
        (&timeout.ours, &timeout.theirs)
    else {
        panic!("both drivers must accept tcp_user_timeout");
    };
    assert_eq!(
        ours_timeout.tcp_user_timeout,
        Some(Duration::from_millis(5000))
    );
    assert_eq!(
        theirs_timeout.tcp_user_timeout,
        Some(Duration::from_secs(5000))
    );

    assert_acceptance(results, "libpq keepalive count spelling", true, false);
    assert_eq!(
        accepted_ours(results, "libpq keepalive count spelling").keepalives_count,
        Some(5)
    );
    assert_acceptance(results, "tokio keepalive retries spelling", false, true);
    assert_eq!(
        accepted_theirs(results, "tokio keepalive retries spelling").keepalives_count,
        Some(5)
    );
}

/// Every mutually exposed `Config` field either agrees by meaning or appears
/// in the exact, libpq-backed deliberate-divergence set above.
#[test]
fn every_shared_config_field_matches_tokio_or_a_documented_libpq_divergence() {
    let results: Vec<_> = cases().into_iter().map(parse).collect();
    let differences: Vec<_> = results.iter().flat_map(field_differences).collect();
    let expected = documented_divergences();

    let findings: Vec<_> = differences
        .iter()
        .filter(|difference| !expected.contains(&(difference.case, difference.field)))
        .collect();
    assert!(
        findings.is_empty(),
        "FINDING: connection-string parsers diverged outside the documented set:\n{}",
        findings
            .iter()
            .map(|difference| format!(
                "input={:?} field={} compio-postgres={} tokio-postgres={}",
                difference.input, difference.field, difference.ours, difference.theirs
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let actual: BTreeSet<_> = differences
        .iter()
        .map(|difference| (difference.case, difference.field))
        .collect();
    assert_eq!(
        actual, expected,
        "a documented divergence was not exercised, or changed classification"
    );
    assert_field_space_was_exercised(&results);
}
