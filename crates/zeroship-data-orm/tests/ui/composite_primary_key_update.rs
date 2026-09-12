use zeroship_data_orm::orm::{Change, Changeset, schema};

schema!(pub models = "../fixtures/composite.runtime.json");

#[derive(Changeset)]
#[orm(entity = models::records)]
struct MoveRun {
    generation: Change<i64>,
}

fn main() {}
