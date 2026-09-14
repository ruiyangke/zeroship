use super::{DbError, Type, Value, decode_value, row_to_value};
use compio_postgres::test_utils::{column_for_test, row_for_test};
use std::net::IpAddr;

fn binary(address: &str, prefix: u8, cidr: bool) -> Vec<u8> {
    let (family, address) = match address.parse::<IpAddr>().unwrap() {
        IpAddr::V4(address) => (2, address.octets().to_vec()),
        IpAddr::V6(address) => (3, address.octets().to_vec()),
    };
    [
        family,
        prefix,
        u8::from(cidr),
        u8::try_from(address.len()).unwrap(),
    ]
    .into_iter()
    .chain(address)
    .collect()
}

#[test]
fn network_binary_preserves_addresses_and_prefixes() {
    for (ty, address, prefix, expected) in [
        (Type::INET, "192.0.2.129", 24, "192.0.2.129/24"),
        (Type::INET, "192.0.2.129", 0, "192.0.2.129/0"),
        (Type::INET, "192.0.2.129", 32, "192.0.2.129"),
        (Type::INET, "2001:db8::1234", 64, "2001:db8::1234/64"),
        (Type::INET, "2001:db8::1234", 0, "2001:db8::1234/0"),
        (Type::INET, "2001:db8::1234", 128, "2001:db8::1234"),
        (Type::CIDR, "192.0.2.128", 25, "192.0.2.128/25"),
        (Type::CIDR, "192.0.2.129", 32, "192.0.2.129/32"),
        (Type::CIDR, "0.0.0.0", 0, "0.0.0.0/0"),
        (Type::CIDR, "2001:db8::", 64, "2001:db8::/64"),
        (Type::CIDR, "2001:db8::1234", 128, "2001:db8::1234/128"),
        (Type::CIDR, "::", 0, "::/0"),
    ] {
        let bytes = binary(address, prefix, ty == Type::CIDR);
        assert_eq!(decode_value(&ty, &bytes).unwrap(), Value::from(expected));
    }
}

#[test]
fn network_null_is_distinct_from_malformed_binary() {
    for ty in [Type::INET, Type::CIDR] {
        let row = row_for_test(vec![column_for_test("address", ty.clone())], vec![None]).unwrap();
        assert_eq!(row_to_value(&row).unwrap()["address"], Value::Null);
        for (address, prefix) in [("192.0.2.0", 24), ("2001:db8::", 64)] {
            let valid = binary(address, prefix, ty == Type::CIDR);
            assert!(decode_value(&ty, &valid).is_ok());
            let mut invalid: Vec<_> = (0..valid.len())
                .map(|length| valid[..length].to_vec())
                .collect();
            let mut trailing = valid.clone();
            trailing.push(0);
            invalid.push(trailing);
            for (index, byte) in [
                (0, 255),
                (1, 255),
                (2, 2),
                (2, u8::from(ty != Type::CIDR)),
                (3, 0),
            ] {
                let mut changed = valid.clone();
                changed[index] = byte;
                invalid.push(changed);
            }
            if ty == Type::CIDR {
                let mut host_bits = valid;
                *host_bits.last_mut().unwrap() = 1;
                invalid.push(host_bits);
            }
            for bytes in invalid {
                let row = row_for_test(
                    vec![column_for_test("address", ty.clone())],
                    vec![Some(bytes)],
                )
                .unwrap();
                let error = row_to_value(&row).unwrap_err();
                assert_eq!(error.code(), "row_decode_failed");
                let DbError::Coded { message, .. } = error else {
                    panic!("expected row decoding failure")
                };
                assert!(message.contains("address"));
                assert!(!message.contains("192.0.2"));
                assert!(!message.contains("2001:db8"));
            }
        }
    }
}

crate::orm::schema! {
    pub network_schema {
        network_values {
            #[orm(primary_key)] id: Text,
            address: Nullable<Text>,
            subnet: Nullable<Text>,
        }
    }
}

#[derive(Debug, PartialEq, crate::orm::FromRow)]
#[orm(entity = network_schema::network_values)]
struct NetworkValue {
    id: String,
    address: Option<String>,
    subnet: Option<String>,
}

#[derive(crate::orm::Insertable)]
#[orm(entity = network_schema::network_values)]
struct NewNetworkValue<'a> {
    id: &'a str,
    address: Option<&'a str>,
    subnet: Option<&'a str>,
}

#[test]
fn postgres_network_columns_roundtrip_native_text() {
    use crate::{
        ConnectOptions, binding::DbBinding, encryption::ProjectKeySource, orm::Database,
        sql::SchemaName,
    };
    let postgres = crate::tests::fixtures::postgres::Postgres::start();
    crate::tests::fixtures::Host::test(|host| {
        host.run(async {
            let url = postgres.url();
            let pool = compio_postgres::Pool::connect(&url, 1).await.unwrap();
            pool.batch_execute("CREATE TABLE public.network_values (id TEXT PRIMARY KEY, address INET, subnet CIDR)").await.unwrap();
            let db = Database::connect(
                DbBinding::new("network_fixture", "network_fixture", SchemaName::new("public").unwrap()),
                ConnectOptions::new(&url, ProjectKeySource::unavailable()).connection_authority(),
                network_schema::schema(),
            ).await.unwrap();
            let table = db.entity::<network_schema::network_values::Entity>().unwrap();
            for (id, address, subnet) in [
                ("ipv4", Some("192.0.2.129/24"), Some("192.0.2.128/25")),
                ("ipv6", Some("2001:db8::1234/64"), Some("2001:db8::/64")),
                ("host", Some("2001:db8::1234"), Some("2001:db8::1234/128")),
                ("null", None, None),
            ] {
                let inserted: NetworkValue = table.insert(NewNetworkValue { id, address, subnet }).await.unwrap();
                let expected = NetworkValue { id: id.into(), address: address.map(str::to_owned), subnet: subnet.map(str::to_owned) };
                assert_eq!(inserted, expected);
                let found = table.query().filter(network_schema::network_values::id.eq(id).unwrap()).first::<NetworkValue>().await.unwrap().unwrap();
                assert_eq!(found, expected);
                let rebound = pool.query(
                    "SELECT address IS NOT DISTINCT FROM $2::text::inet, subnet IS NOT DISTINCT FROM $3::text::cidr FROM network_values WHERE id = $1",
                    &[&id, &found.address, &found.subnet],
                ).await.unwrap();
                let [rebound] = rebound.as_slice() else { panic!("rebound network row must exist") };
                assert!(rebound.get::<_, bool>(0));
                assert!(rebound.get::<_, bool>(1));
            }
            drop(table);
            drop(db);
            pool.close().await;
        });
    });
}
