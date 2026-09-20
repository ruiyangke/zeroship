//! [`DatabaseId`] names one creator database: a schema inside a datastore,
//! owned by a project.
//!
//! This is the identity an app id used to carry by accident. A `DatabaseId`
//! names the physical schema, the migrator and capability roles, the apply lock
//! and the encryption salt, which is what makes a database that outlives its
//! app - or one two apps share - representable at all.
//!
//! It is creator-visible and addressed by id everywhere. There is no
//! `(project, name) -> DatabaseId` resolution on any wire: `databases.name` is
//! display text the CLI dereferences locally, and an id is not a capability,
//! because authorization is evaluated on the database itself.

use crate::entity_id::declare_entity_id;
use crate::typed_id::DATABASE_PREFIX;

declare_entity_id! {
    /// The typed id of one creator database: `dbs_<base36(uuidv7)>`.
    DatabaseId,
    DATABASE_PREFIX,
    database_id_tests,
}
