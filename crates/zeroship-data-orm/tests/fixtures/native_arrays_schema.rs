macro_rules! native_arrays_schema {
    ($visibility:vis $module:ident) => {
        ::zeroship_data_orm::orm::schema! {
            $visibility $module {
                grants {
                    #[orm(primary_key)]
                    id: Text,
                    #[orm(array_storage = "native")]
                    scopes: Array<Text>,
                    #[orm(array_storage = "native")]
                    amr: Nullable<Array<Text>>,
                    tags: Array<Text>,
                }
            }
        }
    };
}
