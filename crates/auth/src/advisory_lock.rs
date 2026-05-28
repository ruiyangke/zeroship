//! PostgreSQL advisory locks for auth multi-instance coordination.

use std::future::Future;

use compio_postgres::Client;

use crate::error::{AuthError, Result};

/// Stable process-wide lock for first-boot hydra signing-key bootstrap.
pub const BOOTSTRAP_SIGNING_KEYS_LOCK: i64 = 0x0042_B007_A071_0001;

/// Run `f` while holding a session-scoped PostgreSQL advisory lock.
///
/// The same [`Client`] must be used for lock, work, and unlock: PostgreSQL
/// advisory locks are scoped to the physical session.
pub async fn with_advisory_lock<F, Fut, R>(conn: &Client, key: i64, f: F) -> Result<R>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<R>>,
{
    acquire_advisory_lock(conn, key).await?;

    let result = f().await;
    let unlock = release_advisory_lock(conn, key).await;

    match (result, unlock) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(err), Ok(())) => Err(err),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), Err(unlock_err)) => {
            tracing::error!(
                error = %unlock_err,
                lock_key = key,
                "pg_advisory_unlock failed after locked operation error"
            );
            Err(err)
        }
    }
}

async fn acquire_advisory_lock(conn: &Client, key: i64) -> Result<()> {
    conn.execute("SELECT pg_advisory_lock($1)", &[&key])
        .await
        .map_err(|e| AuthError::Db(format!("pg_advisory_lock({key}): {e}")))?;
    Ok(())
}

async fn release_advisory_lock(conn: &Client, key: i64) -> Result<()> {
    conn.execute("SELECT pg_advisory_unlock($1)", &[&key])
        .await
        .map_err(|e| AuthError::Db(format!("pg_advisory_unlock({key}): {e}")))?;
    Ok(())
}

/// Stable i64 advisory-lock key for one hydra JWK set.
#[must_use]
pub fn jwk_set_lock_key(set: &str) -> i64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for byte in b"zeroship-auth:jwk-rotation:"
        .iter()
        .copied()
        .chain(set.bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    i64::from_ne_bytes(hash.to_ne_bytes())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;

    use compio_postgres::{connect, Client, NoTls};

    use super::with_advisory_lock;
    use crate::error::AuthError;

    async fn pg_connect(dsn: &str) -> Client {
        let (client, conn) = connect(dsn, NoTls).await.expect("connect");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        client
    }

    #[compio::test]
    async fn advisory_lock_serializes_same_key_across_sessions() {
        let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
            eprintln!("skip: AUTH_DB_URL unset");
            return;
        };

        let client_a = pg_connect(&dsn).await;
        let client_b = pg_connect(&dsn).await;
        let lock_key = 0x0042_B007_A071_2001_i64;
        let first_inside = Arc::new(AtomicBool::new(false));
        let overlapped = Arc::new(AtomicBool::new(false));

        let first_inside_a = first_inside.clone();
        let holder = compio::runtime::spawn(async move {
            with_advisory_lock(&client_a, lock_key, || async {
                first_inside_a.store(true, Ordering::SeqCst);
                compio::time::sleep(Duration::from_millis(150)).await;
                first_inside_a.store(false, Ordering::SeqCst);
                Ok::<(), AuthError>(())
            })
            .await
        });

        for _ in 0..100 {
            if first_inside.load(Ordering::SeqCst) {
                break;
            }
            compio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            first_inside.load(Ordering::SeqCst),
            "first task should enter locked section before contender starts"
        );

        let first_inside_b = first_inside.clone();
        let overlapped_b = overlapped.clone();
        let contender = compio::runtime::spawn(async move {
            with_advisory_lock(&client_b, lock_key, || async {
                if first_inside_b.load(Ordering::SeqCst) {
                    overlapped_b.store(true, Ordering::SeqCst);
                }
                Ok::<(), AuthError>(())
            })
            .await
        });

        holder.await.expect("join holder").expect("holder lock");
        contender
            .await
            .expect("join contender")
            .expect("contender lock");
        assert!(
            !overlapped.load(Ordering::SeqCst),
            "second session entered while first session still held the advisory lock"
        );
    }
}
