//! Native ORM mappings verified against the migration artifact in tests.

zeroship_data_orm::orm::schema! {
    pub journal {
        __zeroship_workflow_activation_scopes {
            #[orm(primary_key)]
            id: Text,
            activation_id: Text,
            revision: BigInt,
        }

        __zeroship_workflow_activations {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            deploy_id: Text,
            revision: BigInt,
        }

        __zeroship_workflow_advance_publications {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            deploy_id: Text,
            run_id: Text,
            generation: BigInt,
            frontier_revision: BigInt,
            available_at: BigInt,
        }

        __zeroship_workflow_app_state {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            #[orm(default = 0)]
            signal_epoch: BigInt,
            #[orm(default = 0)]
            last_polled_at: BigInt,
            #[orm(default = 0)]
            subscription_sequence: BigInt,
            #[orm(default = 0)]
            signal_sequence: BigInt,
            #[orm(default = 0)]
            closed_epoch: BigInt,
            #[orm(default = 1)]
            collection_revision: BigInt,
            collection_after_id: Nullable<Text>,
            collection_upper_id: Nullable<Text>,
            collection_observed_at: Nullable<BigInt>,
            #[orm(default = 1)]
            reconciliation_revision: BigInt,
            #[orm(default = "publications")]
            reconciliation_phase: Text,
            reconciliation_after_id: Nullable<Text>,
            reconciliation_upper_id: Nullable<Text>,
        }

        __zeroship_workflow_broadcasts {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            topic: Text,
            signal_type: Text,
            payload: Text,
            created_at: BigInt,
            cursor: BigInt,
            cutoff_sequence: BigInt,
            origin: Text,
            finished: BigInt,
            sequence: BigInt,
            revision: BigInt,
        }

        __zeroship_workflow_collection_pages {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            plan: Text,
            next_index: BigInt,
        }

        __zeroship_workflow_continuation_heads {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            current_generation_id: Text,
            revision: BigInt,
        }

        __zeroship_workflow_continuation_members {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            head_id: Text,
            revision: BigInt,
        }

        __zeroship_workflow_deployment_holds {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            deploy_id: Text,
            deploy_hash: Nullable<Text>,
            holder_id: Text,
            generation: BigInt,
            state: Text,
        }

        __zeroship_workflow_deploys {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            hash: Text,
            manifest: Text,
            created_at: BigInt,
            active: BigInt,
            state: Text,
            availability_epoch: BigInt,
        }

        __zeroship_workflow_fanout_pages {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            broadcast_id: Text,
            revision: BigInt,
            result: Text,
        }

        __zeroship_workflow_fanout_publications {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            broadcast_id: Text,
            revision: BigInt,
        }

        __zeroship_workflow_generations {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            run_id: Text,
            generation: BigInt,
            deploy_id: Text,
            input: Text,
            input_ref: Nullable<Text>,
            output: Nullable<Text>,
            output_ref: Nullable<Text>,
            error: Nullable<Text>,
            state: Text,
            started_at: BigInt,
            terminal_at: Nullable<BigInt>,
        }

        __zeroship_workflow_job_publications {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            specification: Text,
            created_at: BigInt,
            confirmed_at: Nullable<BigInt>,
        }

        __zeroship_workflow_job_receipts {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            run_id: Nullable<Text>,
            specification: Text,
            outcome: Nullable<Text>,
            created_at: BigInt,
            completed_at: Nullable<BigInt>,
        }

        __zeroship_workflow_management_receipts {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            run_id: Text,
            request_id: Text,
            revision: BigInt,
            outcome: Text,
            created_at: BigInt,
        }

        __zeroship_workflow_occurrences {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            schedule_id: Text,
            revision: BigInt,
            at: BigInt,
            job_id: Text,
            run_id: Nullable<Text>,
        }

        __zeroship_workflow_outbox {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            kind: Text,
            payload: Text,
            created_at: BigInt,
            delivered_at: Nullable<BigInt>,
        }

        __zeroship_workflow_payload_refs {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            run_id: Text,
            generation: BigInt,
            slot: Text,
            ordinal: BigInt,
            payload_id: Text,
        }

        __zeroship_workflow_payloads {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            run_id: Text,
            generation: BigInt,
            task_id: Text,
            request_id: Text,
            hash: Text,
            size: BigInt,
            content_type: Nullable<Text>,
            state: Text,
            created_at: BigInt,
            expires_at: BigInt,
        }

        __zeroship_workflow_propagation_pages {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            propagation_id: Text,
            revision: BigInt,
            result: Text,
        }

        __zeroship_workflow_propagation_publications {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            propagation_id: Text,
            revision: BigInt,
        }

        __zeroship_workflow_propagations {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            run_id: Text,
            generation: BigInt,
            kind: Text,
            cursor: Nullable<Text>,
            revision: BigInt,
            finished: BigInt,
            created_at: BigInt,
        }

        __zeroship_workflow_reconciliation_pages {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            plan: Text,
            next_index: BigInt,
        }

        __zeroship_workflow_requests {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            request_id: Text,
            operation: Text,
            digest: Text,
            result: Text,
            created_at: BigInt,
        }

        __zeroship_workflow_runs {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            workflow_name: Text,
            deploy_id: Text,
            generation: BigInt,
            state: Text,
            control: Text,
            due_at: Nullable<BigInt>,
            task_id: Nullable<Text>,
            lease_epoch: BigInt,
            #[orm(default = 1)]
            frontier_revision: BigInt,
            key: Nullable<Text>,
            parent_id: Nullable<Text>,
            parent_generation: Nullable<BigInt>,
            parent_ordinal: Nullable<BigInt>,
            cascade: BigInt,
            depth: BigInt,
            created_at: BigInt,
            terminal_at: Nullable<BigInt>,
            signal_epoch: BigInt,
            compensation_target: Nullable<Text>,
            schedule_id: Nullable<Text>,
        }

        __zeroship_workflow_schedules {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            name: Text,
        }

        __zeroship_workflow_schema_version {
            #[orm(primary_key)]
            id: Text,
            version: BigInt,
            fingerprint: Text,
        }

        __zeroship_workflow_signals {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            run_id: Text,
            signal_type: Text,
            payload: Text,
            created_at: BigInt,
            delivery_sequence: BigInt,
            consumed_generation: Nullable<BigInt>,
            consumed_ordinal: Nullable<BigInt>,
            broadcast_id: Nullable<Text>,
            #[orm(default = "app")]
            origin: Text,
            #[orm(default = "direct")]
            delivery: Text,
            topic: Nullable<Text>,
            target_generation: Nullable<BigInt>,
            target_ordinal: Nullable<BigInt>,
        }

        __zeroship_workflow_steps {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            run_id: Text,
            generation: BigInt,
            ordinal: BigInt,
            name: Text,
            occurrence: BigInt,
            origin_generation: BigInt,
            kind: Text,
            state: Text,
            record: Text,
            child_member_id: Nullable<Text>,
            child_result_member_id: Nullable<Text>,
            #[orm(default = 0)]
            compensation_attempts: BigInt,
            compensation_due_at: Nullable<BigInt>,
            compensation_error: Nullable<Text>,
            #[orm(default = 1000)]
            compensation_retry_ms: BigInt,
        }

        __zeroship_workflow_subscriptions {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            run_id: Text,
            generation: BigInt,
            ordinal: BigInt,
            topic: Text,
            created_at: BigInt,
            sequence: BigInt,
        }

        __zeroship_workflow_tasks {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            run_id: Text,
            generation: BigInt,
            worker: Text,
            epoch: BigInt,
            token_hash: Text,
            deadline: BigInt,
            state: Text,
            completion_digest: Nullable<Text>,
            receipt: Nullable<Text>,
            #[orm(default = 1)]
            frontier_revision: BigInt,
            job_id: Nullable<Text>,
            delivery_attempt: Nullable<BigInt>,
            assignment_revision: Nullable<BigInt>,
            created_at: BigInt,
            finished_at: Nullable<BigInt>,
        }

        __zeroship_workflow_topics {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            topic: Text,
            signal_epoch: BigInt,
            #[orm(default = 0)]
            accepted_sequence: BigInt,
            #[orm(default = 0)]
            completed_sequence: BigInt,
        }

        __zeroship_workflow_waits {
            #[orm(primary_key)]
            id: Text,
            app_id: Text,
            run_id: Text,
            generation: BigInt,
            ordinal: BigInt,
            kind: Text,
            signal_type: Nullable<Text>,
            topic: Nullable<Text>,
            max_signal_age: Nullable<BigInt>,
            due_at: Nullable<BigInt>,
        }

    }
}

#[cfg(test)]
mod tests {
    use super::journal;
    use zeroship_data_orm::{
        schema::{CollectionSchema, Schema},
        Value,
    };

    #[test]
    fn native_schema_matches_migration_metadata() {
        let artifact: Value =
            serde_json::from_str(zeroship_workflow_schema::RUNTIME_DESCRIPTOR_JSON).unwrap();
        let expected = Schema::from_runtime_descriptor(&artifact).unwrap();
        let native = journal::schema();
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

    #[test]
    fn reconciliation_metadata_keeps_page_state_off_the_receipt_and_phases_the_cursor() {
        use zeroship_data_orm::{orm::Entity, schema::LogicalType};
        let receipts = journal::__zeroship_workflow_job_receipts::Entity::schema();
        assert!(!receipts["run_id"].required);
        // The receipt carries no kind's state. Both paged sweeps keep the same
        // extension in their own table instead.
        for column in [
            "reconciliation",
            "reconciliation_next",
            "plan",
            "next_index",
        ] {
            assert!(!receipts.contains_key(column), "{column}");
        }
        for pages in [
            journal::__zeroship_workflow_collection_pages::Entity::schema(),
            journal::__zeroship_workflow_reconciliation_pages::Entity::schema(),
        ] {
            assert!(pages["id"].primary_key);
            assert_eq!(pages.values().filter(|field| field.primary_key).count(), 1);
            assert!(pages["app_id"].required);
            assert!(pages["plan"].required);
            assert_eq!(pages["plan"].logical_type, LogicalType::Text);
            assert!(pages["next_index"].required);
            assert_eq!(pages["next_index"].logical_type, LogicalType::BigInt);
        }
        let state = journal::__zeroship_workflow_app_state::Entity::schema();
        assert!(state["id"].primary_key);
        assert!(state["app_id"].required);
        for scan in ["collection", "reconciliation"] {
            let revision = &state[&format!("{scan}_revision")];
            assert!(revision.required);
            assert_eq!(revision.logical_type, LogicalType::BigInt);
            for cursor in ["after_id", "upper_id"] {
                let field = &state[&format!("{scan}_{cursor}")];
                assert!(!field.required);
                assert_eq!(field.logical_type, LogicalType::Text);
            }
        }
        assert!(state["reconciliation_phase"].required);
        assert_eq!(
            state["reconciliation_phase"].logical_type,
            LogicalType::Text
        );
        assert!(!state["collection_observed_at"].required);
        assert_eq!(
            state["collection_observed_at"].logical_type,
            LogicalType::BigInt
        );
        assert_eq!(state.values().filter(|field| field.primary_key).count(), 1);
    }

    #[test]
    fn continuation_metadata_binds_exact_generation_and_checkpoint_provenance() {
        use zeroship_data_orm::{orm::Entity, schema::LogicalType};
        let heads = journal::__zeroship_workflow_continuation_heads::Entity::schema();
        let members = journal::__zeroship_workflow_continuation_members::Entity::schema();
        for definition in [&heads, &members] {
            assert!(definition["id"].primary_key);
            assert!(definition["app_id"].required);
            assert!(definition["revision"].required);
            assert_eq!(definition["revision"].logical_type, LogicalType::BigInt);
            assert_eq!(
                definition
                    .values()
                    .filter(|field| field.primary_key)
                    .count(),
                1
            );
        }
        assert!(heads["current_generation_id"].required);
        assert!(members["head_id"].required);
        let steps = journal::__zeroship_workflow_steps::Entity::schema();
        for field in ["child_member_id", "child_result_member_id"] {
            assert!(!steps[field].required);
            assert_eq!(steps[field].logical_type, LogicalType::Text);
        }
        assert!(!journal::__zeroship_workflow_waits::Entity::schema().contains_key("child_id"));
        let runs = journal::__zeroship_workflow_runs::Entity::schema();
        assert!(!runs.contains_key("continued_from_id"));
        assert!(!runs.contains_key("continued_to_id"));
    }
}
