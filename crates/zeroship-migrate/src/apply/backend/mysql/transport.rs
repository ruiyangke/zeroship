use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeroship_runtime::channel::{result_slot, ResultReceiver};
use zeroship_runtime::{EnvSnapshot, NetPolicy, Runtime};

use crate::frontend::embedding::js_driver_module_graph;
use crate::render::step::BindValue;

const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const TRUSTED_DRIVER_MAX_SOCKETS: u32 = 4;
const TRUSTED_DRIVER_EGRESS_CEILING: u64 = 64 * 1024 * 1024;

const MYSQL_DRIVER_ENTRY: &str = r#"
import mysql from "mysql2/promise";

function remoteError(err) {
  return {
    code: Number(err?.errno ?? 0),
    sqlstate: err?.sqlState ?? err?.sqlstate ?? "",
    message: err?.message ?? String(err),
  };
}

async function main() {
  const conn = await mysql.createConnection(__zsDriverDsn());
  for (;;) {
    const cmd = await __zsNextCommand();
    try {
      if (cmd.kind === "exec") {
        await conn.execute(cmd.sql, cmd.binds ?? []);
        __zsResolve(cmd.id, { ok: [] });
      } else if (cmd.kind === "query_json") {
        const [rows] = await conn.execute(cmd.sql, cmd.binds ?? []);
        __zsResolve(cmd.id, { ok: rows });
      } else {
        throw new Error(`unknown JS driver command kind: ${cmd.kind}`);
      }
    } catch (err) {
      __zsResolve(cmd.id, { err: remoteError(err) });
    }
  }
}

void main().catch((err) => {
  __zsResolve(0, { err: remoteError(err) });
});

export default {};
"#;

const ECHO_DRIVER_ENTRY: &str = r#"
const session = {
  dsn: __zsDriverDsn(),
  commands: [],
};

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

pub struct JsDriverConn {
    runtime: Runtime,
    next_id: u64,
    in_flight: bool,
    command_timeout: Duration,
}

impl fmt::Debug for JsDriverConn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsDriverConn")
            .field("next_id", &self.next_id)
            .field("in_flight", &self.in_flight)
            .field("command_timeout", &self.command_timeout)
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
        Self::open_with_entry(
            dsn_json,
            MYSQL_DRIVER_ENTRY,
            NetPolicy::trusted(TRUSTED_DRIVER_MAX_SOCKETS, TRUSTED_DRIVER_EGRESS_CEILING),
            command_timeout,
        )
    }

    pub async fn exec(&mut self, sql: &str) -> Result<(), JsDriverError> {
        let _ = self.command("exec", sql, Vec::new()).await?;
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

        let rx = self.enqueue(id, payload)?;
        let result_json = match compio::time::timeout(self.command_timeout, rx.recv()).await {
            Ok(json) => json,
            Err(_) => {
                self.remove_result_sender(id);
                return Err(JsDriverError::transport(format!(
                    "timed out waiting for JS driver command {id}"
                )));
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
            let state = self.runtime.state();
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
        self.runtime.notify_pump();
        Ok(rx)
    }

    fn remove_result_sender(&self, id: u64) {
        let state = self.runtime.state();
        let mut state = state.borrow_mut();
        if let Some(driver) = state.js_driver.as_mut() {
            driver.result_senders.remove(&id);
        }
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
            runtime,
            next_id: 1,
            in_flight: false,
            command_timeout,
        })
    }
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
}
