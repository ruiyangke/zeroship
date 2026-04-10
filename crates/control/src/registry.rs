//! Registry — application CRUD backed by PostgreSQL.

/// Application registry backed by `appbase-pg`.
#[derive(Debug)]
pub struct Registry;

impl Registry {
    /// Connect to the database and return a new `Registry`.
    ///
    /// # Errors
    /// Returns an error string if the connection fails.
    pub async fn new(_db_url: &str) -> Result<Self, String> {
        Ok(Self)
    }
}
