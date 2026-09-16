//! Native ORM mappings verified against the migration artifact in tests.

zeroship_data_orm::orm::schema! {
    pub models {
        app_deploy_holds {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            deploy_id: Text,
            holder_id: Text,
            generation: BigInt,
            state: Text,
        }

        app_deploys {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            deploy_hash: Text,
            manifest_json: Text,
            created_at: Timestamp,
            activated_at: Nullable<Timestamp>,
            #[orm(default = "available")]
            retention_state: Text,
            #[orm(default = 0)]
            retention_lock: BigInt,
        }

    }
}

#[cfg(test)]
mod tests {
    use super::models;
    use zeroship_data_orm::{
        Value,
        schema::{CollectionSchema, Schema},
    };

    #[test]
    fn native_schema_matches_migration_metadata() {
        let artifact: Value =
            serde_json::from_str(include_str!("../../schema/deployments/schema.runtime.json"))
                .unwrap();
        let expected = Schema::from_runtime_descriptor(&artifact).unwrap();
        let native = models::schema();
        expected.validate().unwrap();
        native.validate().unwrap();
        assert!(native.collections().next().is_some());
        assert_eq!(native, expected);

        let mut changed = native.into_collections();
        let mut fields = changed[0].1.clone().into_fields();
        fields.get_mut("id").unwrap().required = false;
        changed[0].1 = CollectionSchema::new(fields);
        let changed = Schema::new(changed);
        assert_ne!(changed, expected);
        assert!(changed.validate().is_err());
    }
}
