//! The row count carried by a `CommandComplete` tag.
//!
//! This is a LEAF on purpose. The function below is shared by three modules -
//! `query`, `simple_query`, and the query observer in `client` - and it used to
//! live in `query.rs`. That made `simple_query` depend on `query` for a single
//! pure function, which was the whole of one direction of the
//! `query <-> simple_query` dependency cycle.
//!
//! Nothing here may take a dependency on the modules that call it.

use crate::Error;
use postgres_protocol::message::backend::CommandCompleteBody;

/// Extract the number of rows affected from [`CommandCompleteBody`].
///
/// PostgreSQL's command tag is `INSERT <oid> <count>`, `UPDATE <count>`,
/// `SELECT <count>`, or a bare verb such as `CREATE TABLE` that carries no
/// count at all. Taking the LAST whitespace-separated token covers every
/// counted shape, and a tag whose final token does not parse - the bare-verb
/// case - answers 0 rather than failing: reaching a countless tag is not an
/// error, it is the correct answer for DDL.
pub fn extract_row_affected(body: &CommandCompleteBody) -> Result<u64, Error> {
    let rows = body
        .tag()
        .map_err(Error::parse)?
        .rsplit(' ')
        .next()
        .expect("str::rsplit yields one field even for an empty command tag")
        .parse()
        .unwrap_or(0);
    Ok(rows)
}
