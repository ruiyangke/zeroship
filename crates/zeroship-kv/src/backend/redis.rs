//! Scoped operations over a runtime-selected Redis-compatible deployment.

use super::{classify_incr_error, scope, Backend, TtlState};
use crate::{limits::escape_glob, KvError};
use compio_redis::{
    protocol::{build_cmd, expect_array, expect_bulk_or_null, expect_integer, expect_ok},
    OwnedFrame, RedisClient, RedisConfig,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Weak,
    },
};

const INCR_TTL_SCRIPT: &str = "local e=redis.call('EXISTS',KEYS[1]); \
redis.call('INCRBY',KEYS[1],ARGV[1]); \
if e==0 and ARGV[2]~='' then redis.call('PEXPIRE',KEYS[1],ARGV[2]) end; \
return redis.call('GET',KEYS[1])";

static NEXT_STORE: AtomicU64 = AtomicU64::new(1);

type CachedClient = (Weak<()>, RedisClient);
thread_local! {
    static CLIENTS: RefCell<HashMap<u64, CachedClient>> = RefCell::new(HashMap::new());
}

#[derive(Debug)]
pub struct Redis {
    config: RedisConfig,
    id: u64,
    lifetime: Arc<()>,
}

impl Redis {
    pub fn new(config: RedisConfig) -> Self {
        Self {
            config,
            id: NEXT_STORE.fetch_add(1, Ordering::Relaxed),
            lifetime: Arc::new(()),
        }
    }

    async fn client(&self) -> Result<RedisClient, KvError> {
        if let Some(client) = CLIENTS.with(|clients| {
            let mut clients = clients.borrow_mut();
            clients.retain(|_, (owner, _)| owner.strong_count() != 0);
            clients.get(&self.id).map(|(_, client)| client.clone())
        }) {
            return Ok(client);
        }
        let client = RedisClient::connect(&self.config)
            .await
            .map_err(map_error)?;
        // Another future may have initialized this store while discovery awaited.
        Ok(CLIENTS.with(|clients| {
            clients
                .borrow_mut()
                .entry(self.id)
                .or_insert_with(|| (Arc::downgrade(&self.lifetime), client))
                .1
                .clone()
        }))
    }

    async fn execute(&self, routing_key: &str, args: &[&[u8]]) -> Result<OwnedFrame, KvError> {
        self.client()
            .await?
            .execute(routing_key.as_bytes(), build_cmd(args))
            .await
            .map_err(map_error)
    }
}

fn map_error(error: compio_redis::Error) -> KvError {
    use compio_redis::Error;
    match error {
        Error::Server(message) => classify_incr_error(&message),
        Error::Io(_)
        | Error::Pool(_)
        | Error::Auth(_)
        | Error::Config(_)
        | Error::ClusterBootstrap(_)
        | Error::NoRoute { .. } => KvError::connection(error.to_string()),
        _ => KvError::backend(error.to_string()),
    }
}

#[async_trait::async_trait(?Send)]
impl Backend for Redis {
    async fn get(&self, app_id: &str, key: &str) -> Result<Option<String>, KvError> {
        let key = scope(app_id, key);
        let frame = self.execute(&key, &[b"GET", key.as_bytes()]).await?;
        Ok(expect_bulk_or_null(frame)
            .map_err(map_error)?
            .map(|value| String::from_utf8_lossy(&value).into_owned()))
    }

    async fn set(
        &self,
        app_id: &str,
        key: &str,
        value: &str,
        ttl_ms: Option<u64>,
    ) -> Result<(), KvError> {
        let key = scope(app_id, key);
        let ttl = ttl_ms.map(|ttl| ttl.to_string());
        let mut args: Vec<&[u8]> = vec![b"SET", key.as_bytes(), value.as_bytes()];
        if let Some(ttl) = &ttl {
            args.extend([b"PX".as_slice(), ttl.as_bytes()]);
        }
        expect_ok(self.execute(&key, &args).await?).map_err(map_error)
    }

    async fn delete(&self, app_id: &str, key: &str) -> Result<bool, KvError> {
        let key = scope(app_id, key);
        Ok(
            expect_integer(self.execute(&key, &[b"DEL", key.as_bytes()]).await?)
                .map_err(map_error)?
                != 0,
        )
    }

    async fn incr(
        &self,
        app_id: &str,
        key: &str,
        delta: i64,
        ttl_ms: Option<u64>,
    ) -> Result<i64, KvError> {
        let key = scope(app_id, key);
        let delta = delta.to_string();
        let ttl = ttl_ms.map(|ttl| ttl.to_string()).unwrap_or_default();
        let frame = self
            .execute(
                &key,
                &[
                    b"EVAL",
                    INCR_TTL_SCRIPT.as_bytes(),
                    b"1",
                    key.as_bytes(),
                    delta.as_bytes(),
                    ttl.as_bytes(),
                ],
            )
            .await?;
        let value = expect_bulk_or_null(frame)
            .map_err(map_error)?
            .ok_or_else(|| KvError::backend("incr returned no value"))?;
        std::str::from_utf8(&value)
            .ok()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| KvError::backend("incr returned an invalid integer"))
    }

    async fn set_if_absent(
        &self,
        app_id: &str,
        key: &str,
        value: &str,
        ttl_ms: Option<u64>,
    ) -> Result<bool, KvError> {
        let key = scope(app_id, key);
        let ttl = ttl_ms.map(|ttl| ttl.to_string());
        let mut args: Vec<&[u8]> = vec![b"SET", key.as_bytes(), value.as_bytes(), b"NX"];
        if let Some(ttl) = &ttl {
            args.extend([b"PX".as_slice(), ttl.as_bytes()]);
        }
        match self.execute(&key, &args).await? {
            OwnedFrame::Null => Ok(false),
            frame => expect_ok(frame).map(|()| true).map_err(map_error),
        }
    }

    async fn expire(&self, app_id: &str, key: &str, ttl_ms: u64) -> Result<bool, KvError> {
        let key = scope(app_id, key);
        Ok(expect_integer(
            self.execute(
                &key,
                &[b"PEXPIRE", key.as_bytes(), ttl_ms.to_string().as_bytes()],
            )
            .await?,
        )
        .map_err(map_error)?
            != 0)
    }

    async fn ttl(&self, app_id: &str, key: &str) -> Result<TtlState, KvError> {
        let key = scope(app_id, key);
        Ok(
            match expect_integer(self.execute(&key, &[b"PTTL", key.as_bytes()]).await?)
                .map_err(map_error)?
            {
                -2 => TtlState::Missing,
                -1 => TtlState::NoExpiry,
                value => TtlState::ExpiresInMs(value.max(0) as u64),
            },
        )
    }

    async fn persist(&self, app_id: &str, key: &str) -> Result<bool, KvError> {
        let key = scope(app_id, key);
        Ok(
            expect_integer(self.execute(&key, &[b"PERSIST", key.as_bytes()]).await?)
                .map_err(map_error)?
                != 0,
        )
    }

    async fn list(
        &self,
        app_id: &str,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<String>), KvError> {
        let app_prefix = scope(app_id, "");
        let pattern = format!("{app_prefix}{}*", escape_glob(prefix));
        let count = limit.min(u32::MAX as usize).to_string();
        let frame = self
            .execute(
                &app_prefix,
                &[
                    b"SCAN",
                    cursor.unwrap_or("0").as_bytes(),
                    b"MATCH",
                    pattern.as_bytes(),
                    b"COUNT",
                    count.as_bytes(),
                ],
            )
            .await?;
        let mut response = expect_array(frame).map_err(map_error)?.into_iter();
        let next = expect_bulk_or_null(
            response
                .next()
                .ok_or_else(|| KvError::backend("missing scan cursor"))?,
        )
        .map_err(map_error)?
        .ok_or_else(|| KvError::backend("null scan cursor"))?;
        let next = String::from_utf8(next).map_err(|_| KvError::backend("invalid scan cursor"))?;
        let entries = expect_array(
            response
                .next()
                .ok_or_else(|| KvError::backend("missing scan keys"))?,
        )
        .map_err(map_error)?;
        let mut keys = Vec::new();
        for entry in entries {
            if let Some(bytes) = expect_bulk_or_null(entry).map_err(map_error)? {
                let key = String::from_utf8_lossy(&bytes);
                if let Some(key) = key
                    .strip_prefix(&app_prefix)
                    .filter(|key| key.starts_with(prefix))
                {
                    keys.push(key.to_owned());
                }
            }
        }
        Ok((keys, (next != "0").then_some(next)))
    }
}
