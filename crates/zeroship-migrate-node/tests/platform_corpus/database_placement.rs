use super::fixture::Platform;
use compio_postgres::{Client, Error, error::SqlState};
use zeroship_id::{AppId, BindingId, DatabaseId, DatastoreId, OrganizationId, ProjectId};

// The zone seeded by 20260914000450_execution_zones_default_zone.ts, and a
// second one this fixture declares so a cross-zone placement is expressible at
// all. A refusal proves nothing when the forbidden row could not be spelled.
const HOME_ZONE: &str = "ezn_default000000000000000000";
const FAR_ZONE: &str = "ezn_secondzone000000000000000";

const INSERT_BINDING: &str = "INSERT INTO zeroship.database_bindings
    (id, app_id, database_id, project_id, capability)
    VALUES ($1, $2, $3, $4, 'readwrite')";
const INSERT_DATABASE: &str = "INSERT INTO zeroship.databases
    (id, project_id, execution_zone_id, datastore_id, name)
    VALUES ($1, $2, $3, $4, $5)";

#[test]
fn a_binding_joins_an_app_and_a_database_of_one_project() {
    Platform::with_database(async |client| {
        let graph = seed(client).await;

        // The database side: an app of this project reaching a database of
        // another one, named under its own project.
        refuses(
            client
                .execute(
                    INSERT_BINDING,
                    &[
                        &BindingId::mint().as_str(),
                        &graph.app.as_str(),
                        &graph.other_database.as_str(),
                        &graph.project.as_str(),
                    ],
                )
                .await,
            &SqlState::FOREIGN_KEY_VIOLATION,
            Some("database_bindings_database_project_fkey"),
        );
        // The app side: the same pair named under the database's project.
        // Whichever project the binding claims, one of the two edges refuses.
        refuses(
            client
                .execute(
                    INSERT_BINDING,
                    &[
                        &BindingId::mint().as_str(),
                        &graph.app.as_str(),
                        &graph.other_database.as_str(),
                        &graph.other_project.as_str(),
                    ],
                )
                .await,
            &SqlState::FOREIGN_KEY_VIOLATION,
            Some("database_bindings_app_project_fkey"),
        );

        // The control: the same app, the same statement, a database of its own
        // project. Without it a fixture that inserted no app or no database
        // would produce the refusals above for the wrong reason.
        accepts(
            client
                .execute(
                    INSERT_BINDING,
                    &[
                        &BindingId::mint().as_str(),
                        &graph.app.as_str(),
                        &graph.database.as_str(),
                        &graph.project.as_str(),
                    ],
                )
                .await,
        );
        let bound = client
            .query_one(
                "SELECT app_id, database_id, project_id FROM zeroship.database_bindings",
                &[],
            )
            .await
            .expect("exactly one binding survived");
        assert_eq!(bound.get::<_, String>(0), graph.app.as_str());
        assert_eq!(bound.get::<_, String>(1), graph.database.as_str());
        assert_eq!(bound.get::<_, String>(2), graph.project.as_str());
    });
}

#[test]
fn a_database_sits_on_a_datastore_of_its_projects_zone() {
    Platform::with_database(async |client| {
        let graph = seed(client).await;

        // The placement edge: a project of the home zone, a cluster of the far
        // one. The project's own zone is consistent, so only the cluster's
        // disagrees.
        refuses(
            client
                .execute(
                    INSERT_DATABASE,
                    &[
                        &DatabaseId::mint().as_str(),
                        &graph.project.as_str(),
                        &HOME_ZONE,
                        &graph.far_datastore.as_str(),
                        &"cross-zone-cluster",
                    ],
                )
                .await,
            &SqlState::FOREIGN_KEY_VIOLATION,
            Some("databases_placement_fkey"),
        );
        // Claiming the cluster's zone instead does not buy the row a placement:
        // it now disagrees with the project, which is the other half of the
        // pair, so there is no zone a cross-zone database can name.
        refuses(
            client
                .execute(
                    INSERT_DATABASE,
                    &[
                        &DatabaseId::mint().as_str(),
                        &graph.project.as_str(),
                        &FAR_ZONE,
                        &graph.far_datastore.as_str(),
                        &"cross-zone-project",
                    ],
                )
                .await,
            &SqlState::FOREIGN_KEY_VIOLATION,
            Some("databases_project_zone_fkey"),
        );

        // The controls: the same statement placing each project on the cluster
        // of its own zone. Both zones therefore carry a database, so the
        // refusals above are about the mismatch and not about an empty side.
        let home = DatabaseId::mint();
        accepts(
            client
                .execute(
                    INSERT_DATABASE,
                    &[
                        &home.as_str(),
                        &graph.project.as_str(),
                        &HOME_ZONE,
                        &graph.datastore.as_str(),
                        &"home-zone",
                    ],
                )
                .await,
        );
        let far = DatabaseId::mint();
        accepts(
            client
                .execute(
                    INSERT_DATABASE,
                    &[
                        &far.as_str(),
                        &graph.far_project.as_str(),
                        &FAR_ZONE,
                        &graph.far_datastore.as_str(),
                        &"far-zone",
                    ],
                )
                .await,
        );
        let placed = client
            .query(
                "SELECT d.id, d.datastore_id FROM zeroship.databases d
                 WHERE d.id = ANY($1) ORDER BY d.name",
                &[&vec![home.as_str().to_owned(), far.as_str().to_owned()]],
            )
            .await
            .expect("read back the accepted placements");
        assert_eq!(
            placed
                .iter()
                .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
                .collect::<Vec<_>>(),
            [
                (
                    far.as_str().to_owned(),
                    graph.far_datastore.as_str().to_owned()
                ),
                (
                    home.as_str().to_owned(),
                    graph.datastore.as_str().to_owned()
                ),
            ]
        );
    });
}

struct PlacementGraph {
    project: ProjectId,
    other_project: ProjectId,
    far_project: ProjectId,
    app: AppId,
    datastore: DatastoreId,
    far_datastore: DatastoreId,
    database: DatabaseId,
    other_database: DatabaseId,
}

async fn seed(client: &Client) -> PlacementGraph {
    let organization = OrganizationId::mint();
    let graph = PlacementGraph {
        project: ProjectId::mint(),
        other_project: ProjectId::mint(),
        far_project: ProjectId::mint(),
        app: AppId::mint(),
        datastore: DatastoreId::mint(),
        far_datastore: DatastoreId::mint(),
        database: DatabaseId::mint(),
        other_database: DatabaseId::mint(),
    };

    assert_eq!(
        client
            .execute(
                "INSERT INTO zeroship.execution_zones (id, name, status)
                 VALUES ($1, 'placement-far', 'active')",
                &[&FAR_ZONE],
            )
            .await
            .expect("declare the second execution zone"),
        1
    );
    assert_eq!(
        client
            .execute(
                "INSERT INTO zeroship.organizations (id, slug, name, billing_email)
                 VALUES ($1, 'placement', 'Placement', 'billing@placement.test')",
                &[&organization.as_str()],
            )
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        client
            .execute(
                "INSERT INTO zeroship.projects (id, organization_id, slug, name, execution_zone_id)
                 VALUES ($1, $4, 'home', 'Home', $5),
                        ($2, $4, 'neighbour', 'Neighbour', $5),
                        ($3, $4, 'far', 'Far', $6)",
                &[
                    &graph.project.as_str(),
                    &graph.other_project.as_str(),
                    &graph.far_project.as_str(),
                    &organization.as_str(),
                    &HOME_ZONE,
                    &FAR_ZONE,
                ],
            )
            .await
            .unwrap(),
        3
    );
    client
        .execute(
            "INSERT INTO zeroship.plans (id, name, runtime_limits_json)
             VALUES ('free', 'Free', '{}') ON CONFLICT (id) DO NOTHING",
            &[],
        )
        .await
        .unwrap();
    accepts(
        client
            .execute(
                "INSERT INTO zeroship.apps (id, name, project_id, organization_id, execution_zone_id)
                 VALUES ($1, 'placement-app', $2, $3, $4)",
                &[
                    &graph.app.as_str(),
                    &graph.project.as_str(),
                    &organization.as_str(),
                    &HOME_ZONE,
                ],
            )
            .await,
    );
    assert_eq!(
        client
            .execute(
                "INSERT INTO zeroship.datastores (id, system_identifier, execution_zone_id)
                 VALUES ($1, $3, $5), ($2, $4, $6)",
                &[
                    &graph.datastore.as_str(),
                    &graph.far_datastore.as_str(),
                    &7_403_001_i64,
                    &7_403_002_i64,
                    &HOME_ZONE,
                    &FAR_ZONE,
                ],
            )
            .await
            .unwrap(),
        2
    );
    accepts(
        client
            .execute(
                INSERT_DATABASE,
                &[
                    &graph.database.as_str(),
                    &graph.project.as_str(),
                    &HOME_ZONE,
                    &graph.datastore.as_str(),
                    &"seeded",
                ],
            )
            .await,
    );
    accepts(
        client
            .execute(
                INSERT_DATABASE,
                &[
                    &graph.other_database.as_str(),
                    &graph.other_project.as_str(),
                    &HOME_ZONE,
                    &graph.datastore.as_str(),
                    &"seeded",
                ],
            )
            .await,
    );
    graph
}

#[track_caller]
fn accepts(result: Result<u64, Error>) {
    assert_eq!(result.expect("the accepted control must succeed"), 1);
}

#[track_caller]
fn refuses(result: Result<u64, Error>, code: &SqlState, constraint: Option<&str>) {
    let error = result.expect_err("the database accepted a forbidden write");
    let diagnostic = error
        .as_db_error()
        .expect("refusal must come from PostgreSQL");
    assert_eq!(diagnostic.code(), code, "{error:?}");
    assert_eq!(diagnostic.constraint(), constraint, "{error:?}");
}
