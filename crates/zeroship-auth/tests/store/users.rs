//! User identity constraints retain their structured database errors.

use crate::common::database::Database;
use zeroship_auth::store::users;

#[compio::test]
async fn duplicate_email_preserves_the_existing_user_and_reports_unique_violation() {
    Database::run(async |database| {
        let pg = database.connect_as_auth().await;
        let first = users::create(&pg, "creator@example.test", "Original creator", None)
            .await
            .unwrap();
        let error = users::create(&pg, "CREATOR@EXAMPLE.TEST", "Duplicate creator", None)
            .await
            .unwrap_err();
        assert_eq!(error.db_code(), Some("23505"));
        let retained = users::find_by_email(&pg, "creator@example.test")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.id, first.id);
        assert_eq!(retained.name, first.name);
        let count: i64 = pg
            .query_one("SELECT COUNT(*) FROM zeroship.users", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, 1);
    })
    .await;
}
