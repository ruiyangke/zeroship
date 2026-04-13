# plugin-db Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete all missing `appbase.db.*` native primitives and operators so the plugin matches the full spec.

**Architecture:** Add missing filter operators, update operators, and 5 new primitives (insertMany, updateMany, deleteMany, distinct, aggregate) to the existing query builder + V8 callback architecture. All changes are in `crates/plugin-db/src/` across three files.

**Tech Stack:** Rust, V8 bindings, PostgreSQL, serde_json

---

## File Map

| File | Changes |
|---|---|
| `crates/plugin-db/src/query.rs` | Add `$ilike`, `$search`, `$not` filter ops; `build_set_clauses()` with `$inc/$dec/$mul/$push/$pull/$addToSet`; `build_insert_many()`, `build_update_many()`, `build_delete_many()`, `build_distinct()`, `build_aggregate()`; add `select` param to `build_find()`; unit tests for each |
| `crates/plugin-db/src/callbacks.rs` | Add `insert_many`, `update_many`, `delete_many`, `distinct`, `aggregate` callbacks; parse `select` in `find` callback |
| `crates/plugin-db/src/lib.rs` | Register new callbacks: `insertMany`, `updateMany`, `deleteMany`, `distinct`, `aggregate` |

---

### Task 1: Add missing filter operators ($ilike, $search, $not)

**Files:**
- Modify: `crates/plugin-db/src/query.rs`

- [ ] **Step 1: Write failing tests for $ilike, $search, $not**

Add to the `#[cfg(test)] mod tests` block at the bottom of `query.rs`:

```rust
#[test]
fn test_ilike_operator() {
    let filter = json!({"name": {"$ilike": "%alice%"}});
    let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
    assert_eq!(q.sql, r#"SELECT * FROM "app1"."users" WHERE "name" ILIKE $1"#);
    assert_eq!(q.params, vec!["%alice%"]);
}

#[test]
fn test_search_operator() {
    let filter = json!({"bio": {"$search": "rust developer"}});
    let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
    assert_eq!(
        q.sql,
        r#"SELECT * FROM "app1"."users" WHERE to_tsvector('english', "bio") @@ plainto_tsquery('english', $1)"#
    );
    assert_eq!(q.params, vec!["rust developer"]);
}

#[test]
fn test_not_operator() {
    let filter = json!({"$not": {"role": "admin"}});
    let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
    assert_eq!(q.sql, r#"SELECT * FROM "app1"."users" WHERE NOT ("role" = $1)"#);
    assert_eq!(q.params, vec!["admin"]);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p appbase-plugin-db -- test_ilike test_search test_not -v`
Expected: FAIL (unsupported operator)

- [ ] **Step 3: Add $ilike and $search to build_field_condition()**

In `query.rs`, inside `build_field_condition()`, add these arms after the `"$like"` arm (before the `other =>` catch-all):

```rust
"$ilike" => {
    let pattern = val.as_str().ok_or_else(|| {
        QueryError::InvalidFilter("$ilike must be a string".to_string())
    })?;
    params.push(pattern.to_string());
    format!("{col} ILIKE ${}", params.len())
}
"$search" => {
    let query_text = val.as_str().ok_or_else(|| {
        QueryError::InvalidFilter("$search must be a string".to_string())
    })?;
    params.push(query_text.to_string());
    format!(
        "to_tsvector('english', {col}) @@ plainto_tsquery('english', ${})",
        params.len()
    )
}
```

- [ ] **Step 4: Add $not to build_where()**

In `query.rs`, inside `build_where()`, add this arm after the `"$or"` arm (before the `other =>` catch-all):

```rust
"$not" => {
    let sub = build_where(value, params)?;
    if !sub.is_empty() {
        conditions.push(format!("NOT ({sub})"));
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p appbase-plugin-db -v`
Expected: ALL PASS

- [ ] **Step 6: Commit**

```bash
git add crates/plugin-db/src/query.rs
git commit -m "feat(plugin-db): add \$ilike, \$search, \$not filter operators"
```

---

### Task 2: Add update operators ($inc, $dec, $mul, $push, $pull, $addToSet)

**Files:**
- Modify: `crates/plugin-db/src/query.rs`

- [ ] **Step 1: Write failing tests for update operators**

Add to the `#[cfg(test)] mod tests` block:

```rust
#[test]
fn test_update_inc() {
    let filter = json!({"id": "abc"});
    let update = json!({"views": {"$inc": 1}});
    let q = build_update_one("app1", "stats", &filter, &update).unwrap();
    assert!(q.sql.contains(r#""views" = "views" + $1"#));
    assert_eq!(q.params[0], "1");
}

#[test]
fn test_update_dec() {
    let filter = json!({"id": "abc"});
    let update = json!({"stock": {"$dec": 1}});
    let q = build_update_one("app1", "products", &filter, &update).unwrap();
    assert!(q.sql.contains(r#""stock" = "stock" - $1"#));
}

#[test]
fn test_update_mul() {
    let filter = json!({"id": "abc"});
    let update = json!({"price": {"$mul": 1.1}});
    let q = build_update_one("app1", "products", &filter, &update).unwrap();
    assert!(q.sql.contains(r#""price" = "price" * $1"#));
}

#[test]
fn test_update_push() {
    let filter = json!({"id": "abc"});
    let update = json!({"tags": {"$push": "new"}});
    let q = build_update_one("app1", "posts", &filter, &update).unwrap();
    assert!(q.sql.contains(r#""tags" = "tags" || to_jsonb($1::text)"#));
}

#[test]
fn test_update_pull() {
    let filter = json!({"id": "abc"});
    let update = json!({"tags": {"$pull": "old"}});
    let q = build_update_one("app1", "posts", &filter, &update).unwrap();
    assert!(q.sql.contains(r#""tags" = "tags" - $1"#));
}

#[test]
fn test_update_add_to_set() {
    let filter = json!({"id": "abc"});
    let update = json!({"tags": {"$addToSet": "unique"}});
    let q = build_update_one("app1", "posts", &filter, &update).unwrap();
    assert!(q.sql.contains("CASE WHEN"));
    assert!(q.sql.contains("@>"));
}

#[test]
fn test_update_mixed_operators() {
    let filter = json!({"id": "abc"});
    let update = json!({"name": "New", "views": {"$inc": 1}});
    let q = build_update_one("app1", "posts", &filter, &update).unwrap();
    assert!(q.sql.contains(r#""name" = $"#));
    assert!(q.sql.contains(r#""views" = "views" + $"#));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p appbase-plugin-db -- test_update_inc test_update_dec test_update_mul test_update_push test_update_pull test_update_add test_update_mixed -v`
Expected: FAIL

- [ ] **Step 3: Extract build_set_clauses() helper and add all operators**

Replace the SET clause logic in `build_update_one()` with a new shared helper. Add this function before `build_update_one()`:

```rust
/// Build SET clauses from an update object, supporting operators.
///
/// Plain values and `$set` → `"col" = $N`
/// `$inc` → `"col" = "col" + $N`
/// `$dec` → `"col" = "col" - $N`
/// `$mul` → `"col" = "col" * $N`
/// `$push` → `"col" = "col" || to_jsonb($N::text)`
/// `$pull` → `"col" = "col" - $N`
/// `$addToSet` → conditional append
fn build_set_clauses(
    update: &Value,
    params: &mut Vec<String>,
) -> Result<Vec<String>, QueryError> {
    let update_obj = update
        .as_object()
        .ok_or_else(|| QueryError::InvalidFilter("update must be an object".to_string()))?;

    let mut clauses = Vec::new();

    for (key, value) in update_obj {
        // Check if value is { $op: val }
        if let Value::Object(inner) = value {
            if let Some((op, val)) = inner.iter().next() {
                if op.starts_with('$') && inner.len() == 1 {
                    let col = quote_ident(key);
                    let clause = match op.as_str() {
                        "$set" => {
                            params.push(value_to_param(val));
                            format!("{col} = ${}", params.len())
                        }
                        "$inc" => {
                            params.push(value_to_param(val));
                            format!("{col} = {col} + ${}", params.len())
                        }
                        "$dec" => {
                            params.push(value_to_param(val));
                            format!("{col} = {col} - ${}", params.len())
                        }
                        "$mul" => {
                            params.push(value_to_param(val));
                            format!("{col} = {col} * ${}", params.len())
                        }
                        "$push" => {
                            params.push(value_to_param(val));
                            format!("{col} = {col} || to_jsonb(${}::text)", params.len())
                        }
                        "$pull" => {
                            params.push(value_to_param(val));
                            format!("{col} = {col} - ${}", params.len())
                        }
                        "$addToSet" => {
                            params.push(value_to_param(val));
                            let p = params.len();
                            format!(
                                "{col} = CASE WHEN {col} @> to_jsonb(${p}::text) THEN {col} ELSE {col} || to_jsonb(${p}::text) END"
                            )
                        }
                        other => {
                            return Err(QueryError::InvalidFilter(format!(
                                "unsupported update operator: {other}"
                            )));
                        }
                    };
                    clauses.push(clause);
                    continue;
                }
            }
        }

        // Plain value — treat as $set
        let col = quote_ident(key);
        params.push(value_to_param(value));
        clauses.push(format!("{col} = ${}", params.len()));
    }

    if clauses.is_empty() {
        return Err(QueryError::InvalidFilter(
            "update fields cannot be empty".to_string(),
        ));
    }

    Ok(clauses)
}
```

- [ ] **Step 4: Rewrite build_update_one() to use build_set_clauses()**

Replace the entire `build_update_one` function body:

```rust
pub fn build_update_one(
    app_id: &str,
    collection: &str,
    filter: &Value,
    update: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let set_clauses = build_set_clauses(update, &mut params)?;
    let where_clause = build_where(filter, &mut params)?;

    let sql = format!(
        "UPDATE {schema}.{table} SET {} WHERE ctid = (SELECT ctid FROM {schema}.{table}{} LIMIT 1) RETURNING *",
        set_clauses.join(", "),
        if where_clause.is_empty() {
            String::new()
        } else {
            format!(" WHERE {where_clause}")
        }
    );

    Ok(BuiltQuery { sql, params })
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p appbase-plugin-db -v`
Expected: ALL PASS

- [ ] **Step 6: Commit**

```bash
git add crates/plugin-db/src/query.rs
git commit -m "feat(plugin-db): add update operators \$inc/\$dec/\$mul/\$push/\$pull/\$addToSet"
```

---

### Task 3: Add insertMany

**Files:**
- Modify: `crates/plugin-db/src/query.rs`
- Modify: `crates/plugin-db/src/callbacks.rs`
- Modify: `crates/plugin-db/src/lib.rs`

- [ ] **Step 1: Write failing test for build_insert_many**

Add to tests in `query.rs`:

```rust
#[test]
fn test_insert_many() {
    let docs = json!([
        {"name": "alice", "age": 30},
        {"name": "bob", "age": 25}
    ]);
    let q = build_insert_many("app1", "users", &docs).unwrap();
    assert!(q.sql.contains("INSERT INTO"));
    assert!(q.sql.contains("VALUES ($1, $2), ($3, $4)"));
    assert!(q.sql.contains("RETURNING *"));
    assert_eq!(q.params.len(), 4);
}

#[test]
fn test_insert_many_empty() {
    let docs = json!([]);
    let result = build_insert_many("app1", "users", &docs);
    assert!(result.is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p appbase-plugin-db -- test_insert_many -v`
Expected: FAIL (function not found)

- [ ] **Step 3: Add build_insert_many() to query.rs**

Add after `build_insert()`:

```rust
/// Build a batch INSERT query: `INSERT INTO ... (...) VALUES (...), (...) RETURNING *`
///
/// All docs must be objects with the same keys. First doc defines the column set.
pub fn build_insert_many(
    app_id: &str,
    collection: &str,
    docs: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let arr = docs.as_array().ok_or_else(|| {
        QueryError::InvalidFilter("insertMany docs must be an array".to_string())
    })?;
    if arr.is_empty() {
        return Err(QueryError::InvalidFilter(
            "insertMany docs cannot be empty".to_string(),
        ));
    }

    // Get column names from first doc
    let first = arr[0].as_object().ok_or_else(|| {
        QueryError::InvalidFilter("each doc must be an object".to_string())
    })?;
    let columns: Vec<&String> = first.keys().collect();

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);
    let col_list = columns.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");

    let mut params: Vec<String> = Vec::new();
    let mut value_groups = Vec::new();

    for doc in arr {
        let obj = doc.as_object().ok_or_else(|| {
            QueryError::InvalidFilter("each doc must be an object".to_string())
        })?;
        let placeholders: Vec<String> = columns
            .iter()
            .map(|col| {
                let val = obj.get(*col).unwrap_or(&Value::Null);
                params.push(value_to_param(val));
                format!("${}", params.len())
            })
            .collect();
        value_groups.push(format!("({})", placeholders.join(", ")));
    }

    let sql = format!(
        "INSERT INTO {schema}.{table} ({col_list}) VALUES {} RETURNING *",
        value_groups.join(", ")
    );

    Ok(BuiltQuery { sql, params })
}
```

- [ ] **Step 4: Add insert_many callback to callbacks.rs**

Add after the `insert` callback:

```rust
// ---------------------------------------------------------------------------
// Callback: insertMany(collection, docsJson)
// ---------------------------------------------------------------------------

/// `appbase.db.insertMany(collection, docsJson)` -> Promise<array>
pub fn insert_many(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(collection) = require_string_arg(scope, &args, 0, "collection") else {
        return;
    };
    let Some(docs) = parse_json_arg(scope, &args, 1) else {
        return;
    };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_insert_many(&app_id, &collection, &docs) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Completed {
                    op_id,
                    value: error_json(&e.to_string()),
                    request_id,
                }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let value = match exec_mutation(bq).await {
            Ok(json) => json, // Already a JSON array of inserted rows
            Err(e) => error_json(&e),
        };
        OpResult::Completed { op_id, value, request_id }
    }));

    rv.set(promise.into());
}
```

- [ ] **Step 5: Register insertMany in lib.rs**

Add to the `register()` method after the `insert` line:

```rust
r.add("insertMany", callbacks::insert_many);
```

- [ ] **Step 6: Run tests and build**

Run: `cargo test -p appbase-plugin-db -v && cargo build --release -p appbase-worker 2>&1 | tail -3`
Expected: ALL PASS, build succeeds

- [ ] **Step 7: Commit**

```bash
git add crates/plugin-db/
git commit -m "feat(plugin-db): add insertMany primitive"
```

---

### Task 4: Add updateMany and deleteMany

**Files:**
- Modify: `crates/plugin-db/src/query.rs`
- Modify: `crates/plugin-db/src/callbacks.rs`
- Modify: `crates/plugin-db/src/lib.rs`

- [ ] **Step 1: Write failing tests**

Add to tests in `query.rs`:

```rust
#[test]
fn test_update_many() {
    let filter = json!({"role": "user"});
    let update = json!({"role": "member"});
    let q = build_update_many("app1", "users", &filter, &update).unwrap();
    assert!(q.sql.contains("UPDATE"));
    assert!(q.sql.contains(r#"SET "role" = $1"#));
    assert!(q.sql.contains(r#"WHERE "role" = $2"#));
    // No ctid subquery
    assert!(!q.sql.contains("ctid"));
}

#[test]
fn test_delete_many() {
    let filter = json!({"status": "expired"});
    let q = build_delete_many("app1", "sessions", &filter).unwrap();
    assert!(q.sql.contains("DELETE FROM"));
    assert!(q.sql.contains(r#"WHERE "status" = $1"#));
    // No ctid subquery
    assert!(!q.sql.contains("ctid"));
}

#[test]
fn test_delete_many_no_filter() {
    let filter = json!({});
    let q = build_delete_many("app1", "sessions", &filter).unwrap();
    assert_eq!(q.sql, r#"DELETE FROM "app1"."sessions" RETURNING *"#);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p appbase-plugin-db -- test_update_many test_delete_many -v`
Expected: FAIL

- [ ] **Step 3: Add build_update_many() to query.rs**

Add after `build_update_one()`:

```rust
/// Build an UPDATE query without LIMIT 1 (updates all matching rows).
/// Returns rows via RETURNING * so caller can count them.
pub fn build_update_many(
    app_id: &str,
    collection: &str,
    filter: &Value,
    update: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let set_clauses = build_set_clauses(update, &mut params)?;
    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!("UPDATE {schema}.{table} SET {}", set_clauses.join(", "));
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }
    sql.push_str(" RETURNING *");

    Ok(BuiltQuery { sql, params })
}
```

- [ ] **Step 4: Add build_delete_many() to query.rs**

Add after `build_delete_one()`:

```rust
/// Build a DELETE query without LIMIT 1 (deletes all matching rows).
/// Returns rows via RETURNING * so caller can count them.
pub fn build_delete_many(
    app_id: &str,
    collection: &str,
    filter: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!("DELETE FROM {schema}.{table}");
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }
    sql.push_str(" RETURNING *");

    Ok(BuiltQuery { sql, params })
}
```

- [ ] **Step 5: Add update_many and delete_many callbacks**

Add to `callbacks.rs`. These return `{ updated: N }` and `{ deleted: N }` respectively:

```rust
// ---------------------------------------------------------------------------
// Callback: updateMany(collection, filterJson, updateJson)
// ---------------------------------------------------------------------------

/// `appbase.db.updateMany(collection, filterJson, updateJson)` -> Promise<{ updated: N }>
pub fn update_many(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(collection) = require_string_arg(scope, &args, 0, "collection") else {
        return;
    };
    let Some(filter) = parse_json_arg(scope, &args, 1) else {
        return;
    };
    let Some(update) = parse_json_arg(scope, &args, 2) else {
        return;
    };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_update_many(&app_id, &collection, &filter, &update) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Completed {
                    op_id,
                    value: error_json(&e.to_string()),
                    request_id,
                }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let value = match exec_mutation(bq).await {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                serde_json::json!({ "updated": arr.len() }).to_string()
            }
            Err(e) => error_json(&e),
        };
        OpResult::Completed { op_id, value, request_id }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: deleteMany(collection, filterJson)
// ---------------------------------------------------------------------------

/// `appbase.db.deleteMany(collection, filterJson)` -> Promise<{ deleted: N }>
pub fn delete_many(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(collection) = require_string_arg(scope, &args, 0, "collection") else {
        return;
    };
    let Some(filter) = parse_json_arg(scope, &args, 1) else {
        return;
    };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_delete_many(&app_id, &collection, &filter) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Completed {
                    op_id,
                    value: error_json(&e.to_string()),
                    request_id,
                }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let value = match exec_mutation(bq).await {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                serde_json::json!({ "deleted": arr.len() }).to_string()
            }
            Err(e) => error_json(&e),
        };
        OpResult::Completed { op_id, value, request_id }
    }));

    rv.set(promise.into());
}
```

- [ ] **Step 6: Register in lib.rs**

Add to `register()`:

```rust
r.add("updateMany", callbacks::update_many);
r.add("deleteMany", callbacks::delete_many);
```

- [ ] **Step 7: Run tests and build**

Run: `cargo test -p appbase-plugin-db -v && cargo build --release -p appbase-worker 2>&1 | tail -3`
Expected: ALL PASS, build succeeds

- [ ] **Step 8: Commit**

```bash
git add crates/plugin-db/
git commit -m "feat(plugin-db): add updateMany and deleteMany primitives"
```

---

### Task 5: Add projection (select) to find

**Files:**
- Modify: `crates/plugin-db/src/query.rs`
- Modify: `crates/plugin-db/src/callbacks.rs`

- [ ] **Step 1: Write failing test**

Add to tests in `query.rs`:

```rust
#[test]
fn test_find_with_select() {
    let filter = json!({});
    let select = json!(["name", "email"]);
    let q = build_find("app1", "users", &filter, None, None, None, Some(&select)).unwrap();
    assert_eq!(q.sql, r#"SELECT "name", "email" FROM "app1"."users""#);
}

#[test]
fn test_find_without_select() {
    let filter = json!({});
    let q = build_find("app1", "users", &filter, None, None, None, None).unwrap();
    assert_eq!(q.sql, r#"SELECT * FROM "app1"."users""#);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p appbase-plugin-db -- test_find_with_select test_find_without_select -v`
Expected: FAIL (wrong number of arguments)

- [ ] **Step 3: Add select parameter to build_find()**

Update the `build_find` signature and body — add `select: Option<&Value>` as the last parameter:

```rust
pub fn build_find(
    app_id: &str,
    collection: &str,
    filter: &Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&Value>,
    select: Option<&Value>,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let columns = match select {
        Some(Value::Array(arr)) if !arr.is_empty() => {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(quote_ident)
                .collect::<Vec<_>>()
                .join(", ")
        }
        _ => "*".to_string(),
    };

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!("SELECT {columns} FROM {schema}.{table}");
    // ... rest unchanged (WHERE, ORDER BY, LIMIT, OFFSET)
```

- [ ] **Step 4: Update all call sites of build_find()**

In `callbacks.rs`, update both `find_one` and `find` callbacks — pass `None` for select in `find_one`, and parse `opts.get("select")` in `find`:

In the `find_one` callback, change:
```rust
let bq = match query::build_find(&app_id, &collection, &filter, Some(1), None, None, None) {
```

In the `find` callback, add select parsing and pass it:
```rust
let select = opts.get("select");
let bq = match query::build_find(&app_id, &collection, &filter, limit, offset, order_by, select) {
```

Update all existing tests that call `build_find` to add `None` as the last argument.

- [ ] **Step 5: Run tests and build**

Run: `cargo test -p appbase-plugin-db -v && cargo build --release -p appbase-worker 2>&1 | tail -3`
Expected: ALL PASS

- [ ] **Step 6: Commit**

```bash
git add crates/plugin-db/
git commit -m "feat(plugin-db): add select/projection to find"
```

---

### Task 6: Add distinct

**Files:**
- Modify: `crates/plugin-db/src/query.rs`
- Modify: `crates/plugin-db/src/callbacks.rs`
- Modify: `crates/plugin-db/src/lib.rs`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn test_distinct() {
    let filter = json!({});
    let q = build_distinct("app1", "users", "role", &filter).unwrap();
    assert_eq!(
        q.sql,
        r#"SELECT DISTINCT "role" FROM "app1"."users" ORDER BY "role""#
    );
}

#[test]
fn test_distinct_with_filter() {
    let filter = json!({"active": true});
    let q = build_distinct("app1", "users", "role", &filter).unwrap();
    assert!(q.sql.contains("DISTINCT"));
    assert!(q.sql.contains(r#"WHERE "active" = $1"#));
}
```

- [ ] **Step 2: Add build_distinct() to query.rs**

```rust
/// Build a SELECT DISTINCT query for a single field.
pub fn build_distinct(
    app_id: &str,
    collection: &str,
    field: &str,
    filter: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);
    let col = quote_ident(field);

    let mut params: Vec<String> = Vec::new();
    let where_clause = build_where(filter, &mut params)?;

    let mut sql = format!("SELECT DISTINCT {col} FROM {schema}.{table}");
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }
    sql.push_str(&format!(" ORDER BY {col}"));

    Ok(BuiltQuery { sql, params })
}
```

- [ ] **Step 3: Add distinct callback to callbacks.rs**

```rust
// ---------------------------------------------------------------------------
// Callback: distinct(collection, field, filterJson)
// ---------------------------------------------------------------------------

/// `appbase.db.distinct(collection, field, filterJson)` -> Promise<array>
pub fn distinct(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(collection) = require_string_arg(scope, &args, 0, "collection") else {
        return;
    };
    let Some(field) = require_string_arg(scope, &args, 1, "field") else {
        return;
    };
    let filter = parse_json_arg(scope, &args, 2).unwrap_or(Value::Object(serde_json::Map::new()));

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_distinct(&app_id, &collection, &field, &filter) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Completed {
                    op_id,
                    value: error_json(&e.to_string()),
                    request_id,
                }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let value = match exec_query(bq).await {
            Ok(json) => {
                // Extract the single field value from each row into a flat array
                let rows: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let values: Vec<Value> = rows
                    .into_iter()
                    .filter_map(|row| {
                        row.as_object()
                            .and_then(|obj| obj.values().next().cloned())
                    })
                    .collect();
                Value::Array(values).to_string()
            }
            Err(e) => error_json(&e),
        };
        OpResult::Completed { op_id, value, request_id }
    }));

    rv.set(promise.into());
}
```

- [ ] **Step 4: Register in lib.rs**

```rust
r.add("distinct", callbacks::distinct);
```

- [ ] **Step 5: Run tests and build**

Run: `cargo test -p appbase-plugin-db -v && cargo build --release -p appbase-worker 2>&1 | tail -3`
Expected: ALL PASS

- [ ] **Step 6: Commit**

```bash
git add crates/plugin-db/
git commit -m "feat(plugin-db): add distinct primitive"
```

---

### Task 7: Add aggregate

**Files:**
- Modify: `crates/plugin-db/src/query.rs`
- Modify: `crates/plugin-db/src/callbacks.rs`
- Modify: `crates/plugin-db/src/lib.rs`

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn test_aggregate_basic() {
    let pipeline = json!([
        {"$match": {"status": "active"}},
        {"$group": {"by": "category", "count": {"$count": true}, "total": {"$sum": "price"}}},
        {"$sort": {"total": -1}},
        {"$limit": 10}
    ]);
    let q = build_aggregate("app1", "products", &pipeline).unwrap();
    assert!(q.sql.contains(r#"COUNT(*) AS "count""#));
    assert!(q.sql.contains(r#"SUM("price") AS "total""#));
    assert!(q.sql.contains(r#"GROUP BY "category""#));
    assert!(q.sql.contains(r#"WHERE "status" = $1"#));
    assert!(q.sql.contains(r#"ORDER BY "total" DESC"#));
    assert!(q.sql.contains("LIMIT 10"));
}

#[test]
fn test_aggregate_multi_group() {
    let pipeline = json!([
        {"$group": {"by": ["region", "category"], "total": {"$sum": "amount"}}}
    ]);
    let q = build_aggregate("app1", "sales", &pipeline).unwrap();
    assert!(q.sql.contains(r#"GROUP BY "region", "category""#));
}

#[test]
fn test_aggregate_having() {
    let pipeline = json!([
        {"$group": {"by": "category", "count": {"$count": true}}},
        {"$having": {"count": {"$gt": 5}}}
    ]);
    let q = build_aggregate("app1", "products", &pipeline).unwrap();
    assert!(q.sql.contains("HAVING"));
}

#[test]
fn test_aggregate_no_group() {
    let pipeline = json!([
        {"$match": {"status": "active"}},
        {"$group": {"count": {"$count": true}, "avg_price": {"$avg": "price"}}}
    ]);
    let q = build_aggregate("app1", "products", &pipeline).unwrap();
    assert!(q.sql.contains("COUNT(*)"));
    assert!(q.sql.contains("AVG"));
    assert!(!q.sql.contains("GROUP BY"));
}
```

- [ ] **Step 2: Add build_aggregate() to query.rs**

```rust
/// Build an aggregate query from a pipeline JSON array.
///
/// Stages: $match, $group, $having, $sort, $limit
pub fn build_aggregate(
    app_id: &str,
    collection: &str,
    pipeline: &Value,
) -> Result<BuiltQuery, QueryError> {
    validate_collection(collection)?;
    validate_schema(app_id)?;

    let stages = pipeline.as_array().ok_or_else(|| {
        QueryError::InvalidFilter("aggregate pipeline must be an array".to_string())
    })?;

    let schema = quote_ident(app_id);
    let table = quote_ident(collection);

    let mut params: Vec<String> = Vec::new();
    let mut where_clause = String::new();
    let mut group_cols: Vec<String> = Vec::new();
    let mut select_parts: Vec<String> = Vec::new();
    let mut having_clause = String::new();
    let mut order_clause = String::new();
    let mut limit_clause = String::new();

    for stage in stages {
        let obj = stage.as_object().ok_or_else(|| {
            QueryError::InvalidFilter("each pipeline stage must be an object".to_string())
        })?;

        for (stage_name, stage_val) in obj {
            match stage_name.as_str() {
                "$match" => {
                    where_clause = build_where(stage_val, &mut params)?;
                }
                "$group" => {
                    let group = stage_val.as_object().ok_or_else(|| {
                        QueryError::InvalidFilter("$group must be an object".to_string())
                    })?;

                    // Parse "by" field (optional — aggregate without GROUP BY)
                    if let Some(by) = group.get("by") {
                        match by {
                            Value::String(s) => {
                                let col = quote_ident(s);
                                select_parts.push(col.clone());
                                group_cols.push(col);
                            }
                            Value::Array(arr) => {
                                for v in arr {
                                    let s = v.as_str().ok_or_else(|| {
                                        QueryError::InvalidFilter(
                                            "$group.by array must contain strings".to_string(),
                                        )
                                    })?;
                                    let col = quote_ident(s);
                                    select_parts.push(col.clone());
                                    group_cols.push(col);
                                }
                            }
                            _ => {
                                return Err(QueryError::InvalidFilter(
                                    "$group.by must be a string or array".to_string(),
                                ));
                            }
                        }
                    }

                    // Parse aggregation functions
                    for (alias, agg_def) in group {
                        if alias == "by" {
                            continue;
                        }
                        let agg_obj = agg_def.as_object().ok_or_else(|| {
                            QueryError::InvalidFilter(format!(
                                "aggregation '{alias}' must be an object"
                            ))
                        })?;
                        let (op, val) = agg_obj.iter().next().ok_or_else(|| {
                            QueryError::InvalidFilter(format!(
                                "aggregation '{alias}' must have an operator"
                            ))
                        })?;
                        let expr = match op.as_str() {
                            "$count" => "COUNT(*)".to_string(),
                            "$sum" => {
                                let field = val.as_str().ok_or_else(|| {
                                    QueryError::InvalidFilter(
                                        "$sum field must be a string".to_string(),
                                    )
                                })?;
                                format!("SUM({})", quote_ident(field))
                            }
                            "$avg" => {
                                let field = val.as_str().ok_or_else(|| {
                                    QueryError::InvalidFilter(
                                        "$avg field must be a string".to_string(),
                                    )
                                })?;
                                format!("AVG({})", quote_ident(field))
                            }
                            "$min" => {
                                let field = val.as_str().ok_or_else(|| {
                                    QueryError::InvalidFilter(
                                        "$min field must be a string".to_string(),
                                    )
                                })?;
                                format!("MIN({})", quote_ident(field))
                            }
                            "$max" => {
                                let field = val.as_str().ok_or_else(|| {
                                    QueryError::InvalidFilter(
                                        "$max field must be a string".to_string(),
                                    )
                                })?;
                                format!("MAX({})", quote_ident(field))
                            }
                            other => {
                                return Err(QueryError::InvalidFilter(format!(
                                    "unsupported aggregation operator: {other}"
                                )));
                            }
                        };
                        select_parts.push(format!("{expr} AS {}", quote_ident(alias)));
                    }
                }
                "$having" => {
                    having_clause = build_where(stage_val, &mut params)?;
                }
                "$sort" => {
                    order_clause = build_order_by(stage_val)?;
                }
                "$limit" => {
                    let n = stage_val.as_i64().ok_or_else(|| {
                        QueryError::InvalidFilter("$limit must be a number".to_string())
                    })?;
                    limit_clause = format!("LIMIT {n}");
                }
                other => {
                    return Err(QueryError::InvalidFilter(format!(
                        "unsupported pipeline stage: {other}"
                    )));
                }
            }
        }
    }

    if select_parts.is_empty() {
        return Err(QueryError::InvalidFilter(
            "aggregate pipeline must contain a $group stage".to_string(),
        ));
    }

    let mut sql = format!(
        "SELECT {} FROM {schema}.{table}",
        select_parts.join(", ")
    );
    if !where_clause.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clause);
    }
    if !group_cols.is_empty() {
        sql.push_str(" GROUP BY ");
        sql.push_str(&group_cols.join(", "));
    }
    if !having_clause.is_empty() {
        sql.push_str(" HAVING ");
        sql.push_str(&having_clause);
    }
    if !order_clause.is_empty() {
        sql.push_str(" ORDER BY ");
        sql.push_str(&order_clause);
    }
    if !limit_clause.is_empty() {
        sql.push(' ');
        sql.push_str(&limit_clause);
    }

    Ok(BuiltQuery { sql, params })
}
```

- [ ] **Step 3: Add aggregate callback to callbacks.rs**

```rust
// ---------------------------------------------------------------------------
// Callback: aggregate(collection, pipelineJson)
// ---------------------------------------------------------------------------

/// `appbase.db.aggregate(collection, pipelineJson)` -> Promise<array>
pub fn aggregate(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(collection) = require_string_arg(scope, &args, 0, "collection") else {
        return;
    };
    let Some(pipeline) = parse_json_arg(scope, &args, 1) else {
        return;
    };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_aggregate(&app_id, &collection, &pipeline) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Completed {
                    op_id,
                    value: error_json(&e.to_string()),
                    request_id,
                }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let value = match exec_query(bq).await {
            Ok(json) => json,
            Err(e) => error_json(&e),
        };
        OpResult::Completed { op_id, value, request_id }
    }));

    rv.set(promise.into());
}
```

- [ ] **Step 4: Register in lib.rs**

```rust
r.add("aggregate", callbacks::aggregate);
```

- [ ] **Step 5: Run tests and build**

Run: `cargo test -p appbase-plugin-db -v && cargo build --release -p appbase-worker 2>&1 | tail -3`
Expected: ALL PASS

- [ ] **Step 6: Commit**

```bash
git add crates/plugin-db/
git commit -m "feat(plugin-db): add aggregate primitive with pipeline stages"
```

---

### Task 8: E2E verification

**Files:**
- None (manual testing against running platform)

- [ ] **Step 1: Build**

```bash
cargo build --release -p appbase-worker -p appbase-control -p appbase-gateway -p appbase
```

- [ ] **Step 2: Start platform and create test app**

Start control, worker (with `--db`), gateway. Create app, create schema + table with columns: `id SERIAL`, `title TEXT`, `body TEXT`, `views INTEGER DEFAULT 0`, `tags JSONB DEFAULT '[]'`, `category TEXT`, `created_at TIMESTAMPTZ DEFAULT NOW()`.

- [ ] **Step 3: Deploy test app and run all primitives**

Deploy a JS app that exports functions exercising every new primitive:

```javascript
export async function testInsertMany() {
    return await appbase.db.insertMany("notes", [
        { title: "A", body: "first", category: "tech", views: 0, tags: [] },
        { title: "B", body: "second", category: "tech", views: 0, tags: [] },
        { title: "C", body: "third", category: "food", views: 0, tags: [] },
    ]);
}
export async function testUpdateMany() {
    return await appbase.db.updateMany("notes", { category: "tech" }, { views: { $inc: 1 } });
}
export async function testDeleteMany() {
    return await appbase.db.deleteMany("notes", { category: "food" });
}
export async function testDistinct() {
    return await appbase.db.distinct("notes", "category", {});
}
export async function testAggregate() {
    return await appbase.db.aggregate("notes", [
        { $group: { by: "category", count: { $count: true }, total_views: { $sum: "views" } } },
        { $sort: { count: -1 } }
    ]);
}
export async function testFindSelect() {
    return await appbase.db.find("notes", {}, { select: ["title", "category"] });
}
export async function testIlike() {
    return await appbase.db.find("notes", { title: { $ilike: "%a%" } }, {});
}
```

Call each via RPC and verify responses:
- `testInsertMany` → array of 3 inserted rows
- `testUpdateMany` → `{ "updated": 2 }` (2 tech notes)
- `testDeleteMany` → `{ "deleted": 1 }` (1 food note)
- `testDistinct` → `["tech"]` (only tech remains)
- `testAggregate` → `[{ "category": "tech", "count": 2, "total_views": 2 }]`
- `testFindSelect` → rows with only `title` and `category` fields
- `testIlike` → filtered results

- [ ] **Step 4: Commit benchmark results (optional)**

```bash
git add docs/superpowers/
git commit -m "docs: add plugin-db completion spec and plan"
```
