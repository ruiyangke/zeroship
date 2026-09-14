use zeroship_data_orm::orm::*;

schema! {
    models {
        entries {
            #[orm(primary_key)]
            id: Text,
        }
    }
}

fn text(value: String) -> Result<String, DbError> {
    Ok(value)
}

#[derive(Changeset)]
#[orm(entity = models::entries)]
struct Entry {
    #[orm(encode_with = text)]
    id: Change<String>,
}

fn main() {}
