use std::collections::BTreeSet;

pub(crate) fn validate_relation_names<'a>(
    columns: impl IntoIterator<Item = &'a str>,
    relations: impl IntoIterator<Item = (&'a str, Option<&'a str>, Option<&'a str>)>,
) -> Result<(), String> {
    let columns = columns.into_iter().collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    for (name, target, target_column) in relations {
        let folded = name.to_ascii_lowercase();
        if name.is_empty()
            || name.starts_with('_')
            || name.len() > 63
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || ["__zs_", "__zeroship", "sqlite_"]
                .iter()
                .any(|prefix| folded.starts_with(prefix))
            || matches!(name, "__proto__" | "constructor" | "prototype")
        {
            return Err(format!(
                "relation {name:?} must be a nonreserved output identifier"
            ));
        }
        if columns.contains(name) || !seen.insert(name) {
            return Err(format!(
                "relation {name:?} must be unique and cannot shadow a column"
            ));
        }
        if target.is_none_or(str::is_empty) || target_column.is_none_or(str::is_empty) {
            return Err(format!(
                "relation {name:?} requires an explicit reference table and column"
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_descriptor_relations(
    descriptor: &crate::render::declarative::CollectionDescriptor,
) -> Result<(), String> {
    validate_relation_names(
        descriptor.fields.iter().map(|field| field.name.as_str()),
        descriptor.fields.iter().filter_map(|field| {
            field.relation.as_deref().map(|name| {
                (
                    name,
                    field.references.as_deref(),
                    field.reference_column.as_deref(),
                )
            })
        }),
    )
}
