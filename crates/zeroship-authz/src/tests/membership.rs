//! Seed a principal and its resource ownership chain; project seats are explicit.

use crate::{Action, AuthzContext, Policy, Resource};
use compio_postgres::Client;
use zeroship_id::{AppId, UserId};

#[allow(
    clippy::struct_field_names,
    reason = "the fixture describes related entity ids"
)]
#[derive(Debug)]
pub struct Membership {
    pub user_id: UserId,
    pub organization_id: String,
    pub project_id: String,
    pub app_id: AppId,
}

impl Membership {
    pub async fn new(pg: &Client, label: &str, organization_role: &str) -> Self {
        let user_id = UserId::mint();
        let organization_id = zeroship_id::typed_id::generate("org");
        let project_id = zeroship_id::typed_id::generate("prj");
        let app_id = AppId::mint();

        pg.execute(
            "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
            &[
                &user_id.as_str(),
                &format!("{label}-{}@example.com", user_id.as_str()),
                &label,
            ],
        )
        .await
        .expect("insert user");

        pg.execute(
            "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
             VALUES ($1, $2::citext, $3, $4::citext)",
            &[
                &organization_id,
                &organization_id.replace('_', "-"),
                &label,
                &format!("{label}@example.com"),
            ],
        )
        .await
        .expect("insert organization");

        pg.execute(
            "INSERT INTO zeroship.projects (id, organization_id, slug, name) \
             VALUES ($1, $2, $3::citext, $4)",
            &[&project_id, &organization_id, &"default", &label],
        )
        .await
        .expect("insert project");

        pg.execute(
            "INSERT INTO zeroship.apps (id, name, project_id, organization_id) \
             SELECT $1, $2, p.id, p.organization_id FROM zeroship.projects p WHERE p.id = $3",
            &[
                &app_id.as_str(),
                &format!("authz-{label}-{}", app_id.as_str()),
                &project_id,
            ],
        )
        .await
        .expect("insert app");

        pg.execute(
            "INSERT INTO zeroship.organization_members (organization_id, user_id, role) \
             VALUES ($1, $2, $3)",
            &[&organization_id, &user_id.as_str(), &organization_role],
        )
        .await
        .expect("insert organization membership");

        Self {
            user_id,
            organization_id,
            project_id,
            app_id,
        }
    }

    pub async fn seated(pg: &Client, label: &str, role: &str) -> Self {
        let fixture = Self::new(pg, label, role).await;
        fixture.seat_on_project(pg, role).await;
        fixture
    }

    pub async fn seat_on_project(&self, pg: &Client, role: &str) {
        pg.execute(
            "INSERT INTO zeroship.project_members \
                (project_id, organization_id, user_id, role) \
             VALUES ($1, $2, $3, $4)",
            &[
                &self.project_id,
                &self.organization_id,
                &self.user_id.as_str(),
                &role,
            ],
        )
        .await
        .expect("insert project membership");
    }

    pub fn app(&self) -> Resource {
        Resource::App {
            id: self.app_id.clone(),
        }
    }

    pub fn project(&self) -> Resource {
        Resource::Project {
            id: self.project_id.clone(),
        }
    }

    pub fn organization(&self) -> Resource {
        Resource::Organization {
            id: self.organization_id.clone(),
        }
    }

    pub fn ctx(&self, action: Action, resource: Resource) -> AuthzContext<'_> {
        AuthzContext {
            principal_id: self.user_id.clone(),
            token_policy: None,
            action,
            resource,
            now: 12 * 60 * 60,
            request_ip: None,
            request_id: None,
        }
    }

    pub fn ctx_with_policy(
        &self,
        action: Action,
        resource: Resource,
        now: i64,
        policy: Policy,
    ) -> AuthzContext<'_> {
        AuthzContext {
            principal_id: self.user_id.clone(),
            token_policy: Some(policy),
            action,
            resource,
            now,
            request_ip: None,
            request_id: None,
        }
    }
}
