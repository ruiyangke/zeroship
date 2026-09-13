macro_rules! updates_schema {
    ($visibility:vis $module:ident) => {
        ::zeroship_data_orm::orm::schema! {
            $visibility $module {
                documents {
                    #[orm(primary_key, max_length = 255, assign(on = insert, by = typed_id), writable = false)]
                    id: Text,
                    #[orm(assign(on = insert, by = now), writable = false)]
                    created_at: Timestamp,
                    #[orm(assign(on = write, by = now), writable = false)]
                    updated_at: Timestamp,
                    #[orm(max_length = 255, assign(on = insert, by = actor), writable = false)]
                    created_by: Nullable<Text>,
                    #[orm(max_length = 255, assign(on = write, by = actor), writable = false)]
                    updated_by: Nullable<Text>,
                    #[orm(default = 1, assign(on = write, by = increment(1)), writable = false, concurrency)]
                    version: Integer,
                    #[orm(assign(on = delete, by = now), writable = false, soft_delete)]
                    deleted_at: Nullable<Timestamp>,
                    label: Text,
                    payload: Json,
                }
            }
        }
    };
}
