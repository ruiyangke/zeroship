use zeroship_data_orm::orm::*;

schema! {
    models {
        entries {
            #[orm(primary_key)]
            id: Text,
        }
    }
}

fn numeric(value: i64) -> Result<i64, DbError> {
    Ok(value)
}

#[derive(Insertable)]
#[orm(entity = models::entries)]
struct Entry {
    #[orm(encode_with = numeric)]
    id: i64,
}

fn main() {}
