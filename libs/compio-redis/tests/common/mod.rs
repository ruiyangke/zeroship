//! Testcontainers owns every Redis and Dragonfly test server.

#![allow(dead_code)]

use compio_redis as redis_types;
mod containers;
pub use containers::fixtures;

use compio_redis::{Client, Pool};

pub async fn connect(url: &str) -> Client {
    Client::connect(url)
        .await
        .expect("connect to owned Redis fixture")
}

pub async fn connect_pool(url: &str, size: usize) -> Pool {
    Pool::connect(url, size)
        .await
        .expect("connect pool to owned Redis fixture")
}
