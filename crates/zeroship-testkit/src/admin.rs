//! Database-level administration: does a database exist, and create one.
//!
//! THIS IS WHERE `psql` DIES. The shell library reached the server by shelling
//! out to `psql -tAc` and reading stdout back as a string, which made a
//! connection failure and an empty result set the same two bytes of nothing --
//! the reason `zs_suite_db_exists` had to be three-valued and hand-annotated.
//! Here the driver distinguishes them for us: [`DbAdmin::exists`] returns
//! `Ok(false)` for absent and `Err` for could-not-tell, and there is no third
//! way to spell either.
//!
//! CREATE DATABASE AND DROP DATABASE CANNOT RUN INSIDE A TRANSACTION. Both go
//! through the SIMPLE query protocol ([`compio_postgres::Client::simple_query`])
//! for that reason: the extended protocol's parse/bind/execute round is what
//! `PREPARE`-style statements use, and PostgreSQL rejects these two there.
//! Nothing in this module opens a transaction.
//!
//! THE TRAIT EXISTS SO THE DECISIONS CAN BE TESTED WITHOUT A SERVER. The
//! interesting arms of `zs_suite_db_ensure` are the ones no test can schedule
//! against a real cluster -- losing a create race to a peer between the probe
//! and the CREATE. `suite_db`'s unit tests script a [`DbAdmin`] to produce
//! exactly that interleaving; the live behaviour is covered separately by
//! `tests/live_suite_db.rs`.

use compio_postgres::{Config, NoTls};

/// What a suite database provisioner needs from a server, and nothing else.
pub trait DbAdmin {
    /// `Ok(true)` present, `Ok(false)` absent, `Err(reason)` could not tell.
    ///
    /// Reading "could not tell" as "absent" turns an unreachable server into a
    /// `CREATE DATABASE` that fails for reasons nobody can name, which is the
    /// bug the shell version carried a paragraph of comment to avoid.
    fn exists(&mut self, name: &str) -> Result<bool, String>;

    /// `Err` carries the SERVER's own message, verbatim.
    ///
    /// The caller re-emits it: on the losing side of a create race it is the
    /// only evidence of what happened, and swallowing it is how a permission
    /// problem gets reported as a race.
    fn create(&mut self, name: &str) -> Result<(), String>;
}

/// Where to reach the server, as the harness spells it.
#[derive(Debug, Clone)]
pub struct Server {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
}

impl Server {
    /// Take the coordinates from the overlay the caller already loaded.
    ///
    /// NOT FROM `PG_HOST`/`PG_PORT`/`PG_USER`/`PG_PASS`, even though those are
    /// exported and would work. Two reasons, and the second is the one that
    /// decides it:
    ///
    /// 1. Reading the process environment to choose a server is ambient
    ///    configuration: two callers with identical arguments get different
    ///    databases and the difference is invisible in the call.
    /// 2. It would be READING BACK a value this same crate had just written.
    ///    `overlay::assert_agrees` already refuses when the caller's `PG_*`
    ///    disagree with the overlay, so after a successful load the two are
    ///    equal BY CONSTRUCTION -- and a second source that can only ever agree
    ///    is not a second source, it is a place for them to drift.
    ///
    /// The database is `postgres` rather than the overlay's, because you cannot
    /// create a database from inside it; see `PgAdmin::simple`.
    pub fn from_overlay(loaded: &crate::overlay::Loaded) -> Result<Server, String> {
        let port: u16 = loaded.port.parse().map_err(|_| {
            format!(
                "FATAL: {} names port '{}', which is not a port number.\n",
                loaded.overlay.display(),
                loaded.port
            )
        })?;
        Ok(Server {
            host: loaded.host.clone(),
            port,
            user: loaded.user.clone(),
            pass: loaded.pass.clone(),
        })
    }

    fn config(&self, dbname: &str) -> Config {
        let mut config = Config::new();
        config.host(&self.host).port(self.port).dbname(dbname);
        if !self.user.is_empty() {
            config.user(&self.user);
        }
        if !self.pass.is_empty() {
            config.password(&self.pass);
        }
        config
    }
}

/// A [`DbAdmin`] that talks to a real PostgreSQL over compio-postgres.
///
/// One connection per call rather than a held client: these two statements are
/// issued at most three times per suite run, minutes apart, and a connection
/// held open across the `flock` in `suite_db::provision` would sit idle for the
/// whole of a peer's migration.
#[derive(Debug)]
pub struct PgAdmin {
    server: Server,
}

impl PgAdmin {
    pub fn new(server: Server) -> PgAdmin {
        PgAdmin { server }
    }

    /// Run one statement against the maintenance database and return its rows.
    ///
    /// `postgres` is the maintenance database on every cluster this harness
    /// meets; `template1` would also work and is what `createdb` falls back to,
    /// but connecting to a template while another session is copying it is a
    /// documented way to make `CREATE DATABASE` fail with "source database is
    /// being accessed by other users".
    fn simple(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>, String> {
        let config = self.server.config("postgres");
        compio::runtime::Runtime::new()
            .map_err(|e| format!("could not start the io_uring runtime: {e}"))?
            .block_on(async move {
                let (client, connection) = config
                    .connect(NoTls)
                    .await
                    .map_err(|e| format!("connect: {e}"))?;
                let driver = compio::runtime::spawn(async move {
                    let _ = connection.run().await;
                });
                let result = client.simple_query(sql).await;
                drop(client);
                driver.detach();
                let messages = result.map_err(|e| format!("{e}"))?;
                Ok(messages
                    .into_iter()
                    .filter_map(|message| match message {
                        compio_postgres::SimpleQueryMessage::Row(row) => Some(
                            (0..row.len())
                                .map(|i| row.get(i).map(|s| s.to_string()))
                                .collect(),
                        ),
                        _ => None,
                    })
                    .collect())
            })
    }
}

impl DbAdmin for PgAdmin {
    fn exists(&mut self, name: &str) -> Result<bool, String> {
        // The name is quoted as a LITERAL here, not interpolated as an
        // identifier: `pg_database.datname` is a value, and a name that reached
        // this point without passing `suite_db::check_identifier` still cannot
        // close the string.
        let rows = self.simple(&format!(
            "SELECT 1 FROM pg_database WHERE datname = {}",
            quote_literal(name)
        ))?;
        Ok(!rows.is_empty())
    }

    fn create(&mut self, name: &str) -> Result<(), String> {
        // A database name reaches CREATE DATABASE as an IDENTIFIER, which no
        // protocol-level parameter can carry. `check_identifier` is the gate;
        // it runs again here because this function is public and the gate being
        // somewhere else is not the same as it having run.
        crate::suite_db::check_identifier(name).map_err(|refusal| refusal.trim_end().to_string())?;
        self.simple(&format!("CREATE DATABASE {name}")).map(|_| ())
    }
}

/// Double every `'`, the only escape a standard-conforming string literal needs.
fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quote_cannot_close_the_literal() {
        assert_eq!(quote_literal("o'brien"), "'o''brien'");
        assert_eq!(quote_literal("a'; DROP DATABASE x --"), "'a''; DROP DATABASE x --'");
    }
}
