//! Native projections of Control-owned policy inputs, and the native models of
//! this service's own publication storage.
//!
//! Two declarations rather than one, because they are bound to two different
//! schemas: the inputs are Control's and are read under column grants, and the
//! publication tables are the workflow service's own.

zeroship_data_orm::orm::schema! {
    pub source {
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
    }
}

zeroship_data_orm::orm::schema! {
    pub publication {
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
