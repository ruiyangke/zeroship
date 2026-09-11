use crate::{
    Client, ConnectionConfig, Error, OwnedFrame, RedisConfig, Result, Topology,
    config::parse_endpoint,
    protocol::{build_cmd, expect_array, expect_bulk_or_null},
};

#[derive(Clone, Debug)]
pub(crate) enum Source {
    Direct(ConnectionConfig),
    Sentinel(RedisConfig),
}

impl Source {
    pub(crate) fn is_sentinel(&self) -> bool {
        matches!(self, Self::Sentinel(_))
    }

    pub(crate) async fn connect(&self) -> Result<(Client, String)> {
        match self {
            Self::Direct(config) => Ok((
                Client::connect_config(config).await?,
                config.endpoint.clone(),
            )),
            Self::Sentinel(config) => discover(config).await,
        }
    }
}

async fn discover(config: &RedisConfig) -> Result<(Client, String)> {
    let Topology::Sentinel {
        endpoints,
        service_name,
    } = &config.topology
    else {
        unreachable!()
    };
    let mut answered = false;
    let mut unknown_service = false;
    for endpoint in endpoints {
        let connection = ConnectionConfig {
            endpoint: endpoint.clone(),
            auth: config.sentinel_auth.clone(),
            tls: config.sentinel_tls.clone(),
            database: 0,
            timeouts: config.timeouts.clone(),
        };
        let Ok(mut sentinel) = Client::connect_config(&connection).await else {
            continue;
        };
        let Ok(frame) = sentinel
            .send_recv(build_cmd(&[
                b"SENTINEL",
                b"get-master-addr-by-name",
                service_name.as_bytes(),
            ]))
            .await
        else {
            continue;
        };
        if matches!(frame, OwnedFrame::Null) {
            unknown_service = true;
            continue;
        }
        let Ok(items) = expect_array(frame) else {
            continue;
        };
        answered = true;
        if items.len() != 2 {
            continue;
        }
        let mut parts = items.into_iter();
        let (Some(host), Some(port)) = (
            expect_bulk_or_null(parts.next().unwrap())?,
            expect_bulk_or_null(parts.next().unwrap())?,
        ) else {
            continue;
        };
        let host = String::from_utf8(host)
            .map_err(|_| Error::Protocol("invalid Sentinel address".into()))?;
        let port =
            String::from_utf8(port).map_err(|_| Error::Protocol("invalid Sentinel port".into()))?;
        let endpoint = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        parse_endpoint(&endpoint)?;
        let Ok(mut primary) = Client::connect_config(&config.connection(endpoint.clone())).await
        else {
            continue;
        };
        if primary.require_primary().await.is_ok() {
            return Ok((primary, endpoint));
        }
    }
    Err(Error::Pool(
        if answered {
            "Sentinel did not resolve a reachable primary"
        } else if unknown_service {
            "Sentinel service_name is unknown"
        } else {
            "Sentinel discovery is unavailable"
        }
        .into(),
    ))
}
