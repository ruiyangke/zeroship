use super::fixtures::CollectionFixture;
use super::*;

async fn projection(postgres: bool, column: &str, unmask: bool) {
    let keys = std::sync::Arc::new(crate::encryption::SuppliedProjectKeys::new());
    keys.insert_hex("projection_fixture", &"2".repeat(64))
        .unwrap();
    let fields = value!({
        "label":{"type":"string"},
        "secret":{"type":"string","encrypted":true},
        "masked":{"type":"string","mask":{"kind":"last4","classification":"pii"},
            "storage":{"valueColumn":"masked","rawColumn":"__zs_raw__masked"}}
    });
    let source = ProjectKeySource::supplied(keys.clone());
    let owner = if postgres {
        CollectionFixture::postgres_with_keys("records", fields, source).await
    } else {
        CollectionFixture::sqlite_with_keys("records", fields, source).await
    };
    keys.bind_app(owner.database.binding.app_id(), "projection_fixture")
        .unwrap();
    owner
        .database
        .install_mask_policy(value!({"support":["pii"]}))
        .unwrap();
    let records = owner.database.collection("records").unwrap();
    let Output::Rows { rows, .. } = records
        .insert(value!({
            "label":"record", "secret":"private", "masked":"12345678"
        }))
        .await
        .unwrap()
    else {
        panic!("expected rows")
    };
    let id = rows[0]["id"].clone();
    let mut options = value!({"select":[column]});
    if unmask {
        options["unmask"] = value!([column]);
        options["actor"] = value!({"kind":"support","id":"usr_reader"});
        options["unmaskReason"] = value!("projection regression");
    }
    let Output::Rows { rows, .. } = records
        .find(value!({"id":id.clone()}), options)
        .await
        .unwrap()
    else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].as_object().unwrap().len(),
        1,
        "internal columns must stay hidden"
    );
    if unmask {
        assert_eq!(rows[0][column], value!("12345678"));
    } else if column == "secret" {
        assert_eq!(rows[0][column], value!("private"));
    } else {
        assert_eq!(rows[0][column]["_meta"]["row_pk"], id);
        assert_eq!(rows[0][column]["_meta"]["column"], value!(column));
    }
    owner.close().await;
}

#[compio::test]
async fn sqlite_encrypted_projection() {
    projection(false, "secret", false).await;
}
#[compio::test]
async fn postgres_encrypted_projection() {
    projection(true, "secret", false).await;
}
#[compio::test]
async fn sqlite_masked_projection() {
    projection(false, "masked", false).await;
}
#[compio::test]
async fn postgres_masked_projection() {
    projection(true, "masked", false).await;
}
#[compio::test]
async fn sqlite_unmasked_projection() {
    projection(false, "masked", true).await;
}
#[compio::test]
async fn postgres_unmasked_projection() {
    projection(true, "masked", true).await;
}
