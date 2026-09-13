macro_rules! manual_id_schema {
    ($visibility:vis $module:ident) => {
        ::zeroship_data_orm::orm::schema! {
            $visibility $module {
                records {
                    #[orm(primary_key)]
                    id: Text,
                    label: Text,
                }
            }
        }
    };
}
