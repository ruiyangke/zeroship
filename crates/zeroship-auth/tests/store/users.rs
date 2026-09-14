//! User identity constraints retain their structured database errors.

use crate::common::database::Database;
use zeroship_auth::store::users;
use zeroship_core::UserId;
use zeroship_data_orm::orm::{Entity, Insertable};

#[test]
fn native_user_insert_preserves_the_owned_id_buffer() {
    let id = UserId::mint();
    let buffer = id.as_str().as_ptr();
    let record = users::NewUser {
        id,
        email: "native@example.test",
        name: "Native caller",
        password_hash: None,
    }
    .into_record()
    .unwrap();
    assert_eq!(record["id"].as_str().unwrap().as_ptr(), buffer);
}

#[compio::test]
async fn native_user_rows_apply_the_callers_identity_conversion() {
    Database::run(async |database| {
        let native = zeroship_auth::store::native::connect(database.auth_url().as_str())
            .await
            .unwrap();
        use zeroship_auth::store::native::models::users as model;
        let id = UserId::mint();
        let created: users::UserRow = native
            .entity::<model::Entity>()
            .unwrap()
            .insert(users::NewUser {
                id: id.clone(),
                email: "native@example.test",
                name: "Native caller",
                password_hash: None,
            })
            .await
            .unwrap();
        assert_eq!(created.id, id);
        let user = native
            .entity::<model::Entity>()
            .unwrap()
            .query()
            .filter(model::id.eq(created.id.as_str()).unwrap())
            .first::<users::UserRow>()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user.id, created.id);
        assert_eq!(user.email, created.email);
        assert_eq!(user.name, created.name);
        assert_eq!(model::Entity::COLLECTION, "users");
        let locked_until = chrono::Utc::now().timestamp_millis() + 60_000;
        let updated: Option<users::UserRow> = native
            .entity::<model::Entity>()
            .unwrap()
            .update(
                model::id.eq(id.as_str()).unwrap(),
                model::locked_until.set(Some(locked_until)).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            updated.unwrap().locked_until,
            chrono::DateTime::from_timestamp_millis(locked_until)
        );
    })
    .await;
}

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
