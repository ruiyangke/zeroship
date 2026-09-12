//! Decode adapter values into the shared relational query grammar.
use super::*;
use crate::sql::{
    AggregateFunc, AggregateRef, CompareOp, Direction, FieldPath, IdentRole, JoinKind, NullOrder,
    Operand, OrderKey, Predicate, RowLimit, RowOffset,
};

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, DbError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| read::invalid(format!("read requires '{key}'")))
}
fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], DbError> {
    match value.get(key) {
        None => Ok(&[]),
        Some(Value::Array(values)) => Ok(values),
        _ => Err(read::invalid(format!("'{key}' must be an array"))),
    }
}
fn flag(value: &Value, key: &str) -> Result<bool, DbError> {
    match value.get(key) {
        None => Ok(false),
        Some(Value::Bool(v)) => Ok(*v),
        _ => Err(read::invalid(format!("'{key}' must be boolean"))),
    }
}
fn keys(value: &Value, allowed: &[&str]) -> Result<(), DbError> {
    let object = value
        .as_object()
        .ok_or_else(|| read::invalid("read node must be an object"))?;
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(read::invalid(format!("unknown read property '{key}'")));
    }
    Ok(())
}
fn required<'a>(value: &'a Value, key: &str) -> Result<&'a Value, DbError> {
    value
        .get(key)
        .ok_or_else(|| read::invalid(format!("read requires '{key}'")))
}
fn source(value: &Value) -> Result<ReadSource, DbError> {
    keys(value, &["collection", "alias", "includeDeleted"])?;
    let mut source = ReadSource::new(text(value, "collection")?, text(value, "alias")?);
    source.include_deleted = flag(value, "includeDeleted")?;
    Ok(source)
}
fn path(value: &Value) -> Result<FieldPath, DbError> {
    keys(value, &["source", "field"])?;
    Ok(
        FieldPath::column(read::ident(text(value, "field")?, IdentRole::Column)?)
            .in_source(read::ident(text(value, "source")?, IdentRole::Alias)?),
    )
}
fn operand(value: &Value) -> Result<Operand, DbError> {
    if value.get("source").is_some() {
        return Ok(Operand::Path(path(value)?));
    }
    if value.get("aggregate").is_some() {
        keys(value, &["aggregate", "column", "distinct"])?;
        let function = match text(value, "aggregate")? {
            "count" => AggregateFunc::Count,
            "sum" => AggregateFunc::Sum,
            "avg" => AggregateFunc::Avg,
            "min" => AggregateFunc::Min,
            "max" => AggregateFunc::Max,
            _ => return Err(read::invalid("unsupported read aggregate")),
        };
        let distinct = flag(value, "distinct")?;
        let aggregate = match value.get("column") {
            None if function == AggregateFunc::Count && !distinct => AggregateRef::count_rows(),
            Some(column) => AggregateRef::over_path(function, path(column)?, distinct)
                .map_err(|e| read::invalid(e.to_string()))?,
            _ => return Err(read::invalid("aggregate requires a column")),
        };
        return Ok(Operand::Aggregate(aggregate));
    }
    keys(value, &["value"])?;
    crate::sql::Literal::try_from_value(required(value, "value")?.clone())
        .map_err(|error| read::invalid(error.to_string()))?
        .map(Operand::Lit)
        .ok_or_else(|| read::invalid("use a null predicate for null values"))
}
fn predicate(value: &Value, depth: usize, nodes: &mut usize) -> Result<Predicate, DbError> {
    *nodes += 1;
    if depth > crate::sql::MAX_PREDICATE_DEPTH
        || *nodes > crate::sql::joins::MAX_READ_PREDICATE_NODES
    {
        return Err(read::invalid(
            "read predicate exceeds its complexity budget",
        ));
    }
    match text(value, "op")? {
        "and" | "or" => {
            keys(value, &["op", "args"])?;
            let args = array(value, "args")?;
            if args.len() > crate::sql::joins::MAX_READ_PREDICATE_NODES {
                return Err(read::invalid(
                    "read predicate exceeds its complexity budget",
                ));
            }
            let children = args
                .iter()
                .map(|v| predicate(v, depth + 1, nodes))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(if text(value, "op")? == "and" {
                Predicate::And(children)
            } else {
                Predicate::Or(children)
            })
        }
        "not" => {
            keys(value, &["op", "arg"])?;
            Ok(Predicate::Not(Box::new(predicate(
                required(value, "arg")?,
                depth + 1,
                nodes,
            )?)))
        }
        "isNull" | "isNotNull" => {
            keys(value, &["op", "arg"])?;
            Ok(Predicate::IsNull {
                operand: operand(required(value, "arg")?)?,
                negated: text(value, "op")? == "isNotNull",
            })
        }
        operation => {
            keys(value, &["op", "left", "right"])?;
            let op = match operation {
                "eq" => CompareOp::Eq,
                "ne" => CompareOp::Ne,
                "gt" => CompareOp::Gt,
                "gte" => CompareOp::Gte,
                "lt" => CompareOp::Lt,
                "lte" => CompareOp::Lte,
                _ => return Err(read::invalid("unsupported read predicate")),
            };
            Ok(Predicate::Compare {
                lhs: operand(required(value, "left")?)?,
                op,
                rhs: operand(required(value, "right")?)?,
            })
        }
    }
}
impl ReadQuery {
    pub fn decode(value: Value) -> Result<Self, DbError> {
        keys(
            &value,
            &[
                "from", "joins", "select", "where", "groupBy", "having", "orderBy", "limit",
                "offset",
            ],
        )?;
        let mut query = Self::new(source(required(&value, "from")?)?);
        let joins = array(&value, "joins")?;
        if joins.len() >= crate::sql::MAX_READ_SOURCES {
            return Err(read::invalid("read exceeds its source budget"));
        }
        let mut nodes = 0;
        for join in joins {
            keys(join, &["kind", "source", "on"])?;
            let kind = match text(join, "kind")? {
                "inner" => JoinKind::Inner,
                "left" => JoinKind::Left,
                _ => return Err(read::invalid("unsupported join kind")),
            };
            query.joins.push(ReadJoin {
                kind,
                source: source(required(join, "source")?)?,
                on: predicate(required(join, "on")?, 1, &mut nodes)?,
            });
        }
        let select = required(&value, "select")?
            .as_object()
            .ok_or_else(|| read::invalid("select must be a named projection"))?;
        if select.len() > read::MAX_READ_FIELDS {
            return Err(read::invalid("read projection exceeds its field budget"));
        }
        for (name, projection) in select {
            query.projection.push(if projection.get("row").is_some() {
                keys(projection, &["row", "optional", "fields"])?;
                let fields = if projection.get("fields").is_some() {
                    Some(
                        array(projection, "fields")?
                            .iter()
                            .map(|v| {
                                v.as_str().map(String::from).ok_or_else(|| {
                                    read::invalid("projection field must be a string")
                                })
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    )
                } else {
                    None
                };
                ReadProjection::Row {
                    output: name.clone(),
                    source: text(projection, "row")?.into(),
                    optional: flag(projection, "optional")?,
                    fields,
                }
            } else {
                ReadProjection::Scalar {
                    output: name.clone(),
                    expression: operand(projection)?,
                }
            });
        }
        if let Some(filter) = value.get("where") {
            query.filter = predicate(filter, 1, &mut nodes)?;
        }
        if let Some(having) = value.get("having") {
            query.having = predicate(having, 1, &mut nodes)?;
        }
        query.group_by = array(&value, "groupBy")?
            .iter()
            .map(path)
            .collect::<Result<Vec<_>, _>>()?;
        for key in array(&value, "orderBy")? {
            keys(key, &["column", "direction", "nulls"])?;
            query.order_by.push(OrderKey {
                path: path(required(key, "column")?)?,
                direction: match text(key, "direction")? {
                    "asc" => Direction::Ascending,
                    "desc" => Direction::Descending,
                    _ => return Err(read::invalid("invalid sort direction")),
                },
                nulls: match text(key, "nulls")? {
                    "first" => NullOrder::First,
                    "last" => NullOrder::Last,
                    _ => return Err(read::invalid("invalid null order")),
                },
            });
        }
        if let Some(limit) = value.get("limit") {
            query.limit = RowLimit::new(
                limit
                    .as_i64()
                    .ok_or_else(|| read::invalid("limit must be an integer"))?,
            )
            .map_err(|e| read::invalid(e.to_string()))?;
        }
        if let Some(offset) = value.get("offset") {
            query.offset = RowOffset::new(
                offset
                    .as_i64()
                    .ok_or_else(|| read::invalid("offset must be an integer"))?,
            )
            .map_err(|e| read::invalid(e.to_string()))?;
        }
        Ok(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::Literal;
    use crate::value;

    fn query() -> Value {
        value!({"from":{"collection":"records", "alias":"r"}, "select":{"record":{"row":"r"}}})
    }

    #[test]
    fn adapter_predicates_preserve_native_bytes() {
        let bytes = vec![0, 128, 255];
        let mut input = query();
        input["where"] = value!({"op":"eq", "left":{"source":"r", "field":"payload"}, "right":{"value":Value::Bytes(bytes.clone())}});
        let query = ReadQuery::decode(input).unwrap();
        assert!(
            matches!(query.filter, Predicate::Compare { rhs: Operand::Lit(Literal::Bytes(actual)), .. } if actual == bytes)
        );
    }

    #[test]
    fn unknown_stages_and_excessive_predicates_fail_during_decode() {
        let mut input = query();
        input["sql"] = value!("SELECT * FROM records");
        assert!(ReadQuery::decode(input).is_err());
        let mut input = query();
        let mut condition = value!({"op":"isNull", "arg":{"source":"r", "field":"payload"}});
        for _ in 0..crate::sql::MAX_PREDICATE_DEPTH {
            condition = value!({"op":"not", "arg":condition});
        }
        input["where"] = condition;
        assert!(ReadQuery::decode(input).is_err());
    }
}
