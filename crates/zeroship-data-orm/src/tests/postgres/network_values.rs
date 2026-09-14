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
