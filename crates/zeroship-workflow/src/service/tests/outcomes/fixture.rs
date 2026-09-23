use super::*;
use crate::service::tests::objects::Objects;

pub(super) struct Lease {
    pub(super) delivery: Delivery,
    expires: Instant,
}

impl Lease {
    pub(super) fn job(job: JobSpec) -> Self {
        Self {
            delivery: Delivery {
                job,
                worker_id: WorkerId::mint(),
                assignment_revision: 1.try_into().unwrap(),
                attempt: 1.try_into().unwrap(),
                deadline: 0.try_into().unwrap(),
            },
            expires: Instant::now() + Duration::from_secs(3600),
        }
    }

    fn operation(app: &AppId, operation: JobOperation) -> Self {
        Self::job(JobSpec {
            id: JobId::mint(),
            app_id: app.clone(),
            operation,
            available_at: 0.try_into().unwrap(),
        })
    }
}

impl JobLease for Lease {
    fn delivery(&self) -> &Delivery {
        &self.delivery
    }
    fn remaining(&self) -> Option<Duration> {
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
    }
}

pub(super) struct Publisher {
    app: AppId,
    pub(super) calls: Cell<usize>,
}

impl JobPublisher for Publisher {
    fn app_id(&self) -> &AppId {
        &self.app
    }
    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        assert_eq!(job.app_id, self.app);
        self.calls.set(self.calls.get() + 1);
        Ok(job.clone())
    }
}

pub(super) struct Case {
    pub(super) lease: Lease,
    pub(super) receipt: JobReceipt,
    pub(super) forbidden: Vec<JobOutcome>,
    pub(super) wrong_linkage: Vec<JobOutcome>,
}

impl Case {
    pub(super) async fn replay(
        &self,
        fixture: &Fixture,
    ) -> Result<JobReceipt, WorkflowServiceError> {
        let scope = &fixture.app;
        match &self.lease.delivery.job.operation {
            JobOperation::Activate { .. } => scope.activate_job(&self.lease).await,
            JobOperation::Cron { .. } => scope.cron_job(&self.lease, &fixture.objects).await,
            JobOperation::Reconcile {} => {
                scope
                    .reconcile_job(
                        &self.lease,
                        &fixture.publisher,
                        ReconciliationOptions::default(),
                    )
                    .await
            }
            JobOperation::Advance { .. } => match scope.accept_job(&self.lease).await? {
                JobAcceptance::Settled(receipt) => Ok(*receipt),
                other => panic!("completed job must never create another task: {other:?}"),
            },
            _ => panic!("only implemented creator operations belong in fixture"),
        }
    }
}

pub(super) struct Fixture {
    pub(super) service: WorkflowService,
    pub(super) app: AppWorkflows,
    /// Where a schedule activation puts the run input it stages.
    pub(super) objects: Objects,
    pub(super) publisher: Publisher,
    pub(super) cases: Vec<Case>,
    _platform: Deployments,
}

impl Fixture {
    pub(super) async fn new(store: Rc<OrmStore>) -> Self {
        let (service, app, _, platform) = registered_service(store).await;
        let scoped = service.fixture_app(app.clone());
        let deployment = platform
            .publish(&app, &scheduled(), &Sources::default())
            .await
            .unwrap();
        let deployment_id = DeploymentId::parse(&deployment.id).unwrap();
        let activation = Lease::operation(
            &app,
            JobOperation::Activate {
                deployment_id: deployment_id.clone(),
                revision: 1.try_into().unwrap(),
            },
        );
        let receipt = scoped.activate_job(&activation).await.unwrap();
        let mut cases = vec![Case {
            lease: activation,
            receipt,
            forbidden: vec![JobOutcome::Waiting {}, JobOutcome::Rejected {}],
            wrong_linkage: vec![],
        }];
        let objects = Objects::new();
        cases.extend(cron_cases(&scoped, &deployment_id, &objects).await);
        let lease = Lease::job(scoped.pending_jobs(None, 1).await.unwrap().remove(0));
        let JobAcceptance::Execute(task) = scoped.accept_job(&lease).await.unwrap() else {
            panic!("accepted cron must publish executable work")
        };
        let receipt = scoped
            .complete_job(&task, &lease, execution(json!([{"kind":"RunCompleted"}])))
            .await
            .unwrap();
        cases.push(Case {
            lease,
            receipt,
            forbidden: vec![],
            wrong_linkage: vec![],
        });
        let publisher = Publisher {
            app: app.clone(),
            calls: Cell::new(0),
        };
        for expected in [JobOutcome::Waiting {}, JobOutcome::Completed {}] {
            let lease = Lease::operation(&app, JobOperation::Reconcile {});
            let receipt = scoped
                .reconcile_job(&lease, &publisher, ReconciliationOptions::default())
                .await
                .unwrap();
            assert_eq!(receipt.outcome, expected);
            cases.push(Case {
                lease,
                receipt,
                forbidden: vec![JobOutcome::Rejected {}],
                wrong_linkage: vec![],
            });
        }
        Self {
            service,
            app: scoped,
            objects,
            publisher,
            cases,
            _platform: platform,
        }
    }
}

fn scheduled() -> DeployRegistration {
    DeployRegistration {
        id: DeploymentId::mint().as_str().to_owned(),
        hash: String::new(),
        workflows: ["Example".into()].into(),
        schedules: vec![ScheduleRegistration {
            name: "periodic".into(),
            workflow_name: "Example".into(),
            schedule: ScheduleTiming::Interval {
                interval_ms: 60_000,
                anchor: IntervalAnchor::Epoch,
            },
            input: json!({"scheduled":true}),
            overlap: ScheduleOverlap::SkipIfRunning,
            catch_up: crate::service::ScheduleCatchUp::default(),
        }],
    }
}

async fn cron_cases(
    scope: &AppWorkflows,
    deployment_id: &DeploymentId,
    objects: &Objects,
) -> Vec<Case> {
    let mut cases = Vec::new();
    let schedule_id = ScheduleId::mint();
    for (instant, expected) in [
        (1000, JobOutcome::Completed {}),
        (2000, JobOutcome::Rejected {}),
    ] {
        let lease = Lease::operation(
            scope.app_id(),
            JobOperation::Cron {
                deployment_id: deployment_id.clone(),
                schedule_id: schedule_id.clone(),
                schedule_name: "periodic".into(),
                request_id: RequestId::mint(),
                run_id: RunId::mint(),
                revision: 1.try_into().unwrap(),
                scheduled_at: instant.try_into().unwrap(),
            },
        );
        let receipt = scope.cron_job(&lease, objects).await.unwrap();
        assert_eq!(receipt.outcome, expected);
        let opposite = match expected {
            JobOutcome::Completed {} => JobOutcome::Rejected {},
            _ => JobOutcome::Completed {},
        };
        cases.push(Case {
            lease,
            receipt,
            forbidden: vec![JobOutcome::Waiting {}],
            wrong_linkage: vec![opposite],
        });
    }
    cases
}
