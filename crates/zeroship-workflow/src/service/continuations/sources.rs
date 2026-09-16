use super::{
    models,
    records::{Generation, Head, Membership, Run},
    WorkflowServiceError,
};
use zeroship_data_orm::orm::{Database, EntityAlias, ReadBuilder, ReadSelection};

pub(super) type Joined = (Membership, Generation, Head, Membership, Generation, Run);

pub(super) struct Sources {
    pub accepted: EntityAlias<models::continuation_members::Entity>,
    pub accepted_generation: EntityAlias<models::generations::Entity>,
    pub head: EntityAlias<models::continuation_heads::Entity>,
    pub current: EntityAlias<models::continuation_members::Entity>,
    pub generation: EntityAlias<models::generations::Entity>,
    pub run: EntityAlias<models::runs::Entity>,
}

impl Sources {
    pub fn new(db: &Database) -> Result<Self, WorkflowServiceError> {
        Ok(Self {
            accepted: db
                .entity::<models::continuation_members::Entity>()?
                .alias("accepted")?,
            accepted_generation: db
                .entity::<models::generations::Entity>()?
                .alias("accepted_generation")?,
            head: db
                .entity::<models::continuation_heads::Entity>()?
                .alias("head")?,
            current: db
                .entity::<models::continuation_members::Entity>()?
                .alias("current_member")?,
            generation: db
                .entity::<models::generations::Entity>()?
                .alias("head_generation")?,
            run: db.entity::<models::runs::Entity>()?.alias("head_run")?,
        })
    }

    pub fn base(&self, db: &Database) -> Result<ReadBuilder, WorkflowServiceError> {
        use models::{continuation_heads as h, continuation_members as m, generations as g};
        Ok(db
            .from(&self.accepted)
            .inner_join(
                &self.accepted_generation,
                self.accepted
                    .column(m::app_id)
                    .eq(self.accepted_generation.column(g::app_id))?
                    .and(
                        self.accepted
                            .column(m::id)
                            .eq(self.accepted_generation.column(g::id))?,
                    ),
            )?
            .inner_join(
                &self.head,
                self.accepted
                    .column(m::app_id)
                    .eq(self.head.column(h::app_id))?
                    .and(
                        self.accepted
                            .column(m::head_id)
                            .eq(self.head.column(h::id))?,
                    ),
            )?)
    }

    pub fn read(&self, db: &Database) -> Result<ReadBuilder, WorkflowServiceError> {
        use models::{
            continuation_heads as h, continuation_members as m, generations as g, runs as r,
        };
        Ok(self
            .base(db)?
            .inner_join(
                &self.current,
                self.head
                    .column(h::app_id)
                    .eq(self.current.column(m::app_id))?
                    .and(
                        self.head
                            .column(h::id)
                            .eq(self.current.column(m::head_id))?,
                    )
                    .and(
                        self.head
                            .column(h::current_generation_id)
                            .eq(self.current.column(m::id))?,
                    )
                    .and(
                        self.head
                            .column(h::revision)
                            .eq(self.current.column(m::revision))?,
                    ),
            )?
            .inner_join(
                &self.generation,
                self.current
                    .column(m::app_id)
                    .eq(self.generation.column(g::app_id))?
                    .and(
                        self.current
                            .column(m::id)
                            .eq(self.generation.column(g::id))?,
                    ),
            )?
            .inner_join(
                &self.run,
                self.generation
                    .column(g::app_id)
                    .eq(self.run.column(r::app_id))?
                    .and(
                        self.generation
                            .column(g::run_id)
                            .eq(self.run.column(r::id))?,
                    )
                    .and(
                        self.generation
                            .column(g::generation)
                            .eq(self.run.column(r::generation))?,
                    ),
            )?)
    }

    pub fn selection(&self) -> impl ReadSelection<Output = Joined> {
        (
            self.accepted.row::<Membership>(),
            self.accepted_generation.row::<Generation>(),
            self.head.row::<Head>(),
            self.current.row::<Membership>(),
            self.generation.row::<Generation>(),
            self.run.row::<Run>(),
        )
    }
}
