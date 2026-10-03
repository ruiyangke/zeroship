//! The placement row a live-placement contract authenticates against.
//!
//! Included by this crate's end-to-end target and by `zeroship-control`'s
//! deployment-hold suite, which seed the row the manager's selection lane would
//! have committed. The in-process workflow-server target places apps through
//! the manager and compiles no placement seeding.

use super::platform::Platform;

impl Platform {
    /// The placement row the manager's selection lane would commit, seeded
    /// directly. Contracts that place apps through selection use the manager;
    /// the job, policy and registry endpoints need only a live placement to
    /// authenticate against, and selection needs claimable work this fixture
    /// has no reason to publish.
    pub async fn seed_placement(
        &self,
        app: &zeroship_core::AppId,
        worker: &zeroship_core::workflow_coordination::WorkerId,
        ttl: std::time::Duration,
    ) -> zeroship_core::workflow_coordination::Assignment {
        let expires = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
                + ttl.as_millis(),
        )
        .unwrap();
        self.admin
            .execute(
                "INSERT INTO workflow_manager.queue_scopes(id) VALUES($1) ON CONFLICT DO NOTHING",
                &[&app.as_str()],
            )
            .await
            .unwrap();
        let inserted = self
            .admin
            .execute(
                "INSERT INTO workflow_manager.assignments\
                 (id,app_id,worker_id,revision,expires_at,released,refused) \
                 VALUES($1,$2,$3,1,$4,false,false)",
                &[
                    &zeroship_core::typed_id::generate("wca"),
                    &app.as_str(),
                    &worker.as_str(),
                    &expires,
                ],
            )
            .await
            .unwrap();
        assert_eq!(inserted, 1);
        zeroship_core::workflow_coordination::Assignment {
            app_id: app.clone(),
            worker_id: worker.clone(),
            revision: 1.try_into().unwrap(),
            expires_at: expires.try_into().unwrap(),
        }
    }
}
