fn serializable<T: serde::Serialize>() {}

fn main() {
    serializable::<zeroship_data_orm::sql::statement::Statement>();
}
