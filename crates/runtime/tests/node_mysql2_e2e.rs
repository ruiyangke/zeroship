#![allow(unsafe_code)]

#[path = "support/node_realworld.rs"]
mod node_realworld;

use std::time::Duration;

use serde_json::json;

use node_realworld::{
    EnvGuard, allowlist, ensure_mysql, lock_env, module, run_js,
};

const MYSQL2_BUNDLE: &str = include_str!("fixtures/mysql2/mysql2-3.14.1.bundle.mjs");
const MYSQL_PASSWORD: &str = "zeroship";
const MYSQL_DATABASE: &str = "zeroship_e2e";

#[test]
fn unmodified_mysql2_driver_queries_live_mysql_and_pool_temp_table() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();
    let server = ensure_mysql();

    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        run_js(
            format!(
                r#"
import mysql from "./fixtures/mysql2/mysql2-3.14.1.bundle.mjs";

const config = {{
  host: "{host}",
  port: {port},
  user: "root",
  password: "{password}",
  database: "{database}",
}};

async function main() {{
  const connection = await mysql.createConnection(config);
  const [headline] = await connection.query("SELECT 42 AS n, 'ok' AS s");
  await connection.end();

  const pool = mysql.createPool({{
    ...config,
    waitForConnections: true,
    connectionLimit: 4,
    idleTimeout: 1000,
  }});
  const pooled = await pool.getConnection();
  let roundtrip;
  try {{
    await pooled.query("CREATE TEMPORARY TABLE zs_mysql2_e2e (id INT PRIMARY KEY, label VARCHAR(64))");
    await pooled.query("INSERT INTO zs_mysql2_e2e (id, label) VALUES (?, ?)", [7, "pool-ok"]);
    const [rows] = await pooled.query("SELECT id, label FROM zs_mysql2_e2e WHERE id = ?", [7]);
    roundtrip = rows;
  }} finally {{
    pooled.release();
    await pool.end();
  }}

  return {{ headline, pool: roundtrip }};
}}

export default {{
  async fetch() {{
    try {{
      return Response.json(await main());
    }} catch (err) {{
      return new Response(JSON.stringify({{
        name: err?.name ?? "Error",
        code: err?.code ?? null,
        message: err?.message ?? String(err),
        stack: err?.stack ?? null,
      }}), {{ status: 500, headers: {{ "content-type": "application/json" }} }});
    }}
  }},
}};
"#,
                host = server.host,
                port = server.port,
                password = MYSQL_PASSWORD,
                database = MYSQL_DATABASE,
            ),
            vec![module(
                "fixtures/mysql2/mysql2-3.14.1.bundle.mjs",
                MYSQL2_BUNDLE,
            )],
            allowlist(server.host, server.port, 12, 8 * 1024 * 1024),
            Duration::from_secs(45),
        )
        .await
    });

    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    let body: serde_json::Value =
        serde_json::from_str(&result.body).expect("mysql2 e2e response must be JSON");
    assert_eq!(
        body.get("headline"),
        Some(&json!([{ "n": 42, "s": "ok" }])),
        "headline SELECT row mismatch: {body}"
    );
    assert_eq!(
        body.get("pool"),
        Some(&json!([{ "id": 7, "label": "pool-ok" }])),
        "pool temp-table roundtrip mismatch: {body}"
    );
}

