#![cfg(feature = "redis")]
#![allow(
    clippy::future_not_send,
    reason = "KV and driver operations execute on the owning compio thread."
)]

use compio_redis::{
    protocol::{build_cmd, expect_bulk_or_null},
    Client, OwnedFrame, RedisClient,
};
use std::time::Duration;
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use zeroship_kv::{Auth, KvConfig, KvStore, Namespace, RedisConfig, TlsConfig, Topology};

const PORT: u16 = 6379;

type Server = Container<GenericImage>;

fn start(dragonfly: bool, args: Vec<String>) -> Server {
    let (image, tag) = if dragonfly {
        ("docker.dragonflydb.io/dragonflydb/dragonfly", "latest")
    } else {
        ("redis", "7")
    };
    let wait = if dragonfly {
        WaitFor::message_on_stderr("listening on")
    } else {
        WaitFor::message_on_stdout("Ready to accept connections")
    };
    let mut command: Vec<String> = if dragonfly {
        vec![
            "--logtostderr".into(),
            "--proactor_threads=1".into(),
            "--maxmemory=256mb".into(),
        ]
    } else {
        vec![
            "redis-server".into(),
            "--protected-mode".into(),
            "no".into(),
        ]
    };
    command.extend(args);
    GenericImage::new(image, tag)
        .with_exposed_port(PORT.tcp())
        .with_wait_for(wait)
        .with_cmd(command)
        .start()
        .expect("start KV server")
}

fn endpoint(server: &Server) -> String {
    format!(
        "{}:{}",
        server.get_host().unwrap(),
        server.get_host_port_ipv4(PORT).unwrap()
    )
}

fn config(server: &Server) -> RedisConfig {
    RedisConfig::new(Topology::Standalone {
        endpoint: endpoint(server),
    })
}

async fn command(client: &mut Client, args: &[&str]) -> OwnedFrame {
    let args: Vec<_> = args.iter().map(|s| s.as_bytes()).collect();
    let frame = client.send_recv(build_cmd(&args)).await.unwrap();
    assert!(
        !matches!(&frame, OwnedFrame::Error(_)),
        "command rejected: {frame:?}"
    );
    frame
}

async fn contract(config: RedisConfig, name: &str) {
    let store = KvStore::open(&KvConfig::Redis { redis: config }).unwrap();
    let a = store.namespace(Namespace::app(&format!("{name}_a")).unwrap());
    let b = store.namespace(Namespace::app(&format!("{name}_b")).unwrap());
    a.set("shared", "a", None).await.unwrap();
    b.set("shared", "b", None).await.unwrap();
    assert_eq!(a.get("shared").await.unwrap().as_deref(), Some("a"));
    assert_eq!(b.get("shared").await.unwrap().as_deref(), Some("b"));
    assert!(!a.set_if_absent("shared", "wrong", None).await.unwrap());
    assert!(a.expire("shared", 60_000).await.unwrap());
    assert!(a.persist("shared").await.unwrap());
    assert!(a.delete("shared").await.unwrap());
    assert_eq!(b.get("shared").await.unwrap().as_deref(), Some("b"));
    assert_eq!(
        a.incr("counter", 9_007_199_254_740_993, Some(60_000))
            .await
            .unwrap(),
        9_007_199_254_740_993
    );
    assert_eq!(
        a.get("counter").await.unwrap().as_deref(),
        Some("9007199254740993")
    );
    let mut cursor = None;
    let mut keys = Vec::new();
    loop {
        let (page, next) = a.list("", cursor.as_deref(), 1).await.unwrap();
        keys.extend(page);
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(keys, vec!["counter"]);
}

#[compio::test]
async fn dragonfly_direct_and_emulated_cluster() {
    let server = start(true, vec!["--cluster_mode=emulated".into()]);
    contract(config(&server), "direct").await;
    let mut client = Client::connect(&format!("redis://{}", endpoint(&server)))
        .await
        .unwrap();
    command(
        &mut client,
        &["CONFIG", "SET", "cluster_announce_ip", "127.0.0.1"],
    )
    .await;
    command(
        &mut client,
        &[
            "CONFIG",
            "SET",
            "announce_port",
            &server.get_host_port_ipv4(PORT).unwrap().to_string(),
        ],
    )
    .await;
    contract(
        RedisConfig::new(Topology::Cluster {
            seeds: vec![endpoint(&server)],
        }),
        "emulated_cluster",
    )
    .await;
}

#[compio::test]
async fn redis_cluster_discovers_slots_and_refreshes_after_ownership_changes() {
    let servers: Vec<_> = (0..3)
        .map(|_| {
            start(
                false,
                vec![
                    "--requirepass".into(),
                    "cluster-secret".into(),
                    "--masterauth".into(),
                    "cluster-secret".into(),
                    "--cluster-enabled".into(),
                    "yes".into(),
                    "--cluster-node-timeout".into(),
                    "1000".into(),
                ],
            )
        })
        .collect();
    let mut clients = Vec::new();
    for server in &servers {
        clients.push(
            Client::connect(&format!("redis://:cluster-secret@{}", endpoint(server)))
                .await
                .unwrap(),
        );
    }
    let ranges = [(0, 5460), (5461, 10922), (10923, 16383)];
    for (client, (start, end)) in clients.iter_mut().zip(ranges) {
        command(
            client,
            &[
                "CLUSTER",
                "ADDSLOTSRANGE",
                &start.to_string(),
                &end.to_string(),
            ],
        )
        .await;
        for server in &servers {
            command(
                client,
                &[
                    "CLUSTER",
                    "MEET",
                    &server.get_bridge_ip_address().unwrap().to_string(),
                    "6379",
                ],
            )
            .await;
        }
    }
    compio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mut ready = true;
            for client in &mut clients {
                let info = expect_bulk_or_null(command(client, &["CLUSTER", "INFO"]).await)
                    .unwrap()
                    .unwrap();
                ready &= String::from_utf8(info)
                    .unwrap()
                    .contains("cluster_state:ok");
            }
            if ready {
                break;
            }
            compio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("cluster becomes ready");
    let mut seeds = vec!["127.0.0.1:1".into()];
    seeds.extend(servers.iter().map(endpoint));
    let mut config = RedisConfig::new(Topology::Cluster { seeds });
    config.auth.password = Some("cluster-secret".into());
    contract(config.clone(), "redis_cluster").await;
    let client = RedisClient::connect(&config).await.unwrap();
    let key = "{redis_moved}:key";
    let slot = compio_redis::protocol::expect_integer(
        command(&mut clients[0], &["CLUSTER", "KEYSLOT", key]).await,
    )
    .unwrap() as u16;
    let old = ranges
        .iter()
        .position(|(start, end)| (*start..=*end).contains(&slot))
        .unwrap();
    let next = (old + 1) % clients.len();
    let id = expect_bulk_or_null(command(&mut clients[next], &["CLUSTER", "MYID"]).await)
        .unwrap()
        .unwrap();
    let id = String::from_utf8(id).unwrap();
    for connection in &mut clients {
        command(
            connection,
            &["CLUSTER", "SETSLOT", &slot.to_string(), "NODE", &id],
        )
        .await;
    }
    let reply = client
        .execute(
            key.as_bytes(),
            build_cmd(&[b"SET", key.as_bytes(), b"moved"]),
        )
        .await
        .unwrap();
    assert!(!matches!(reply, OwnedFrame::Error(_)), "{reply:?}");
    assert_eq!(
        clients[next].get(key).await.unwrap().as_deref(),
        Some(b"moved".as_slice())
    );
}

#[compio::test]
async fn sentinel_discovers_and_follows_redis_and_dragonfly_primaries() {
    for dragonfly in [false, true] {
        let password = "data-secret";
        let args = if dragonfly {
            vec![format!("--requirepass={password}")]
        } else {
            vec!["--requirepass".into(), password.into()]
        };
        let primary = start(dragonfly, args.clone());
        let replica = start(dragonfly, args);
        let mut primary_config = config(&primary);
        primary_config.auth.password = Some(password.into());
        let mut replica_config = config(&replica);
        replica_config.auth.password = Some(password.into());
        let mut primary_client =
            Client::connect_config(&primary_config.connection(endpoint(&primary)))
                .await
                .unwrap();
        let mut replica_client =
            Client::connect_config(&replica_config.connection(endpoint(&replica)))
                .await
                .unwrap();
        command(
            &mut replica_client,
            &["CONFIG", "SET", "masterauth", password],
        )
        .await;
        command(
            &mut replica_client,
            &[
                "REPLICAOF",
                &primary.get_bridge_ip_address().unwrap().to_string(),
                "6379",
            ],
        )
        .await;
        primary_client
            .set("replication-marker", b"ready", None)
            .await
            .unwrap();
        compio::time::timeout(Duration::from_secs(30), async {
            loop {
                if replica_client
                    .get("replication-marker")
                    .await
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some(b"ready")
                {
                    break;
                }
                compio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("replica catches up");
        let sentinel_config = format!("port 6379\nbind 0.0.0.0\nprotected-mode no\nrequirepass sentinel-secret\nsentinel monitor kv {} 6379 1\nsentinel auth-pass kv {password}\nsentinel down-after-milliseconds kv 300\nsentinel failover-timeout kv 3000\n", primary.get_bridge_ip_address().unwrap());
        let sentinel = GenericImage::new("redis", "7")
            .with_exposed_port(PORT.tcp())
            .with_wait_for(WaitFor::message_on_stdout("+monitor"))
            .with_copy_to("/data/sentinel.conf", sentinel_config.into_bytes())
            .with_cmd(["redis-server", "/data/sentinel.conf", "--sentinel"])
            .start()
            .expect("start Sentinel");
        let mut deployment = RedisConfig::new(Topology::Sentinel {
            endpoints: vec!["127.0.0.1:1".into(), endpoint(&sentinel)],
            service_name: "kv".into(),
        });
        deployment.auth = Auth {
            username: Some("default".into()),
            password: Some(password.into()),
        };
        deployment.sentinel_auth = Auth {
            username: Some("default".into()),
            password: Some("sentinel-secret".into()),
        };
        let mut unknown = deployment.clone();
        if let Topology::Sentinel { service_name, .. } = &mut unknown.topology {
            *service_name = "unknown-service".into();
        }
        let error = RedisClient::connect(&unknown).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "redis: pool: Sentinel service_name is unknown"
        );
        let store = KvStore::open(&KvConfig::Redis {
            redis: deployment.clone(),
        })
        .unwrap();
        let app = store.namespace(Namespace::app("sentinel_a").unwrap());
        let other = store.namespace(Namespace::app("sentinel_b").unwrap());
        app.set("shared", "a", None).await.unwrap();
        other.set("shared", "b", None).await.unwrap();
        let mut sentinel_client = Client::connect_config(&compio_redis::ConnectionConfig {
            endpoint: endpoint(&sentinel),
            auth: deployment.sentinel_auth.clone(),
            tls: None,
            database: 0,
            timeouts: deployment.timeouts.clone(),
        })
        .await
        .unwrap();
        compio::time::timeout(Duration::from_secs(30), async {
            loop {
                let replicas = command(&mut sentinel_client, &["SENTINEL", "replicas", "kv"]).await;
                if matches!(&replicas, OwnedFrame::Array(items) if !items.is_empty())
                    && replica_client
                        .get("{sentinel_b}:shared")
                        .await
                        .ok()
                        .flatten()
                        .as_deref()
                        == Some(b"b")
                {
                    break;
                }
                compio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("Sentinel learns the synchronized replica");
        primary.stop().unwrap();
        compio::time::timeout(Duration::from_secs(40), async {
            loop {
                if app.get("shared").await.ok().flatten().as_deref() == Some("a") {
                    break;
                }
                compio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("existing KV store follows the promoted replica");
        app.set("shared", "after-failover", None).await.unwrap();
        assert_eq!(other.get("shared").await.unwrap().as_deref(), Some("b"));
        contract(
            deployment,
            if dragonfly {
                "dragonfly_sentinel"
            } else {
                "redis_sentinel"
            },
        )
        .await;
    }
}

#[compio::test]
async fn tls_and_acl_credentials_work_with_redis_and_dragonfly() {
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = certificate.cert.pem();
    let key = certificate.signing_key.serialize_pem();
    let directory = tempfile::tempdir().unwrap();
    let ca_file = directory.path().join("ca.pem");
    std::fs::write(&ca_file, &cert).unwrap();
    for dragonfly in [false, true] {
        let (image, tag) = if dragonfly {
            ("docker.dragonflydb.io/dragonflydb/dragonfly", "latest")
        } else {
            ("redis", "7")
        };
        let wait = if dragonfly {
            WaitFor::message_on_stderr("listening on")
        } else {
            WaitFor::message_on_stdout("Ready to accept connections")
        };
        let args = if dragonfly {
            vec![
                "--tls",
                "--cluster_mode=emulated",
                "--requirepass=bootstrap-secret",
                "--tls_cert_file=/tmp/cert.pem",
                "--tls_key_file=/tmp/key.pem",
                "--logtostderr",
                "--proactor_threads=1",
                "--maxmemory=256mb",
            ]
        } else {
            vec![
                "redis-server",
                "--requirepass",
                "bootstrap-secret",
                "--port",
                "0",
                "--tls-port",
                "6379",
                "--tls-cert-file",
                "/tmp/cert.pem",
                "--tls-key-file",
                "/tmp/key.pem",
                "--tls-ca-cert-file",
                "/tmp/cert.pem",
                "--tls-auth-clients",
                "no",
                "--protected-mode",
                "no",
            ]
        };
        let server = GenericImage::new(image, tag)
            .with_exposed_port(PORT.tcp())
            .with_wait_for(wait)
            .with_copy_to("/tmp/cert.pem", cert.as_bytes().to_vec())
            .with_copy_to("/tmp/key.pem", key.as_bytes().to_vec())
            .with_cmd(args)
            .start()
            .expect("start TLS server");
        let mut configuration = config(&server);
        configuration.auth.password = Some("bootstrap-secret".into());
        configuration.topology = Topology::Standalone {
            endpoint: format!("localhost:{}", server.get_host_port_ipv4(PORT).unwrap()),
        };
        configuration.tls = Some(TlsConfig {
            ca_file: Some(ca_file.clone()),
            ..TlsConfig::default()
        });
        let mut admin =
            Client::connect_config(&configuration.connection(match &configuration.topology {
                Topology::Standalone { endpoint } => endpoint.clone(),
                _ => unreachable!(),
            }))
            .await
            .unwrap();
        command(
            &mut admin,
            &[
                "ACL",
                "SETUSER",
                "kv-service",
                "on",
                ">acl-secret",
                "~*",
                "+@all",
            ],
        )
        .await;
        configuration.auth = Auth {
            username: Some("kv-service".into()),
            password: Some("acl-secret".into()),
        };
        contract(
            configuration.clone(),
            if dragonfly {
                "dragonfly_tls"
            } else {
                "redis_tls"
            },
        )
        .await;
        if dragonfly {
            command(
                &mut admin,
                &["CONFIG", "SET", "cluster_announce_ip", "127.0.0.1"],
            )
            .await;
            command(
                &mut admin,
                &[
                    "CONFIG",
                    "SET",
                    "announce_port",
                    &server.get_host_port_ipv4(PORT).unwrap().to_string(),
                ],
            )
            .await;
            let mut cluster = configuration.clone();
            cluster.topology = Topology::Cluster {
                seeds: vec![endpoint(&server)],
            };
            cluster.tls.as_mut().unwrap().server_name = Some("localhost".into());
            contract(cluster, "dragonfly_tls_cluster").await;
        }
        let Topology::Standalone { endpoint } = &configuration.topology else {
            unreachable!()
        };
        let mut connection = configuration.connection(endpoint.clone());
        connection.auth.password = Some("wrong-secret".into());
        let error = match Client::connect_config(&connection).await {
            Ok(_) => panic!("wrong credentials accepted"),
            Err(error) => error,
        };
        assert!(!error.to_string().contains("wrong-secret"));
        connection.auth = configuration.auth.clone();
        connection.tls.as_mut().unwrap().server_name = Some("wrong.example".into());
        assert!(
            Client::connect_config(&connection).await.is_err(),
            "certificate identity must be verified"
        );
    }
}
