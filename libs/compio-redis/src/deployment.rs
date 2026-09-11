use crate::{ClusterClient, Pool, PoolConfig, RedisConfig, Result, Topology};
use redis_protocol::resp2::types::OwnedFrame;

/// A deployment selected at runtime. Operations are routed to primaries.
#[derive(Clone)]
pub enum RedisClient {
    Standalone(Pool),
    Cluster(ClusterClient),
    Sentinel(Pool),
}

impl From<&crate::PoolSettings> for PoolConfig {
    fn from(settings: &crate::PoolSettings) -> Self {
        use std::time::Duration;
        Self {
            max_size: settings.max_size,
            min_idle: settings.min_idle,
            idle_timeout: Duration::from_millis(settings.idle_timeout_ms),
            liveness_probe_after: Duration::from_millis(settings.liveness_probe_after_ms),
        }
    }
}

impl RedisClient {
    pub async fn connect(config: &RedisConfig) -> Result<Self> {
        config.validate()?;
        match &config.topology {
            Topology::Standalone { endpoint } => Ok(Self::Standalone(
                Pool::connect_config(config.connection(endpoint.clone()), (&config.pool).into())
                    .await?,
            )),
            Topology::Cluster { .. } => {
                Ok(Self::Cluster(ClusterClient::connect_config(config).await?))
            }
            Topology::Sentinel { .. } => Ok(Self::Sentinel(
                Pool::sentinel(config.clone(), (&config.pool).into()).await?,
            )),
        }
    }

    /// Execute a command using its scoped routing key. Transport errors are
    /// returned without replay: the server may already have applied a mutation.
    pub async fn execute(&self, routing_key: &[u8], command: OwnedFrame) -> Result<OwnedFrame> {
        match self {
            Self::Cluster(client) => client.send_to_slot(routing_key, command).await,
            Self::Standalone(pool) => pool.acquire().await?.send_recv(command).await,
            Self::Sentinel(pool) => {
                let result = pool.acquire().await?.send_recv(command.clone()).await;
                if matches!(&result, Ok(OwnedFrame::Error(error)) if error.starts_with("READONLY "))
                {
                    pool.invalidate();
                    return pool.acquire().await?.send_recv(command).await;
                }
                if result.is_err() {
                    pool.invalidate();
                }
                result
            }
        }
    }
}

impl std::fmt::Debug for RedisClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisClient")
            .field(
                "topology",
                &match self {
                    Self::Standalone(_) => "standalone",
                    Self::Cluster(_) => "cluster",
                    Self::Sentinel(_) => "sentinel",
                },
            )
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Read,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    #[compio::test]
    async fn a_lost_mutation_reply_is_not_replayed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let mutations = Arc::new(AtomicUsize::new(0));
        let observed = mutations.clone();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut buffer = [0; 1024];
                        let received = stream.read(&mut buffer).unwrap();
                        assert!(received > 0);
                        observed.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            }
        });
        let client = RedisClient::connect(&RedisConfig::new(Topology::Standalone { endpoint }))
            .await
            .unwrap();
        assert!(
            client
                .execute(
                    b"counter",
                    crate::protocol::build_cmd(&[b"INCR", b"counter"])
                )
                .await
                .is_err()
        );
        server.join().unwrap();
        assert_eq!(mutations.load(Ordering::Relaxed), 1);
    }
}
