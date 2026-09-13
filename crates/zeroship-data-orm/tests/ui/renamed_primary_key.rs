use zeroship_data_orm::orm::schema;

schema! {
    pub models {
        entries {
            #[orm(primary_key)]
            key: Text,
        }
    }
}

fn main() {}
