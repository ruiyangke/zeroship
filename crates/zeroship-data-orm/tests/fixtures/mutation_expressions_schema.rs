macro_rules! mutation_expressions_schema {
    ($visibility:vis $module:ident) => {
        ::zeroship_data_orm::orm::schema! {
            $visibility $module {
                counters {
                    #[orm(primary_key)]
                    id: Text,
                    quantity: BigInt,
                    ratio: Number,
                    optional: Nullable<BigInt>,
                    tags: Array<Text>,
                    moments: Array<Timestamp>,
                    dates: Array<CalendarDate>,
                    document: Json,
                    #[orm(default = 1, assign(on = write, by = increment(1)), writable = false)]
                    version: Integer,
                }
            }
        }
    };
}
