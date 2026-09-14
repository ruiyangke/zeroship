#![deny(clippy::large_stack_arrays)]

use zeroship_data_orm::{orm::Entity, schema::LogicalType};

macro_rules! wide_schema {
    ($($field:ident),+ $(,)?) => {
        const FIELDS: &[&str] = &[$(stringify!($field)),+];
        zeroship_data_orm::orm::schema! {
            pub models {
                records {
                    #[orm(primary_key)] id: Text,
                    $($field: Text),+
                }
                nested {
                    #[orm(primary_key)] id: Text,
                    #[orm(shape($($field: Text),+))]
                    payload: Object,
                }
            }
        }
    };
}

wide_schema! {
    alpha, bravo, charlie, delta, echo, foxtrot, golf, hotel,
    india, juliet, kilo, lima, mike, november, oscar, papa,
    quebec, romeo, sierra, tango, uniform, victor, whiskey, xray,
    yankee, zulu,
}

#[test]
fn wide_metadata_preserves_order_types_and_cached_entity_identity() {
    let schema = models::schema();
    schema.validate().unwrap();
    let record = models::records::Entity::schema();
    assert!(std::ptr::eq(record, models::records::Entity::schema()));
    let expected = std::iter::once("id").chain(FIELDS.iter().copied());
    assert!(record.keys().map(String::as_str).eq(expected));
    let nested = models::nested::Entity::schema();
    assert!(nested["payload"]
        .shape
        .keys()
        .map(String::as_str)
        .eq(FIELDS.iter().copied()));
    for name in FIELDS {
        assert_eq!(record[*name].logical_type, LogicalType::Text);
        assert_eq!(
            nested["payload"].shape[*name].logical_type,
            LogicalType::Text
        );
    }
    assert_eq!(schema, models::schema());
}
