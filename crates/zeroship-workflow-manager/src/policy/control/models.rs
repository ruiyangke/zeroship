//! Native projections of Control-owned policy inputs and publication metadata.

zeroship_data_orm::orm::schema! {
    pub schema {
        apps {
            #[orm(primary_key)]
            id: Text,
            plan_id: Text,
            workflows_enabled: Boolean,
            archived_at: Nullable<Timestamp>,
        }
        plans {
            #[orm(primary_key)]
            id: Text,
            workflows_allowed: Boolean,
            archived: Boolean,
            workflow_policy_json: Nullable<Json>,
        }
        workflow_rollout_config {
            #[orm(primary_key)]
            id: Text,
            dispatch_paused: Boolean,
            ingress_disabled: Boolean,
            source_validity_ms: BigInt,
        }
        workflow_policy_ledger {
            #[orm(primary_key)]
            id: Text,
            #[orm(default = 0)]
            revision: BigInt,
            policy_json: Nullable<Json>,
            source_validity_ms: Nullable<BigInt>,
        }
    }
}
