use zeroship_data_orm::orm::*;

schema! {
    models {
        entries {
            #[orm(primary_key)]
            id: Text,
            label: Nullable<Text>,
        }
    }
}

fn required_text(value: String) -> Result<String, DbError> {
    Ok(value)
}

#[derive(FromRow)]
#[orm(entity = models::entries)]
struct Entry {
    #[orm(decode_with = required_text)]
    label: String,
}

fn main() {}
