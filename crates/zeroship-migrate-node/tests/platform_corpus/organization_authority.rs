use super::fixture::Platform;
use compio_postgres::{Client, Error, error::SqlState};
use std::collections::BTreeMap;

const ORGANIZATION: &str = "org_0000000000000000000001";
const OTHER_ORGANIZATION: &str = "org_0000000000000000000002";
const PROJECT: &str = "prj_0000000000000000000001";
const OTHER_PROJECT: &str = "prj_0000000000000000000002";
const MEMBER: &str = "11111111-1111-1111-1111-111111111111";
const STRANGER: &str = "22222222-2222-2222-2222-222222222222";

#[test]
fn project_membership_requires_matching_organization_membership() {
    Platform::with_database(async |client| {
        seed_graph(client).await;
        let insert = "INSERT INTO zeroship.project_members
            (project_id, organization_id, user_id, role)
            VALUES ($1, $2, $3::text::uuid, $4)";

        refuses(
            client
                .execute(
                    insert,
                    &[&OTHER_PROJECT, &ORGANIZATION, &MEMBER, &"developer"],
                )
                .await,
            &SqlState::FOREIGN_KEY_VIOLATION,
            Some("project_members_project_ownership_fkey"),
        );
        refuses(
            client
                .execute(insert, &[&PROJECT, &ORGANIZATION, &STRANGER, &"developer"])
                .await,
            &SqlState::FOREIGN_KEY_VIOLATION,
            Some("project_members_organization_member_fkey"),
        );
        accepts(
            client
                .execute(insert, &[&PROJECT, &ORGANIZATION, &MEMBER, &"developer"])
                .await,
        );
        assert_eq!(
            client
                .query_one("SELECT count(*) FROM zeroship.project_members", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            1,
        );

        let member_insert = "INSERT INTO zeroship.organization_members
            (organization_id, user_id, role) VALUES ($1, $2::text::uuid, $3)";
        refuses(
            client
                .execute(member_insert, &[&ORGANIZATION, &STRANGER, &"superuser"])
                .await,
            &SqlState::FOREIGN_KEY_VIOLATION,
            Some("organization_members_role_fkey"),
        );
        accepts(
            client
                .execute(member_insert, &[&ORGANIZATION, &STRANGER, &"viewer"])
                .await,
        );
        accepts(
            client
                .execute(insert, &[&PROJECT, &ORGANIZATION, &STRANGER, &"viewer"])
                .await,
        );

        accepts(client.execute(
            "DELETE FROM zeroship.organization_members WHERE organization_id = $1 AND user_id = $2::text::uuid",
            &[&ORGANIZATION, &MEMBER],
        ).await);
        let remaining = client
            .query(
                "SELECT user_id::text FROM zeroship.project_members WHERE project_id = $1",
                &[&PROJECT],
            )
            .await
            .unwrap();
        assert_eq!(
            remaining
                .iter()
                .map(|row| row.get::<_, String>(0))
                .collect::<Vec<_>>(),
            [STRANGER]
        );
    });
}

#[test]
fn apps_keep_matching_ownership_until_deleted() {
    Platform::with_database(async |client| {
        seed_graph(client).await;
        client.execute(
            "INSERT INTO zeroship.plans (id, name, runtime_limits_json) VALUES ('free', 'Free', '{}')
             ON CONFLICT (id) DO NOTHING", &[],
        ).await.unwrap();
        let insert = "INSERT INTO zeroship.apps (id, name, project_id, organization_id)
            VALUES (gen_random_uuid(), 'authority-test', $1, $2)";
        refuses(
            client
                .execute(insert, &[&"prj_0000000000000000000009", &ORGANIZATION])
                .await,
            &SqlState::FOREIGN_KEY_VIOLATION,
            Some("apps_project_ownership_fkey"),
        );
        refuses(
            client
                .execute(insert, &[&OTHER_PROJECT, &ORGANIZATION])
                .await,
            &SqlState::FOREIGN_KEY_VIOLATION,
            Some("apps_project_ownership_fkey"),
        );
        refuses(
            client
                .execute(insert, &[&Option::<&str>::None, &ORGANIZATION])
                .await,
            &SqlState::CHECK_VIOLATION,
            Some("apps_live_app_has_project"),
        );
        accepts(client.execute(insert, &[&PROJECT, &ORGANIZATION]).await);

        let delete_project = "DELETE FROM zeroship.projects WHERE id = $1";
        let delete_organization = "DELETE FROM zeroship.organizations WHERE id = $1";
        refuses(
            client.execute(delete_project, &[&PROJECT]).await,
            &SqlState::FOREIGN_KEY_VIOLATION,
            Some("apps_project_ownership_fkey"),
        );
        refuses(
            client.execute(delete_organization, &[&ORGANIZATION]).await,
            &SqlState::FOREIGN_KEY_VIOLATION,
            Some("projects_organization_id_fkey"),
        );
        accepts(client.execute(delete_project, &[&OTHER_PROJECT]).await);
        accepts(
            client
                .execute(delete_organization, &[&OTHER_ORGANIZATION])
                .await,
        );

        // Deletion releases the project but preserves the app's organization.
        accepts(client.execute(
            "UPDATE zeroship.apps SET archived_at = now(), deleted_at = now(), project_id = NULL
             WHERE organization_id = $1",
            &[&ORGANIZATION],
        ).await);
        accepts(client.execute(delete_project, &[&PROJECT]).await);
        let app = client
            .query_one("SELECT organization_id, project_id FROM zeroship.apps", &[])
            .await
            .unwrap();
        assert_eq!(app.get::<_, String>(0), ORGANIZATION);
        assert_eq!(app.get::<_, Option<String>>(1), None);
    });
}

#[test]
fn invitations_obey_migrated_role_ranks_and_address_uniqueness() {
    Platform::with_database(async |client| {
        seed_graph(client).await;
        let roles = roles(client).await;
        let admin = roles["admin"];
        let developer = roles["developer"];
        let owner = roles["owner"];
        let billing = roles["billing"];
        assert!(admin.authority > developer.authority && admin.billing >= developer.billing);
        assert!(owner.authority > admin.authority);
        assert!(admin.authority > billing.authority && admin.billing < billing.billing);

        // The same invitation is retried with each invalid grant. Failed
        // inserts must leave its identity available to the accepted control.
        let id = "ivt_0000000000000000000001";
        let email = "invitee@authority.test";
        for (role, ranks, constraint, code) in [
            (
                "owner",
                owner,
                "organization_invites_no_escalation",
                &SqlState::CHECK_VIOLATION,
            ),
            (
                "billing",
                billing,
                "organization_invites_no_escalation",
                &SqlState::CHECK_VIOLATION,
            ),
            (
                "developer",
                RoleRanks {
                    authority: developer.authority - 1,
                    ..developer
                },
                "organization_invites_role_fkey",
                &SqlState::FOREIGN_KEY_VIOLATION,
            ),
        ] {
            refuses(
                invite(client, id, email, role, ranks, admin).await,
                code,
                Some(constraint),
            );
        }
        accepts(invite(client, id, email, "developer", developer, admin).await);
        let second = "ivt_0000000000000000000002";
        refuses(
            invite(
                client,
                second,
                &email.to_uppercase(),
                "developer",
                developer,
                admin,
            )
            .await,
            &SqlState::UNIQUE_VIOLATION,
            Some("organization_invites_one_active"),
        );
        accepts(
            invite(
                client,
                second,
                "another@authority.test",
                "developer",
                developer,
                admin,
            )
            .await,
        );
        let invitations = client
            .query(
                "SELECT email::text FROM zeroship.organization_invites ORDER BY email",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            invitations
                .iter()
                .map(|row| row.get::<_, String>(0))
                .collect::<Vec<_>>(),
            ["another@authority.test", email]
        );
    });
}

#[test]
fn control_can_manage_membership_but_cannot_redefine_roles() {
    Platform::with_database(async |client| {
        seed_graph(client).await;
        let before = roles(client).await;
        client
            .batch_execute("SET ROLE zeroship_control")
            .await
            .unwrap();
        let role = client
            .query_one("SELECT current_user::text", &[])
            .await
            .unwrap();
        assert_eq!(role.get::<_, String>(0), "zeroship_control");
        assert_eq!(
            roles(client).await,
            before,
            "control must be able to read the ladder"
        );
        for statement in [
            "INSERT INTO zeroship.organization_roles (role, rank, billing_rank, label) VALUES ('superowner', 99, 99, 'forged')",
            "UPDATE zeroship.organization_roles SET label = 'forged' WHERE role = 'viewer'",
            "DELETE FROM zeroship.organization_roles WHERE role = 'viewer'",
        ] {
            refuses(
                client.execute(statement, &[]).await,
                &SqlState::INSUFFICIENT_PRIVILEGE,
                None,
            );
        }
        assert_eq!(
            roles(client).await,
            before,
            "refused writes changed the ladder"
        );
        accepts(
            client
                .execute(
                    "INSERT INTO zeroship.organization_members (organization_id, user_id, role)
             VALUES ($1, $2::text::uuid, $3)",
                    &[&ORGANIZATION, &STRANGER, &"viewer"],
                )
                .await,
        );
        let membership = client.query_one(
            "SELECT role FROM zeroship.organization_members WHERE organization_id = $1 AND user_id = $2::text::uuid",
            &[&ORGANIZATION, &STRANGER],
        ).await.unwrap();
        assert_eq!(membership.get::<_, String>(0), "viewer");
    });
}

#[test]
fn organization_identifiers_and_case_insensitive_columns_keep_their_contract() {
    Platform::with_database(async |client| {
        let insert = "INSERT INTO zeroship.organizations (id, slug, name, billing_email)
            VALUES ($1, 'acme', 'Acme', 'billing@authority.test')";
        refuses(
            client.execute(insert, &[&"org_short"]).await,
            &SqlState::CHECK_VIOLATION,
            Some("organizations_id_shape"),
        );
        accepts(client.execute(insert, &[&ORGANIZATION]).await);

        let rows = client.query(
            "SELECT c.relname || '.' || a.attname AS column_name,
                    format_type(a.atttypid, a.atttypmod) AS column_type, co.collname
             FROM pg_attribute a
             JOIN pg_class c ON c.oid = a.attrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             JOIN pg_collation co ON co.oid = a.attcollation
             WHERE n.nspname = 'zeroship' AND a.attnum > 0 AND NOT a.attisdropped
               AND ((c.relname IN ('organizations', 'projects', 'organization_members',
                                   'project_members', 'organization_invites')
                     AND a.attname IN ('id', 'organization_id', 'project_id', 'slug', 'billing_email', 'email'))
                 OR (c.relname = 'apps' AND a.attname IN ('project_id', 'organization_id')))",
            &[],
        ).await.unwrap();
        let observed: BTreeMap<String, (String, String)> = rows
            .iter()
            .map(|row| (row.get(0), (row.get(1), row.get(2))))
            .collect();
        assert_eq!(
            observed.len(),
            rows.len(),
            "catalog returned duplicate columns"
        );
        let expected: BTreeMap<_, _> = [
            ("apps.project_id", "text", "C"),
            ("apps.organization_id", "text", "C"),
            ("organization_invites.email", "citext", "default"),
            ("organization_invites.id", "text", "C"),
            ("organization_invites.organization_id", "text", "C"),
            ("organization_members.organization_id", "text", "C"),
            ("organizations.billing_email", "citext", "default"),
            ("organizations.id", "text", "C"),
            ("organizations.slug", "citext", "default"),
            ("project_members.organization_id", "text", "C"),
            ("project_members.project_id", "text", "C"),
            ("projects.id", "text", "C"),
            ("projects.organization_id", "text", "C"),
            ("projects.slug", "citext", "default"),
        ]
        .into_iter()
        .map(|(column, ty, collation)| (column.to_owned(), (ty.to_owned(), collation.to_owned())))
        .collect();
        assert_eq!(observed, expected);
    });
}

async fn seed_graph(client: &Client) {
    assert_eq!(
        client
            .execute(
                "INSERT INTO zeroship.users (id, email, name) VALUES
         ($1::text::uuid, 'member@authority.test', 'Member'),
         ($2::text::uuid, 'stranger@authority.test', 'Stranger')",
                &[&MEMBER, &STRANGER],
            )
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        client
            .execute(
                "INSERT INTO zeroship.organizations (id, slug, name, billing_email) VALUES
         ($1, 'acme', 'Acme', 'billing@acme.test'), ($2, 'other', 'Other', 'billing@other.test')",
                &[&ORGANIZATION, &OTHER_ORGANIZATION],
            )
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        client
            .execute(
                "INSERT INTO zeroship.projects (id, organization_id, slug, name) VALUES
         ($1, $2, 'web', 'Web'), ($3, $4, 'web', 'Web')",
                &[&PROJECT, &ORGANIZATION, &OTHER_PROJECT, &OTHER_ORGANIZATION],
            )
            .await
            .unwrap(),
        2
    );
    accepts(
        client
            .execute(
                "INSERT INTO zeroship.organization_members (organization_id, user_id, role)
         VALUES ($1, $2::text::uuid, 'admin')",
                &[&ORGANIZATION, &MEMBER],
            )
            .await,
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RoleRanks {
    authority: i32,
    billing: i32,
}

async fn roles(client: &Client) -> BTreeMap<String, RoleRanks> {
    let rows = client
        .query(
            "SELECT role, rank, billing_rank FROM zeroship.organization_roles",
            &[],
        )
        .await
        .unwrap();
    assert!(
        !rows.is_empty(),
        "the migrated role ladder must be populated"
    );
    rows.iter()
        .map(|row| {
            (
                row.get(0),
                RoleRanks {
                    authority: row.get(1),
                    billing: row.get(2),
                },
            )
        })
        .collect()
}

async fn invite(
    client: &Client,
    id: &str,
    email: &str,
    role: &str,
    ranks: RoleRanks,
    issuer: RoleRanks,
) -> Result<u64, Error> {
    client
        .execute(
            "INSERT INTO zeroship.organization_invites
         (id, token_hash, organization_id, email, role, role_rank, role_billing_rank,
          invited_by, invited_by_rank, invited_by_billing_rank, purpose, expires_at)
         VALUES ($1, decode($1, 'escape'), $2, $3, $4, $5, $6,
                 $7::text::uuid, $8, $9, 'organization_invite', now() + interval '1 day')",
            &[
                &id,
                &ORGANIZATION,
                &email,
                &role,
                &ranks.authority,
                &ranks.billing,
                &MEMBER,
                &issuer.authority,
                &issuer.billing,
            ],
        )
        .await
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
