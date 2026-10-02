//! A fresh, scoped set of platform ids for one test case.
//!
//! Cases share one migrated database, so every case owns the rows it creates.
//! [`Tenant::mint`] mints the four ids a case roots its data at - an
//! organization, a project, an app and a user - from the typed-id vocabulary, so
//! no case can collide with another's rows and no fixed id is shared between
//! cases.

use zeroship_id::{AppId, OrganizationId, ProjectId, UserId};

/// The four typed ids one case scopes its rows to.
#[derive(Debug)]
pub struct Tenant {
    pub organization: OrganizationId,
    pub project: ProjectId,
    pub app: AppId,
    pub user: UserId,
}

impl Tenant {
    /// Mint a set of ids no other case carries.
    #[must_use]
    pub fn mint() -> Self {
        Self {
            organization: OrganizationId::mint(),
            project: ProjectId::mint(),
            app: AppId::mint(),
            user: UserId::mint(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Tenant;

    #[test]
    fn minted_ids_carry_their_entity_prefix() {
        let tenant = Tenant::mint();
        assert!(tenant.organization.as_str().starts_with("org_"));
        assert!(tenant.project.as_str().starts_with("prj_"));
        assert!(tenant.app.as_str().starts_with("app_"));
        assert!(tenant.user.as_str().starts_with("usr_"));
    }

    #[test]
    fn each_case_mints_distinct_ids() {
        let first = Tenant::mint();
        let second = Tenant::mint();
        assert_ne!(first.organization.as_str(), second.organization.as_str());
        assert_ne!(first.project.as_str(), second.project.as_str());
        assert_ne!(first.app.as_str(), second.app.as_str());
        assert_ne!(first.user.as_str(), second.user.as_str());
    }
}
