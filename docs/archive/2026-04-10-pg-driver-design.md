# appbase-pg: PostgreSQL Driver for compio

## Goal

Replace sqlx with a minimal, native compio PostgreSQL driver. Eliminates tokio from the platform crate's database layer. Lives at `crates/pg/` as a standalone, reusable crate.

## Architecture

```
crates/pg/
├── src/
│   ├── lib.rs       Public API: Pool, Conn, Row, Transaction, Error, ToSql, FromSql
│   ├── conn.rs      Connection lifecycle + query/execute (uses postgres-protocol)
│   ├── pool.rs      Rc<RefCell<Vec<Conn>>> single-threaded pool
│   └── stream.rs    Buffered compio I/O (TcpStream + optional TLS)
├── Cargo.toml
└── tests/
    └── integration.rs
```

4 source files, ~850 lines total.

## Dependencies

```toml
[package]
name = "appbase-pg"
version = "0.1.0"
edition = "2021"

[dependencies]
postgres-protocol = "0.6"
postgres-types = "0.2"
bytes = "1"
compio = { version = "0.18", features = ["io", "net", "runtime", "macros", "time"] }

[dependencies.compio-tls]
version = "0.5"
features = ["native-tls"]
optional = true

[features]
default = []
tls = ["dep:compio-tls"]

[dev-dependencies]
compio = { version = "0.18", features = ["io", "net", "runtime", "macros", "time"] }
```

No tokio. No async-trait. postgres-protocol provides wire format encoding/decoding, SCRAM-SHA-256 auth, and type serialization — all without any runtime dependency.

## Component Design

### stream.rs — Buffered compio I/O (~200 lines)

Wraps compio's `TcpStream` (or `TlsStream`) with an 8KB userspace read buffer to amortize io_uring submissions. Without buffering, parsing one Postgres message (1-byte tag + 4-byte length + payload) would be 3 separate io_uring SQEs.

```rust
pub struct BufStream {
    stream: StreamInner,
    read_buf: BytesMut,     // 8KB read-ahead buffer
    write_buf: BytesMut,    // accumulator, flushed explicitly
}

enum StreamInner {
    Tcp(TcpStream),
    Tls(TlsStream<TcpStream>),
}
```

Key operations:
- `fill(min_bytes)` — ensure read_buf has at least `min_bytes`, read from socket if needed
- `buf()` — return `&mut BytesMut` for `Message::parse()` to consume from
- `write(data)` — append to write_buf (no I/O)
- `flush()` — send write_buf to socket, clear it

postgres-protocol's `Message::parse(&mut BytesMut)` consumes complete messages from the buffer and returns `None` when incomplete — we call `fill()` and retry.

### conn.rs — Connection + Query (~400 lines)

#### Connect flow

```
Conn::connect(url) → Result<Conn>
  1. Parse URL → host, port, user, password, database, sslmode
  2. TcpStream::connect(host:port).await
  3. TLS negotiation (if sslmode != disable):
     a. Send SSLRequest via frontend::ssl_request()
     b. Read 1 byte: 'S' = upgrade to TLS, 'N' = stay plaintext
     c. If 'S': wrap stream with compio-tls
  4. Send StartupMessage via frontend::startup_message()
  5. Auth loop:
     - AuthenticationSASL → SCRAM-SHA-256 via postgres_protocol::authentication::sasl
     - AuthenticationCleartextPassword → send password_message()
     - AuthenticationOk → done
  6. Consume ParameterStatus*, BackendKeyData, ReadyForQuery
  7. Return Conn { stream, pid, secret, params, status }
```

#### Conn struct

```rust
pub struct Conn {
    stream: BufStream,
    pid: i32,
    secret: i32,
    params: HashMap<String, String>,
    status: u8,  // 'I' idle, 'T' in txn, 'E' error
}
```

#### Query methods

All use the Extended Query Protocol — 5 messages pipelined in one flush:

```rust
impl Conn {
    pub async fn query(&mut self, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Row>>;
    pub async fn execute(&mut self, sql: &str, params: &[&dyn ToSql]) -> Result<u64>;
    pub async fn begin(&mut self) -> Result<Transaction<'_>>;
    pub async fn close(self) -> Result<()>;
}
```

`query()` internal flow:
1. Write: Parse → Bind → Describe('P') → Execute → Sync (all into write_buf)
2. Flush write_buf (one TCP send)
3. Read: ParseComplete → BindComplete → RowDescription → DataRow* → CommandComplete → ReadyForQuery
4. Return Vec<Row> built from RowDescription + DataRow values

`execute()` is the same but skips Describe, ignores DataRows, returns row count from CommandComplete tag.

All messages use the unnamed statement/portal ("") so we don't need statement lifecycle management for our simple CRUD queries.

#### Row type

```rust
pub struct Row {
    columns: Arc<Vec<Column>>,          // shared across rows from same query
    values: Vec<Option<Vec<u8>>>,       // raw binary values from DataRow
}

pub struct Column {
    pub name: String,
    pub oid: u32,       // Postgres type OID
}

impl Row {
    pub fn get<T: FromSql>(&self, column: &str) -> T;
    pub fn try_get<T: FromSql>(&self, column: &str) -> Result<T>;
}
```

Values are stored as raw bytes from the wire. `get()` uses postgres-types' `FromSql` trait to deserialize on access. This avoids upfront conversion of unused columns.

#### Transaction

```rust
pub struct Transaction<'a> {
    conn: &'a mut Conn,
    done: bool,
}

impl Transaction<'_> {
    pub async fn query(&mut self, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Row>>;
    pub async fn execute(&mut self, sql: &str, params: &[&dyn ToSql]) -> Result<u64>;
    pub async fn commit(mut self) -> Result<()>;    // sends COMMIT
    pub async fn rollback(mut self) -> Result<()>;  // sends ROLLBACK
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.done {
            // Set conn.needs_rollback = true.
            // Pool checks this flag on return and sends ROLLBACK before reuse.
            self.conn.needs_rollback = true;
        }
    }
}
```

`begin()` sends `BEGIN` via `execute("BEGIN", &[])`. `commit()`/`rollback()` send `COMMIT`/`ROLLBACK` the same way and set `done = true`. Drop without commit sets `needs_rollback` on the connection — the pool sends ROLLBACK before returning the connection to the idle vec.

### pool.rs — Connection Pool (~150 lines)

Single-threaded, Rc-based pool. Fits compio's thread-per-core model — each worker gets its own pool.

```rust
pub struct Pool {
    conns: RefCell<Vec<Conn>>,
    config: ConnectConfig,
    max_size: usize,
}

impl Pool {
    pub async fn connect(url: &str, max_size: usize) -> Result<Pool>;
    pub async fn get(&self) -> Result<PooledConn<'_>>;
    pub async fn query(&self, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Row>>;
    pub async fn execute(&self, sql: &str, params: &[&dyn ToSql]) -> Result<u64>;
}
```

`get()` flow:
1. Pop from `conns` vec → if available, return it wrapped in `PooledConn`
2. If vec empty and total < max_size → `Conn::connect()` a new one
3. If at max_size → return `Error::Pool("exhausted")`

`PooledConn` wraps `Option<Conn>` and returns the connection to the pool on drop (if still healthy, i.e. `status == 'I'`). Unhealthy connections are dropped (closed).

Convenience methods `query()` and `execute()` on Pool call `get()` internally, so callers don't need to manage connections for simple queries.

### lib.rs — Public API (~100 lines)

```rust
pub use conn::Conn;
pub use pool::{Pool, PooledConn};

pub struct Row { ... }
pub struct Column { ... }
pub struct Transaction<'a> { ... }

pub enum Error {
    Postgres { severity: String, code: String, message: String },
    Io(std::io::Error),
    Protocol(String),
    Auth(String),
    Pool(String),
    Tls(String),
}

pub use postgres_types::{ToSql, FromSql, Type};
```

Re-exports `ToSql`/`FromSql` from postgres-types so callers don't need a direct dependency.

## Error Handling

Postgres errors carry SQLSTATE codes (e.g. `"23505"` = unique violation, `"42P01"` = undefined table). The `Error::Postgres` variant preserves severity, code, and message for caller matching.

I/O errors, protocol violations, and auth failures are separate variants so callers can distinguish infrastructure failures from application errors.

## TLS Negotiation

When `sslmode=prefer` or `sslmode=require` in the connection URL:

1. Send SSLRequest (8-byte message, no tag)
2. Read 1 byte: `'S'` = server accepts TLS, `'N'` = no TLS
3. If `'S'`: upgrade TCP stream to TLS via compio-tls (native-tls backend)
4. If `'N'` and `sslmode=require`: return Error::Tls
5. If `'N'` and `sslmode=prefer`: continue without TLS

The `tls` feature flag gates the compio-tls dependency. Without it, only plaintext connections work.

## Platform Integration

Replace sqlx in two files:

### control/sqlx_registry.rs → control/pg_registry.rs

```rust
use appbase_pg::{Pool, Row, Error};

pub struct PgRegistry {
    pool: Pool,  // Rc-based, single-threaded
}

impl PgRegistry {
    pub async fn new(database_url: &str) -> Result<Self, RegistryError> {
        let pool = Pool::connect(database_url, 4).await?;
        pool.execute("CREATE TABLE IF NOT EXISTS apps (...)", &[]).await?;
        Ok(Self { pool })
    }
}

impl AppRegistry for PgRegistry {
    async fn get_app(&self, id: &str) -> Result<AppData, RegistryError> {
        let rows = self.pool.query(
            "SELECT id, plan_id, server_js, client_html, version, api_key, created_at, updated_at FROM apps WHERE id = $1",
            &[&id],
        ).await?;
        // ...
    }
}
```

### metering/store/sqlite.rs → metering/store/pg.rs

Same pattern — replace sqlx queries with `pool.query()` / `pool.execute()`. Transactions use `pool.get()` + `conn.begin()`.

### Cargo.toml changes

```toml
# Remove:
- sqlx = { version = "0.8", features = ["runtime-tokio", "any", "sqlite", "postgres"] }

# Add:
+ appbase-pg = { path = "../pg", features = ["tls"] }
```

## Testing Strategy

1. Unit tests (no Postgres needed):
   - URL parsing
   - BufStream buffering logic
   - Row column access + type conversion

2. Integration tests (require Postgres):
   - Connect with password auth (SCRAM-SHA-256)
   - Connect with TLS (sslmode=require)
   - Query + execute (the 6 platform queries)
   - Transaction commit + rollback
   - Pool acquire/release/exhaustion
   - Error handling (bad SQL, constraint violation)

3. CI: `docker run -d -e POSTGRES_PASSWORD=test -p 5432:5432 postgres:16`

## Scope Exclusions

Not implementing (YAGNI):
- COPY protocol
- NOTIFY/LISTEN
- Logical replication
- Named prepared statements (we use unnamed only)
- Connection-level pipelining beyond single-query pipeline
- Multi-host / failover
- MD5 authentication (deprecated, SCRAM is standard since Postgres 10)
- Large objects
- Array types beyond what postgres-types provides
