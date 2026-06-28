use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeroship_runtime::channel::{result_slot, ResultReceiver};
use zeroship_runtime::{EnvSnapshot, HostPort, NetPolicy, Runtime};

use crate::frontend::embedding::js_driver_module_graph;
use crate::render::step::BindValue;

const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const TRUSTED_DRIVER_MAX_SOCKETS: u32 = 4;
const TRUSTED_DRIVER_EGRESS_CEILING: u64 = 64 * 1024 * 1024;
const MYSQL_TLS_PIN_REQUIRED_MESSAGE: &str =
    "mysql TLS requires a non-empty pinned ssl.ca when ssl is enabled; refusing public roots with disabled hostname verification";

const MYSQL_DRIVER_ENTRY_PREFIX: &str = r#"
import mysql from "mysql2/promise";

function remoteError(err) {
  return {
    code: Number(err?.errno ?? 0),
    sqlstate: err?.sqlState ?? err?.sqlstate ?? "",
    message: err?.message ?? String(err),
  };
}

async function main() {
  let conn = null;
  let startupErr = null;
  try {
    conn = await mysql.createConnection(__zsDriverDsn());
  } catch (err) {
    startupErr = err;
  }
  for (;;) {
    const cmd = await __zsNextCommand();
    try {
      if (startupErr) {
        throw startupErr;
      }
      if (cmd.kind === "exec") {
        await conn.execute(cmd.sql, cmd.binds ?? []);
        __zsResolve(cmd.id, { ok: [] });
      } else if (cmd.kind === "query_json") {
        const [rows] = await conn.execute(cmd.sql, cmd.binds ?? []);
        __zsResolve(cmd.id, { ok: rows });
"#;

#[cfg(test)]
const MYSQL_DRIVER_TEST_CLOSE_BRANCH: &str = r#"
      } else if (cmd.kind === "__zs_close_for_test") {
        const stream = conn?.connection?.stream;
        if (stream && typeof stream.destroy === "function") {
          stream.destroy();
        } else if (conn?.connection && typeof conn.connection.destroy === "function") {
          conn.connection.destroy();
        } else {
          await conn.end();
        }
        __zsResolve(cmd.id, { ok: [] });
        break;
"#;

const MYSQL_DRIVER_ENTRY_SUFFIX: &str = r#"
      } else {
        throw new Error(`unknown JS driver command kind: ${cmd.kind}`);
      }
    } catch (err) {
      __zsResolve(cmd.id, { err: remoteError(err) });
    }
  }
}

void main().catch((err) => {
  console.error("fatal JS driver loop error:", err?.stack ?? err);
});

export default {};
"#;

#[cfg(test)]
fn mysql_driver_entry() -> String {
    format!(
        "{MYSQL_DRIVER_ENTRY_PREFIX}{MYSQL_DRIVER_TEST_CLOSE_BRANCH}{MYSQL_DRIVER_ENTRY_SUFFIX}"
    )
}

#[cfg(not(test))]
fn mysql_driver_entry() -> String {
    format!("{MYSQL_DRIVER_ENTRY_PREFIX}{MYSQL_DRIVER_ENTRY_SUFFIX}")
}

const ECHO_DRIVER_ENTRY: &str = r#"
const session = {
  dsn: __zsDriverDsn(),
  commands: [],
};

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function okRows(cmd) {
  return [{
    command_count: session.commands.length,
    last_sql: cmd.sql,
    last_kind: cmd.kind,
    bind_count: Array.isArray(cmd.binds) ? cmd.binds.length : 0,
    driver: session.dsn?.driver ?? "echo",
  }];
}

async function main() {
  for (;;) {
    const cmd = await __zsNextCommand();
    try {
      if (cmd.sql === "__zs_echo_delay_after_timeout__") {
        await delay(25);
      }
      session.commands.push({ kind: cmd.kind, sql: cmd.sql, binds: cmd.binds ?? [] });
      if (cmd.kind === "exec") {
        __zsResolve(cmd.id, { ok: [] });
      } else if (cmd.kind === "query_json") {
        __zsResolve(cmd.id, { ok: okRows(cmd) });
      } else {
        throw new Error(`unknown echo command kind: ${cmd.kind}`);
      }
    } catch (err) {
      __zsResolve(cmd.id, {
        err: {
          code: 0,
          sqlstate: "",
          message: err?.message ?? String(err),
        },
      });
    }
  }
}

void main().catch((err) => {
  __zsResolve(0, {
    err: {
      code: 0,
      sqlstate: "",
      message: err?.message ?? String(err),
    },
  });
});

export default {};
"#;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RowSet {
    pub rows: Vec<Map<String, Value>>,
}

#[derive(Debug, thiserror::Error)]
pub enum JsDriverError {
    #[error(
        "timed out waiting for JS driver command {command_id} after {timeout:?}; connection poisoned and driver runtime torn down"
    )]
    TimedOut {
        command_id: u64,
        timeout: Duration,
    },
    #[error("JS driver connection is poisoned: {message}")]
    Poisoned { message: String },
    #[error("JS driver transport error: {message}")]
    Transport { message: String },
    #[error("JS driver remote error {code} SQLSTATE {sqlstate}: {message}")]
    Remote {
        code: u16,
        sqlstate: String,
        message: String,
    },
    #[error("JS driver marshal error: {message}")]
    Marshal { message: String },
}

impl JsDriverError {
    pub(crate) fn transport(message: impl Into<String>) -> Self {
        Self::Transport {
            message: message.into(),
        }
    }

    pub(crate) fn marshal(message: impl Into<String>) -> Self {
        Self::Marshal {
            message: message.into(),
        }
    }

    pub(crate) fn poisoned(message: impl Into<String>) -> Self {
        Self::Poisoned {
            message: message.into(),
        }
    }
}

#[derive(Debug, Serialize)]
struct DriverCommand<'a> {
    id: u64,
    kind: &'a str,
    sql: &'a str,
    binds: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct DriverReply {
    ok: Option<Value>,
    err: Option<RemoteReply>,
}

#[derive(Debug, Deserialize)]
struct RemoteReply {
    #[serde(default)]
    code: u16,
    #[serde(default, alias = "sqlState")]
    sqlstate: String,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct MysqlDsnNetTarget {
    host: String,
    #[serde(default = "default_mysql_port")]
    port: u16,
}

const fn default_mysql_port() -> u16 {
    3306
}

pub struct JsDriverConn {
    runtime: Option<Runtime>,
    next_id: u64,
    in_flight: bool,
    command_timeout: Duration,
    poisoned: Option<String>,
}

impl fmt::Debug for JsDriverConn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsDriverConn")
            .field("next_id", &self.next_id)
            .field("in_flight", &self.in_flight)
            .field("command_timeout", &self.command_timeout)
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

impl JsDriverConn {
    pub fn open_echo() -> Result<Self, JsDriverError> {
        Self::open_echo_with_timeout(DEFAULT_COMMAND_TIMEOUT)
    }

    pub fn open_echo_with_timeout(command_timeout: Duration) -> Result<Self, JsDriverError> {
        Self::open_with_entry(
            serde_json::json!({ "driver": "echo" }).to_string(),
            ECHO_DRIVER_ENTRY,
            NetPolicy::trusted(TRUSTED_DRIVER_MAX_SOCKETS, TRUSTED_DRIVER_EGRESS_CEILING),
            command_timeout,
        )
    }

    pub fn open_mysql_dsn_json(
        dsn_json: String,
        command_timeout: Duration,
    ) -> Result<Self, JsDriverError> {
        let policy = mysql_dsn_net_policy(&dsn_json)?;
        Self::open_mysql_dsn_json_with_policy(dsn_json, policy, command_timeout)
    }

    pub fn open_mysql_dsn_json_allowlisted(
        dsn_json: String,
        host: &str,
        port: u16,
        command_timeout: Duration,
    ) -> Result<Self, JsDriverError> {
        let policy = NetPolicy::allowlist(
            vec![HostPort::new(host, port)],
            TRUSTED_DRIVER_MAX_SOCKETS,
            TRUSTED_DRIVER_EGRESS_CEILING,
        )
        .map_err(|e| JsDriverError::transport(format!("invalid MySQL net policy: {e}")))?;
        Self::open_mysql_dsn_json_with_policy(dsn_json, policy, command_timeout)
    }

    pub fn open_mysql_dsn_json_with_policy(
        dsn_json: String,
        net_policy: NetPolicy,
        command_timeout: Duration,
    ) -> Result<Self, JsDriverError> {
        validate_mysql_tls_pin(&dsn_json)?;
        let entry_source = mysql_driver_entry();
        Self::open_with_entry(
            dsn_json,
            &entry_source,
            net_policy,
            command_timeout,
        )
    }

    pub async fn exec(&mut self, sql: &str) -> Result<(), JsDriverError> {
        self.exec_with_binds(sql, &[]).await
    }

    pub async fn exec_with_binds(
        &mut self,
        sql: &str,
        binds: &[BindValue],
    ) -> Result<(), JsDriverError> {
        let _ = self
            .command("exec", sql, binds.iter().map(bind_to_json).collect())
            .await?;
        Ok(())
    }

    pub async fn query_json(
        &mut self,
        sql: &str,
        binds: &[BindValue],
    ) -> Result<RowSet, JsDriverError> {
        let reply = self
            .command("query_json", sql, binds.iter().map(bind_to_json).collect())
            .await?;
        let Value::Array(rows) = reply else {
            return Err(JsDriverError::marshal(
                "query_json reply field 'ok' must be an array",
            ));
        };
        let mut mapped = Vec::with_capacity(rows.len());
        for row in rows {
            match row {
                Value::Object(map) => mapped.push(map),
                other => {
                    return Err(JsDriverError::marshal(format!(
                        "query_json row must be an object, got {other}"
                    )));
                }
            }
        }
        Ok(RowSet { rows: mapped })
    }

    async fn command(
        &mut self,
        kind: &'static str,
        sql: &str,
        binds: Vec<Value>,
    ) -> Result<Value, JsDriverError> {
        if let Some(message) = &self.poisoned {
            return Err(JsDriverError::poisoned(message.clone()));
        }
        if self.in_flight {
            return Err(JsDriverError::transport(
                "JS driver connection is non-reentrant: command already in flight",
            ));
        }
        self.in_flight = true;
        let result = self.command_inner(kind, sql, binds).await;
        self.in_flight = false;
        result
    }

    async fn command_inner(
        &mut self,
        kind: &'static str,
        sql: &str,
        binds: Vec<Value>,
    ) -> Result<Value, JsDriverError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let payload = serde_json::to_string(&DriverCommand {
            id,
            kind,
            sql,
            binds,
        })
        .map_err(|e| JsDriverError::marshal(format!("failed to encode command: {e}")))?;

        let rx = match self.enqueue(id, payload) {
            Ok(rx) => rx,
            Err(err) => {
                self.poison_transport("failed to enqueue JS driver command");
                return Err(err);
            }
        };
        let result_json = match compio::time::timeout(self.command_timeout, rx.recv()).await {
            Ok(json) => json,
            Err(_) => {
                let timeout = self.command_timeout;
                self.poison_timeout(id, timeout);
                return Err(JsDriverError::TimedOut {
                    command_id: id,
                    timeout,
                });
            }
        };
        let reply: DriverReply = serde_json::from_str(&result_json).map_err(|e| {
            JsDriverError::marshal(format!("driver result was not JSON: {e}: {result_json}"))
        })?;
        if let Some(err) = reply.err {
            return Err(JsDriverError::Remote {
                code: err.code,
                sqlstate: err.sqlstate,
                message: err.message,
            });
        }
        reply.ok.ok_or_else(|| {
            JsDriverError::marshal("driver result has neither 'ok' nor 'err' field")
        })
    }

    fn enqueue(&self, id: u64, payload: String) -> Result<ResultReceiver<String>, JsDriverError> {
        let (tx, rx) = result_slot();
        {
            let runtime = self.runtime.as_ref().ok_or_else(|| {
                JsDriverError::poisoned(
                    self.poisoned
                        .clone()
                        .unwrap_or_else(|| "driver runtime has been torn down".to_string()),
                )
            })?;
            let state = runtime.state();
            let mut state = state.borrow_mut();
            let driver = state.js_driver.as_mut().ok_or_else(|| {
                JsDriverError::transport("runtime was not seeded with JS driver state")
            })?;
            if driver.result_senders.insert(id, tx).is_some() {
                return Err(JsDriverError::transport(format!(
                    "duplicate JS driver command id {id}"
                )));
            }
            driver.command_queue.push_back(payload);
        }
        if let Some(runtime) = &self.runtime {
            runtime.notify_pump();
        }
        Ok(rx)
    }

    fn poison_timeout(&mut self, id: u64, timeout: Duration) {
        self.poison_and_teardown(format!(
            "timed out waiting for JS driver command {id} after {timeout:?}"
        ));
    }

    fn poison_transport(&mut self, context: &str) {
        self.poison_and_teardown(context.to_string());
    }

    fn poison_and_teardown(&mut self, message: String) {
        if self.poisoned.is_none() {
            self.poisoned = Some(message);
        }
        drop(self.runtime.take());
    }

    #[cfg(test)]
    pub async fn close_for_test(&mut self, message: impl Into<String>) -> Result<(), JsDriverError> {
        let message = message.into();
        let result = self
            .command("__zs_close_for_test", "", Vec::new())
            .await
            .map(|_| ());
        self.poison_and_teardown(message);
        result
    }

    fn open_with_entry(
        dsn_json: String,
        entry_source: &str,
        net_policy: NetPolicy,
        command_timeout: Duration,
    ) -> Result<Self, JsDriverError> {
        let runtime = Runtime::builder()
            .modules(js_driver_module_graph(entry_source))
            .net_policy(net_policy)
            .js_driver_dsn_json(dsn_json)
            .build();
        runtime.start_pump();
        runtime
            .initialize(&EnvSnapshot::empty())
            .map_err(|e| JsDriverError::transport(format!("driver module init failed: {e}")))?;
        runtime.notify_pump();
        Ok(Self {
            runtime: Some(runtime),
            next_id: 1,
            in_flight: false,
            command_timeout,
            poisoned: None,
        })
    }
}

fn mysql_dsn_net_policy(dsn_json: &str) -> Result<NetPolicy, JsDriverError> {
    let target: MysqlDsnNetTarget = serde_json::from_str(dsn_json).map_err(|e| {
        JsDriverError::transport(format!("invalid MySQL DSN JSON for net policy: {e}"))
    })?;
    let host_port = HostPort::try_new(target.host, target.port)
        .map_err(|e| JsDriverError::transport(format!("invalid MySQL net policy: {e}")))?;
    NetPolicy::allowlist(
        vec![host_port],
        TRUSTED_DRIVER_MAX_SOCKETS,
        TRUSTED_DRIVER_EGRESS_CEILING,
    )
    .map_err(|e| JsDriverError::transport(format!("invalid MySQL net policy: {e}")))
}

fn validate_mysql_tls_pin(dsn_json: &str) -> Result<(), JsDriverError> {
    let dsn: Value = serde_json::from_str(dsn_json).map_err(|e| {
        JsDriverError::transport(format!("invalid MySQL DSN JSON for TLS policy: {e}"))
    })?;
    let Some(ssl) = dsn.get("ssl") else {
        return Ok(());
    };
    match ssl {
        Value::Null | Value::Bool(false) => Ok(()),
        Value::Object(ssl) => {
            if ca_value_contains_pem(ssl.get("ca"))? {
                Ok(())
            } else {
                Err(JsDriverError::transport(MYSQL_TLS_PIN_REQUIRED_MESSAGE))
            }
        }
        Value::Bool(true) | Value::String(_) => {
            Err(JsDriverError::transport(MYSQL_TLS_PIN_REQUIRED_MESSAGE))
        }
        _ => Err(JsDriverError::transport(
            "mysql TLS ssl config must be an object with a non-empty pinned ssl.ca",
        )),
    }
}

fn ca_value_contains_pem(value: Option<&Value>) -> Result<bool, JsDriverError> {
    match value {
        None | Some(Value::Null) => Ok(false),
        Some(Value::String(ca)) => ca_string_contains_pem(ca),
        Some(Value::Array(values)) => {
            let mut saw_pem = false;
            for value in values {
                saw_pem |= ca_value_contains_pem(Some(value))?;
            }
            Ok(saw_pem)
        }
        Some(_) => Err(JsDriverError::transport(
            "mysql TLS ssl.ca must be a PEM-encoded certificate string",
        )),
    }
}

fn ca_string_contains_pem(ca: &str) -> Result<bool, JsDriverError> {
    if ca.trim().is_empty() {
        return Ok(false);
    }
    if ca.contains("-----BEGIN CERTIFICATE-----") && ca.contains("-----END CERTIFICATE-----") {
        return Ok(true);
    }
    Err(JsDriverError::transport(
        "mysql TLS ssl.ca must contain PEM-encoded certificates",
    ))
}

fn bind_to_json(bind: &BindValue) -> Value {
    match bind {
        BindValue::Null => Value::Null,
        BindValue::Bool(v) => Value::Bool(*v),
        BindValue::Int(v) => Value::Number((*v).into()),
        BindValue::Decimal(v) | BindValue::Text(v) => Value::String(v.clone()),
    }
}

pub(crate) fn backend_error(error: JsDriverError) -> crate::apply::executor::BackendError {
    crate::apply::executor::BackendError::new(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::executor::{ApplyError, BackendError};

    #[test]
    fn js_driver_error_downcasts_through_backend_error() {
        let err = JsDriverError::Remote {
            code: 1205,
            sqlstate: "HY000".to_string(),
            message: "lock wait timeout exceeded".to_string(),
        };
        let apply = ApplyError::Db(BackendError::new(err));
        let ApplyError::Db(db) = apply else {
            panic!("expected ApplyError::Db");
        };
        let downcast = db
            .downcast_ref::<JsDriverError>()
            .expect("JsDriverError should downcast");
        assert!(matches!(
            downcast,
            JsDriverError::Remote {
                code: 1205,
                sqlstate,
                ..
            } if sqlstate == "HY000"
        ));
    }

    #[test]
    fn default_mysql_dsn_policy_allows_only_dsn_host_port() {
        let policy = mysql_dsn_net_policy(
            &serde_json::json!({
                "host": "Db.Local.Test",
                "port": 4406,
                "user": "root",
                "database": "zeroship_e2e",
            })
            .to_string(),
        )
        .expect("dsn host:port should derive an allowlist policy");

        assert!(policy.allows_host_port("db.local.test", 4406));
        assert!(!policy.allows_host_port("db.local.test", 3306));
        assert!(!policy.allows_host_port("other.local.test", 4406));
        assert_eq!(policy.max_sockets(), TRUSTED_DRIVER_MAX_SOCKETS);
        assert_eq!(
            policy.egress_ceiling_bytes(),
            Some(TRUSTED_DRIVER_EGRESS_CEILING)
        );
        let NetPolicy::Allowlist { entries, .. } = policy else {
            panic!("default MySQL DSN opener must derive an allowlist policy");
        };
        assert_eq!(entries.as_slice().len(), 1);
        assert_eq!(entries.as_slice()[0].host(), "db.local.test");
        assert_eq!(entries.as_slice()[0].port(), 4406);
    }

    #[test]
    fn mysql_tls_dsn_requires_non_empty_pinned_ca() {
        for ssl in [
            serde_json::json!(true),
            serde_json::json!({}),
            serde_json::json!({ "ca": "" }),
            serde_json::json!({ "ca": [] }),
        ] {
            let dsn = serde_json::json!({
                "host": "db.local.test",
                "port": 3306,
                "ssl": ssl,
            })
            .to_string();
            let err = validate_mysql_tls_pin(&dsn).expect_err("TLS without pinned CA must fail");
            assert!(
                err.to_string().contains("non-empty pinned ssl.ca"),
                "unexpected error: {err}"
            );
        }

        let invalid = serde_json::json!({
            "host": "db.local.test",
            "port": 3306,
            "ssl": { "ca": "not a certificate" },
        })
        .to_string();
        let err = validate_mysql_tls_pin(&invalid).expect_err("invalid PEM must fail");
        assert!(
            err.to_string().contains("PEM-encoded certificates"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn mysql_tls_dsn_accepts_absent_ssl_and_pinned_pem_ca() {
        validate_mysql_tls_pin(
            &serde_json::json!({
                "host": "db.local.test",
                "port": 3306,
            })
            .to_string(),
        )
        .expect("plain MySQL DSN should not require TLS CA");

        validate_mysql_tls_pin(
            &serde_json::json!({
                "host": "db.local.test",
                "port": 3306,
                "ssl": {
                    "ca": "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"
                },
            })
            .to_string(),
        )
        .expect("non-empty PEM CA should satisfy the migrate TLS pin policy");
    }

    #[test]
    fn echo_driver_pump_reuses_session_state_across_separate_awaits() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let mut conn = JsDriverConn::open_echo().expect("echo driver opens");

            conn.exec("first").await.expect("first exec");
            compio::time::sleep(Duration::from_millis(1)).await;

            conn.exec("second").await.expect("second exec");
            compio::time::sleep(Duration::from_millis(1)).await;

            let rows = conn
                .query_json("third", &[BindValue::Text("bound".to_string())])
                .await
                .expect("query_json");
            assert_eq!(rows.rows.len(), 1);
            let row = &rows.rows[0];
            assert_eq!(row.get("command_count"), Some(&Value::from(3)));
            assert_eq!(row.get("last_sql"), Some(&Value::from("third")));
            assert_eq!(row.get("last_kind"), Some(&Value::from("query_json")));
            assert_eq!(row.get("bind_count"), Some(&Value::from(1)));
            assert_eq!(row.get("driver"), Some(&Value::from("echo")));
        });
    }

    #[test]
    fn timed_out_command_poisons_conn_and_blocks_later_commands() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let mut conn =
                JsDriverConn::open_echo_with_timeout(Duration::from_millis(5))
                    .expect("echo driver opens");
            let probe = conn
                .runtime
                .as_ref()
                .expect("driver runtime exists before timeout")
                .clone()
                .into_inner_probe_for_test();

            let first = conn.exec("__zs_echo_delay_after_timeout__").await;
            assert!(matches!(
                first,
                Err(JsDriverError::TimedOut {
                    command_id: 1,
                    timeout,
                }) if timeout == Duration::from_millis(5)
            ));
            assert!(
                conn.runtime.is_none(),
                "timed-out connection should eagerly drop its runtime"
            );
            assert_eq!(
                probe.strong_count(),
                0,
                "timed-out connection should tear down the runtime inner"
            );

            compio::time::sleep(Duration::from_millis(40)).await;

            let later = conn.exec("after timeout").await;
            assert!(matches!(later, Err(JsDriverError::Poisoned { .. })));
        });
    }
}
