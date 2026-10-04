//! Native ORM mappings verified against the migration artifact in tests.

zeroship_data_orm::orm::schema! {
    pub schema {
        capacity_targets {
            #[orm(primary_key)]
            id: Text,
            #[orm(default = 0)]
            revision: BigInt,
            #[orm(default = 0)]
            desired: BigInt,
            #[orm(default = "steady")]
            state: Text,
            refusal: Nullable<Text>,
            #[orm(default = 0)]
            backlog_depth: BigInt,
            oldest_available_at: Nullable<BigInt>,
            #[orm(default = 0)]
            exhausted_jobs: BigInt,
            #[orm(default = 0)]
            backed_off_jobs: BigInt,
            #[orm(default = 0)]
            withheld_jobs: BigInt,
            #[orm(default = 0)]
            attempt: BigInt,
            attempt_deadline: Nullable<BigInt>,
            retry_at: Nullable<BigInt>,
            below_since: Nullable<BigInt>,
            #[orm(default = 0)]
            lock_version: BigInt,
        }

        deployment_holds {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            deployment_id: Text,
            holder_id: Text,
            deploy_hash: Nullable<Text>,
            generation: BigInt,
            state: Text,
            held_at: Nullable<BigInt>,
            #[orm(default = "pending")]
            journal_state: Text,
            journal_job_id: Nullable<Text>,
            journal_published_at: Nullable<BigInt>,
        }

        jobs {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            deployment_id: Nullable<Text>,
            operation: Text,
            operation_kind: Text,
            management_request_id: Nullable<Text>,
            run_id: Nullable<Text>,
            spec_digest: Text,
            available_at: BigInt,
            dispatch_order: BigInt,
            state: Text,
            #[orm(default = 0)]
            attempt: BigInt,
            #[orm(default = 0)]
            execution_attempts: BigInt,
            executed_attempt: Nullable<BigInt>,
            worker_id: Nullable<Text>,
            lease_deadline: Nullable<BigInt>,
            leased_at: Nullable<BigInt>,
            deferred_until: Nullable<BigInt>,
            #[orm(default = 0)]
            deferrals: BigInt,
            outcome: Nullable<Text>,
            settlement_digest: Nullable<Text>,
            created_at: BigInt,
        }

        management {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            request_id: Text,
            run_id: Text,
            revision: BigInt,
            actor: Text,
            request: Text,
            request_digest: Text,
            blocks_execution: Boolean,
            created_at: BigInt,
            outcome: Nullable<Text>,
        }

        management_scopes {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            run_id: Text,
            accepted_revision: BigInt,
            settled_revision: BigInt,
        }

        queue_scopes {
            #[orm(primary_key)]
            id: Text,
            execution_zone_id: Text,
            #[orm(default = 0)]
            lock_version: BigInt,
            #[orm(default = 0)]
            dispatch_cursor: BigInt,
        }

        recovery_duties {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            kind: Text,
            next_due_at: BigInt,
            pending_job_id: Nullable<Text>,
        }

        recovery_scopes {
            #[orm(primary_key)]
            id: Text,
            deployment_id: Text,
            activation_revision: BigInt,
            ingress_epoch: BigInt,
            state: Text,
            closing_watermark: Nullable<BigInt>,
            close_job_id: Nullable<Text>,
            active_at: BigInt,
            close_after: Nullable<BigInt>,
            #[orm(default = 0)]
            close_attempts: BigInt,
        }

        schedule_activations {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            deployment_id: Text,
            revision: BigInt,
            activated_at: BigInt,
        }

        schedule_deployments {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            definition: Text,
            interpretation: Text,
            created_at: BigInt,
        }

        schedule_disables {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            revision: BigInt,
            created_at: BigInt,
        }

        schedule_occurrences {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            schedule_id: Text,
            revision: BigInt,
            scheduled_at: BigInt,
            run_id: Text,
            job_id: Text,
            activation_id: Text,
        }

        schedule_scopes {
            #[orm(primary_key)]
            id: Text,
            revision: BigInt,
            enabled: Boolean,
            activation_id: Nullable<Text>,
        }

        schedules {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            name: Text,
            activation_id: Text,
            revision: BigInt,
            definition: Text,
            next_at: Nullable<BigInt>,
            anchor_at: BigInt,
            catch_up_until: Nullable<BigInt>,
            catch_up_remaining: Nullable<BigInt>,
        }

        schema_version {
            #[orm(primary_key)]
            id: Text,
            fingerprint: Text,
        }

    }
}

#[cfg(test)]
mod tests {
    use super::schema;
    use zeroship_data_orm::{
        schema::{CollectionSchema, Schema},
        Value,
    };

    #[test]
    fn native_schema_matches_migration_metadata() {
        let artifact: Value =
            serde_json::from_str(include_str!("../../schema/schema.runtime.json")).unwrap();
        let expected = Schema::from_runtime_descriptor(&artifact).unwrap();
        let native = schema::schema();
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
