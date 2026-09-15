//! Native schema and domain conversions owned by the auth service.

use chrono::{DateTime, Utc};
use zeroship_core::UserId;
use zeroship_data_orm::{
    binding::DbBinding,
    encryption::ProjectKeySource,
    orm::{Database, DbError, UtcInstant},
    sql::SchemaName,
    ConnectOptions,
};

zeroship_data_orm::orm::schema! {
    pub models {
        signing_keys {
            #[orm(primary_key, assign(on = insert, by = identity))]
            id: BigInt,
            #[orm(unique)]
            kid: Text,
            alg: Text,
            public_jwk: Json,
            status: Text,
            #[orm(assign(on = insert, by = now))]
            created_at: Timestamp,
            activated_at: Nullable<Timestamp>,
            retiring_at: Nullable<Timestamp>,
            retired_at: Nullable<Timestamp>,
            max_issued_expires_at: Nullable<Timestamp>,
        }
        users {
            #[orm(primary_key)]
            id: Text,
            #[orm(case_sensitive = false, unique)]
            email: Text,
            email_verified_at: Nullable<Timestamp>,
            name: Text,
            avatar_url: Nullable<Text>,
            password_hash: Nullable<Text>,
            #[orm(default = 0)]
            credential_version: BigInt,
            locked_until: Nullable<Timestamp>,
            disabled_at: Nullable<Timestamp>,
            #[orm(assign(on = insert, by = now), writable = false)]
            created_at: Timestamp,
            #[orm(assign(on = write, by = now), writable = false)]
            updated_at: Timestamp,
            last_login_at: Nullable<Timestamp>,
            #[orm(default = 0)]
            failed_login_count: Integer,
            deletion_requested_at: Nullable<Timestamp>,
            deletion_scheduled_for: Nullable<Timestamp>,
            anonymized_at: Nullable<Timestamp>,
        }
    }
}

/// Open auth's native ORM using the configured database role.
///
/// # Errors
/// Returns connection or schema configuration errors.
#[allow(
    clippy::future_not_send,
    reason = "the ORM owns a thread-local compio pool"
)]
pub async fn connect(url: &str) -> Result<Database, DbError> {
    Database::connect(
        DbBinding::new("platform", "auth", SchemaName::new("zeroship")?),
        ConnectOptions::new(url, ProjectKeySource::unavailable()).connection_authority(),
        models::schema(),
    )
    .await
}

pub(crate) fn user_id(value: String) -> Result<UserId, DbError> {
    UserId::parse_owned(value)
        .map_err(|_| DbError::validation("invalid_user_id", "invalid stored user identity"))
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "ORM conversion hooks return Result"
)]
pub(crate) fn user_id_text(value: UserId) -> Result<String, DbError> {
    Ok(value.into_string())
}

/// Auth owns its own chrono conversion; the ORM carries the instant untyped by
/// any auth concept, at the microsecond resolution PostgreSQL stores.
pub(crate) fn instant(value: UtcInstant) -> Result<DateTime<Utc>, DbError> {
    DateTime::from_timestamp_micros(value.unix_micros())
        .ok_or_else(|| DbError::validation("invalid_timestamp", "invalid stored timestamp"))
}

pub(crate) fn optional_instant(
    value: Option<UtcInstant>,
) -> Result<Option<DateTime<Utc>>, DbError> {
    value.map(instant).transpose()
}

/// The inverse, for a value auth binds back into a predicate or an assignment.
pub(crate) fn instant_value(value: DateTime<Utc>) -> Result<UtcInstant, DbError> {
    UtcInstant::from_unix_micros(value.timestamp_micros())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A microsecond survives the conversion in both directions, which is what
    /// a reservation token compared for equality depends on.
    #[test]
    fn instant_conversions_keep_every_microsecond() {
        for micros in [-1_i64, 0, 1, 1_000_001, -1_000_001] {
            let stored = UtcInstant::from_unix_micros(micros).unwrap();
            let converted = instant(stored).unwrap();
            assert_eq!(converted.timestamp_micros(), micros, "{micros}");
            assert_eq!(instant_value(converted).unwrap(), stored, "{micros}");
            assert_eq!(optional_instant(Some(stored)).unwrap(), Some(converted));
        }
        assert_eq!(optional_instant(None).unwrap(), None);
        // Rejection control: an instant outside the portable calendar has no
        // conversion in either direction.
        assert!(
            instant_value(DateTime::from_timestamp_micros(-62_135_596_800_000_001).unwrap())
                .is_err()
        );
    }
}
