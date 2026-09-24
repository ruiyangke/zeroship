//! Native models of this service's own publication storage, and of the one
//! Control table an operator credential provisions plan policy into.
//!
//! Two declarations rather than one, because they are bound to two different
//! schemas under two different credentials: the publication tables are the
//! workflow service's own, and the plan row belongs to Control and is reached
//! only by the administrative credential `PlanPolicyStore` takes. The service's
//! serving path binds neither Control table: its policy inputs arrive over
//! Control's app-facts endpoint.

zeroship_data_orm::orm::schema! {
    pub plan_admin {
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
            // `source_watermark` is where Control's source stood when the
            // inputs behind `revision` were read. The publication fence refuses
            // an observation below it; see the comparison in `store::publish`.
            source_watermark: Nullable<BigInt>,
        }
    }
}
