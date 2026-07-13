use serde_json::Value;

/// Walk a serialized `pg_query` parse tree and return the first node whose
/// `PascalCase` variant key satisfies `matches`.
///
/// `pg_query` nests real statement nodes under `RawStmt`, CTEs, sub-selects,
/// expression arguments, and other wrappers. Walking the serialized tree visits
/// all of those uniformly, which is the same defense the precondition shape
/// gate uses for data-modifying CTEs.
pub fn first_matching_node<T, F>(v: &Value, matches: &F) -> Option<T>
where
    F: Fn(&str, &Value) -> Option<T>,
{
    match v {
        Value::Object(map) => {
            for (key, child) in map {
                if let Some(found) = matches(key, child) {
                    return Some(found);
                }
            }
            map.values()
                .find_map(|child| first_matching_node(child, matches))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|item| first_matching_node(item, matches)),
        _ => None,
    }
}

/// The first data-modifying statement node key found anywhere in the serialized
/// parse tree, or `None`. DML nodes serialize as the `PascalCase` variant keys
/// `InsertStmt`/`UpdateStmt`/`DeleteStmt`/`MergeStmt` (e.g. a `DeleteStmt` nested
/// in a CTE's `with_clause`).
#[must_use]
pub fn first_dml_node(v: &Value) -> Option<&'static str> {
    first_matching_node(v, &|key, _| match key {
        "InsertStmt" => Some("InsertStmt"),
        "UpdateStmt" => Some("UpdateStmt"),
        "DeleteStmt" => Some("DeleteStmt"),
        "MergeStmt" => Some("MergeStmt"),
        _ => None,
    })
}
