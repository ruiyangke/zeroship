//! Drive the real signup budget and isolate it from elapsed fixture time.

use super::fixtures::*;
use crate::common::{CapturingMailer, auth_server::AuthServer, database::Database};
use std::sync::Arc;
use zeroship_authn::rate_limit::Quota;

#[ntex::test]
#[allow(
    clippy::float_cmp,
    reason = "refill is frozen and the declared burst balance is integral"
)]
async fn signup_exhausts_only_its_ip_budget_and_recovers_after_refill() {
    Database::run(async |database| {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        let quota = Quota::SIGNUP_IP;
        assert!(quota.capacity.is_finite() && quota.capacity >= 1.0 && quota.capacity.fract() == 0.0);
        let form = Form::get(&server, "/signup").await;
        let expected = assert_redirect(form.submit(&server, "first@example.test", NAME, IP).await, "/me").await;
        let rows = server.pg.query("SELECT bucket_key FROM zeroship.rate_limits", &[]).await.unwrap();
        let [row] = rows.as_slice() else { panic!("signup must create its IP budget: {rows:?}") };
        let bucket: String = row.get(0);
        let mut accepted: u32 = 1;
        loop {
            let balance: f64 = server.pg.query_one(
                "SELECT tokens::DOUBLE PRECISION FROM zeroship.rate_limits WHERE bucket_key = $1", &[&bucket],
            ).await.unwrap().get(0);
            assert_eq!(balance, quota.capacity - f64::from(accepted));
            let updated = server.pg.execute(
                "UPDATE zeroship.rate_limits SET updated_at = NOW() + INTERVAL '1 day' WHERE bucket_key = $1", &[&bucket],
            ).await.unwrap();
            assert_eq!(updated, 1);
            if f64::from(accepted) >= quota.capacity { break; }
            let form = Form::get(&server, "/signup").await;
            let email = format!("creator-{accepted}@example.test");
            assert_eq!(assert_redirect(form.submit(&server, &email, NAME, IP).await, "/me").await, expected);
            accepted += 1;
        }
        assert_counts(&server, i64::from(accepted), i64::from(accepted)).await;
        assert_eq!(mailer.sent().len(), usize::try_from(accepted).unwrap());
        let form = Form::get(&server, "/signup").await;
        assert_eq!(assert_redirect(form.submit(&server, "throttled@example.test", NAME, IP).await, "/me").await, expected);
        assert_counts(&server, i64::from(accepted), i64::from(accepted)).await;
        assert_eq!(mailer.sent().len(), usize::try_from(accepted).unwrap());

        let form = Form::get(&server, "/signup").await;
        assert_redirect(form.submit(&server, "another-ip@example.test", NAME, "192.0.2.2").await, "/me").await;
        created(&server, "another-ip@example.test").await;
        let refill_seconds = (quota.capacity + 1.0) / quota.refill_per_sec;
        assert!(refill_seconds.is_finite() && refill_seconds > 0.0);
        let updated = server.pg.execute(
            "UPDATE zeroship.rate_limits \
             SET updated_at = NOW() - $2::DOUBLE PRECISION * INTERVAL '1 second' WHERE bucket_key = $1",
            &[&bucket, &refill_seconds],
        ).await.unwrap();
        assert_eq!(updated, 1);
        let form = Form::get(&server, "/signup").await;
        assert_redirect(form.submit(&server, "throttled@example.test", NAME, IP).await, "/me").await;
        created(&server, "throttled@example.test").await;
        assert_counts(&server, i64::from(accepted) + 2, i64::from(accepted) + 2).await;
        assert_eq!(mailer.sent().len(), usize::try_from(accepted).unwrap() + 2);
        verification_link(&server, &mailer, "throttled@example.test");
    }).await;
}
