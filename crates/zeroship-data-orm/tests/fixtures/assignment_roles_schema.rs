macro_rules! assignment_roles_schema {
    ($visibility:vis $module:ident) => {
        ::zeroship_data_orm::orm::schema! {
            $visibility $module {
                entries {
                    #[orm(assign(on = insert, by = typed_id))]
                    entry_key: Text,
                    #[orm(primary_key)]
                    id: Text,
                    created_at: Text,
                    version: Integer,
                }
            }
        }
    };
}
