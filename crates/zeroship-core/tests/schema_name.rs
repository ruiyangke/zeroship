use zeroship_core::schema_name::{SchemaName, SchemaNameError};

#[test]
fn physical_schema_identity_preserves_supported_names() {
    for name in [
        "public",
        "app_demo",
        "A-1",
        "_",
        "db_0191e7a2b3c44d5e8f90123456789abc",
    ] {
        let schema = SchemaName::new(name).unwrap();
        assert_eq!(schema.as_str(), name);
        assert_eq!(schema.clone(), schema);
    }
    assert_ne!(SchemaName::new("app-demo"), SchemaName::new("app_demo"));
}

#[test]
fn physical_schema_identity_rejects_invalid_input() {
    assert_eq!(SchemaName::new(""), Err(SchemaNameError::Empty));
    for name in [
        "app.public",
        "app id",
        "app\0id",
        "épp",
        "app\"; DROP SCHEMA public; --",
    ] {
        assert!(matches!(
            SchemaName::new(name),
            Err(SchemaNameError::Invalid(_))
        ));
    }
}
