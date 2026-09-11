//! Docker fixtures shared with the independent Redis driver tests.

use zeroship_kv as redis_types;

#[allow(dead_code)]
#[path = "../../../../libs/compio-redis/tests/common/containers.rs"]
mod containers;

pub use containers::{fixtures, standalone, start_redis};
