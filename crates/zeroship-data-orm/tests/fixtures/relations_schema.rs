macro_rules! relations_schema {
    ($visibility:vis $module:ident) => {
        ::zeroship_data_orm::orm::schema! {
            $visibility $module {
                authors {
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
                    name: Text,
                    #[orm(unique)]
                    handle: Text,
                    #[orm(unique)]
                    serial: BigInt,
                    payload: Bytes,
                }
                posts {
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
                    title: Text,
                    #[orm(references(authors::id), relation(author))]
                    authorId: Nullable<Text>,
                    #[orm(references(authors::handle), relation(authorByHandle))]
                    authorHandle: Nullable<Text>,
                    #[orm(references(authors::serial), relation(authorBySerial))]
                    authorSerial: Nullable<BigInt>,
                }
            }
        }
    };
}
