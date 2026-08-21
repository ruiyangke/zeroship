//! SQLite's parser and surgical rewriter for catalog-stored table DDL.
//!
//! These bodies are vendor grammar, so they live with the SQLite backend and are
//! reached by core only through the neutral StoredDdl contract.

use std::collections::BTreeSet;

use zero_migrate_backend::error::DeclarativeError;
use zero_migrate_backend::schema::SchemaRenderer;
use zero_migrate_backend::snapshot::{ColumnSnapshot, ConstraintSnapshot, TableSnapshot};
use zero_migrate_backend::stored_ddl::StoredDdl;

/// True iff `b` is a SQL identifier byte (so a whole-word scan does not match a
/// substring of a larger identifier). ASCII alphanumerics + `_` + `$`. A
/// double-quote is NOT an identifier byte, so `"col"` boundaries match a word.
const fn is_sql_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Count whole-word, case-insensitive occurrences of `needle` in `haystack`.
/// Used by the DROP-COLUMN rebuild router to find references to a dropped
/// column in the stored `CREATE TABLE` DDL (CHECK / generated / partial-index
/// expressions). A match is whole-word so `id` does not match `idx` or `user_id`.
/// Empty `needle` counts zero.
fn word_count_ci(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let hay = haystack.as_bytes();
    let need = needle.as_bytes();
    let mut count = 0;
    let mut start = 0;
    let lower_hay = haystack.to_ascii_lowercase();
    let lower_need = needle.to_ascii_lowercase();
    let lh = lower_hay.as_bytes();
    let ln = lower_need.as_bytes();
    while let Some(pos) = find_sub(&lh[start..], ln) {
        let abs = start + pos;
        let before = abs.checked_sub(1).map(|p| hay[p]);
        let after = hay.get(abs + need.len()).copied();
        let ok_before = before.is_none_or(|b| !is_sql_ident_byte(b));
        let ok_after = after.is_none_or(|b| !is_sql_ident_byte(b));
        if ok_before && ok_after {
            count += 1;
        }
        start = abs + 1;
    }
    count
}

/// True iff `needle` appears as a whole word (case-insensitive) in `haystack`.
fn word_present_ci(haystack: &str, needle: &str) -> bool {
    word_count_ci(haystack, needle) > 0
}

/// First byte offset of `needle` in `haystack` (plain substring search).
fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Locate the outer column/constraint list of a stored SQLite `CREATE TABLE`.
///
/// This deliberately understands SQLite's four identifier/string quoting forms
/// and both comment forms. A comma or parenthesis inside a generated expression,
/// CHECK, quoted default, or comment therefore cannot be mistaken for table DDL
/// structure.
pub(crate) fn sqlite_create_body_bounds(sql: &str) -> Option<(usize, usize)> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Normal,
        Single,
        Double,
        Backtick,
        Bracket,
        LineComment,
        BlockComment,
    }

    let bytes = sql.as_bytes();
    let mut state = State::Normal;
    let mut cursor = 0_usize;
    let mut open = None;
    let mut depth = 0_usize;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        let next = bytes.get(cursor + 1).copied();
        match state {
            State::Normal => match (byte, next) {
                (b'-', Some(b'-')) => {
                    state = State::LineComment;
                    cursor += 2;
                    continue;
                }
                (b'/', Some(b'*')) => {
                    state = State::BlockComment;
                    cursor += 2;
                    continue;
                }
                (b'\'', _) => state = State::Single,
                (b'"', _) => state = State::Double,
                (b'`', _) => state = State::Backtick,
                (b'[', _) => state = State::Bracket,
                (b'(', _) => {
                    if open.is_none() {
                        open = Some(cursor);
                    }
                    depth += 1;
                }
                (b')', _) if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        return open.map(|open| (open, cursor));
                    }
                }
                _ => {}
            },
            State::Single if byte == b'\'' => {
                if next == Some(b'\'') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Double if byte == b'"' => {
                if next == Some(b'"') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Backtick if byte == b'`' => {
                if next == Some(b'`') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Bracket if byte == b']' => {
                if next == Some(b']') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::LineComment if matches!(byte, b'\n' | b'\r') => state = State::Normal,
            State::BlockComment if byte == b'*' && next == Some(b'/') => {
                state = State::Normal;
                cursor += 2;
                continue;
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

/// Split an outer SQLite table body into its column/constraint clauses while
/// retaining each clause's bytes verbatim.
pub(crate) fn sqlite_table_clauses(body: &str) -> Option<Vec<&str>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Normal,
        Single,
        Double,
        Backtick,
        Bracket,
        LineComment,
        BlockComment,
    }

    let bytes = body.as_bytes();
    let mut state = State::Normal;
    let mut depth = 0_usize;
    let mut cursor = 0_usize;
    let mut start = 0_usize;
    let mut clauses = Vec::new();
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        let next = bytes.get(cursor + 1).copied();
        match state {
            State::Normal => match (byte, next) {
                (b'-', Some(b'-')) => {
                    state = State::LineComment;
                    cursor += 2;
                    continue;
                }
                (b'/', Some(b'*')) => {
                    state = State::BlockComment;
                    cursor += 2;
                    continue;
                }
                (b'\'', _) => state = State::Single,
                (b'"', _) => state = State::Double,
                (b'`', _) => state = State::Backtick,
                (b'[', _) => state = State::Bracket,
                (b'(', _) => depth += 1,
                (b')', _) => depth = depth.checked_sub(1)?,
                (b',', _) if depth == 0 => {
                    clauses.push(&body[start..cursor]);
                    start = cursor + 1;
                }
                _ => {}
            },
            State::Single if byte == b'\'' => {
                if next == Some(b'\'') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Double if byte == b'"' => {
                if next == Some(b'"') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Backtick if byte == b'`' => {
                if next == Some(b'`') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Bracket if byte == b']' => {
                if next == Some(b']') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::LineComment if matches!(byte, b'\n' | b'\r') => state = State::Normal,
            State::BlockComment if byte == b'*' && next == Some(b'/') => {
                state = State::Normal;
                cursor += 2;
                continue;
            }
            _ => {}
        }
        cursor += 1;
    }
    if depth != 0
        || matches!(
            state,
            State::Single | State::Double | State::Backtick | State::Bracket | State::BlockComment
        )
    {
        return None;
    }
    clauses.push(&body[start..]);
    Some(clauses)
}

fn sqlite_skip_space_and_comments(sql: &str, cursor: &mut usize) {
    let bytes = sql.as_bytes();
    loop {
        while bytes.get(*cursor).is_some_and(u8::is_ascii_whitespace) {
            *cursor += 1;
        }
        if bytes.get(*cursor..*cursor + 2) == Some(b"--") {
            *cursor += 2;
            while bytes
                .get(*cursor)
                .is_some_and(|byte| !matches!(byte, b'\n' | b'\r'))
            {
                *cursor += 1;
            }
            continue;
        }
        if bytes.get(*cursor..*cursor + 2) == Some(b"/*") {
            *cursor += 2;
            while *cursor + 1 < bytes.len() && bytes.get(*cursor..*cursor + 2) != Some(b"*/") {
                *cursor += 1;
            }
            *cursor = (*cursor + 2).min(bytes.len());
            continue;
        }
        break;
    }
}

/// Consume one SQLite DDL word or quoted identifier. Quoted identifier escapes
/// are decoded so catalog and desired constraint names can be compared by name.
pub(crate) fn sqlite_ddl_word(sql: &str, cursor: &mut usize) -> Option<String> {
    sqlite_skip_space_and_comments(sql, cursor);
    let bytes = sql.as_bytes();
    let first = *bytes.get(*cursor)?;
    let (close, doubled) = match first {
        // SQLite accepts single-quoted identifiers in legacy schema text when
        // the token appears in an identifier position (notably column names).
        b'\'' => (b'\'', true),
        b'"' => (b'"', true),
        b'`' => (b'`', true),
        b'[' => (b']', true),
        _ => {
            let start = *cursor;
            while bytes.get(*cursor).is_some_and(|byte| {
                !byte.is_ascii_whitespace() && !matches!(byte, b'(' | b')' | b',' | b'.' | b';')
            }) {
                *cursor += 1;
            }
            return (*cursor > start).then(|| sql[start..*cursor].to_string());
        }
    };

    *cursor += 1;
    let mut word = Vec::new();
    while let Some(&byte) = bytes.get(*cursor) {
        if byte == close {
            if doubled && bytes.get(*cursor + 1) == Some(&close) {
                word.push(close);
                *cursor += 2;
                continue;
            }
            *cursor += 1;
            return String::from_utf8(word).ok();
        }
        word.push(byte);
        *cursor += 1;
    }
    None
}

pub(crate) fn sqlite_first_ddl_word_is_quoted(sql: &str) -> bool {
    let mut cursor = 0_usize;
    sqlite_skip_space_and_comments(sql, &mut cursor);
    sql.as_bytes()
        .get(cursor)
        .is_some_and(|byte| matches!(byte, b'\'' | b'"' | b'`' | b'['))
}

fn sqlite_named_foreign_key_clause(clause: &str) -> Option<String> {
    let mut cursor = 0_usize;
    if !sqlite_ddl_word(clause, &mut cursor)?.eq_ignore_ascii_case("CONSTRAINT") {
        return None;
    }
    let name = sqlite_ddl_word(clause, &mut cursor)?;
    if !sqlite_ddl_word(clause, &mut cursor)?.eq_ignore_ascii_case("FOREIGN")
        || !sqlite_ddl_word(clause, &mut cursor)?.eq_ignore_ascii_case("KEY")
    {
        return None;
    }
    Some(name)
}

fn sqlite_constraint_by_name<'a>(
    constraints: &'a [ConstraintSnapshot],
    name: &str,
) -> Option<&'a ConstraintSnapshot> {
    constraints.iter().find(|constraint| {
        constraint.kind == "FOREIGN KEY" && constraint.name.eq_ignore_ascii_case(name)
    })
}

/// Find an unquoted SQLite keyword, ignoring strings, quoted identifiers, and
/// comments. This must not classify `DEFAULT 'generated'` as a generated column:
/// doing so would omit an ordinary column from the rebuild copy and lose data.
fn sqlite_has_unquoted_keyword(sql: &str, keyword: &str) -> bool {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Normal,
        Single,
        Double,
        Backtick,
        Bracket,
        LineComment,
        BlockComment,
    }

    let bytes = sql.as_bytes();
    let mut state = State::Normal;
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        let next = bytes.get(cursor + 1).copied();
        match state {
            State::Normal => match (byte, next) {
                (b'-', Some(b'-')) => {
                    state = State::LineComment;
                    cursor += 2;
                    continue;
                }
                (b'/', Some(b'*')) => {
                    state = State::BlockComment;
                    cursor += 2;
                    continue;
                }
                (b'\'', _) => state = State::Single,
                (b'"', _) => state = State::Double,
                (b'`', _) => state = State::Backtick,
                (b'[', _) => state = State::Bracket,
                (_, _) if is_sql_ident_byte(byte) => {
                    let start = cursor;
                    while bytes
                        .get(cursor)
                        .is_some_and(|byte| is_sql_ident_byte(*byte))
                    {
                        cursor += 1;
                    }
                    if sql[start..cursor].eq_ignore_ascii_case(keyword) {
                        return true;
                    }
                    continue;
                }
                _ => {}
            },
            State::Single if byte == b'\'' => {
                if next == Some(b'\'') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Double if byte == b'"' => {
                if next == Some(b'"') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Backtick if byte == b'`' => {
                if next == Some(b'`') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Bracket if byte == b']' => {
                if next == Some(b']') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::LineComment if matches!(byte, b'\n' | b'\r') => state = State::Normal,
            State::BlockComment if byte == b'*' && next == Some(b'/') => {
                state = State::Normal;
                cursor += 2;
                continue;
            }
            _ => {}
        }
        cursor += 1;
    }
    false
}

/// Return column names whose stored definition is explicitly generated. SQLite's
/// `PRAGMA table_info` omits generated columns on supported versions, but this
/// extra filter makes the copy mapping safe if a catalog adapter ever exposes
/// them (inserting into a generated column is illegal).
pub(crate) fn sqlite_generated_columns(create_sql: &str) -> BTreeSet<String> {
    let Some((open, close)) = sqlite_create_body_bounds(create_sql) else {
        return BTreeSet::new();
    };
    let Some(clauses) = sqlite_table_clauses(&create_sql[open + 1..close]) else {
        return BTreeSet::new();
    };
    clauses
        .into_iter()
        .filter_map(|clause| {
            let mut cursor = 0_usize;
            let quoted_name = sqlite_first_ddl_word_is_quoted(clause);
            let name = sqlite_ddl_word(clause, &mut cursor)?;
            if !quoted_name
                && matches!(
                    name.to_ascii_uppercase().as_str(),
                    "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN"
                )
            {
                return None;
            }
            sqlite_has_unquoted_keyword(&clause[cursor..], "GENERATED").then_some(name)
        })
        .collect()
}

/// Rewrite only the primary-key clauses in a catalog-stored SQLite `CREATE
/// TABLE`, preserving every unrelated column facet, constraint, comment, and
/// trailing table option verbatim. The caller has already verified the exact
/// live key and all lifecycle prerequisites from the catalog.
pub(crate) fn rewrite_sqlite_stored_primary_key(
    table: &str,
    stored: &str,
    target_columns: Option<&[String]>,
    materialize_not_null: Option<&str>,
    backend: &dyn SchemaRenderer,
) -> Result<String, DeclarativeError> {
    let (open, close) = sqlite_create_body_bounds(stored).ok_or_else(|| {
        DeclarativeError::Invalid(format!(
            "SQLite primary-key rebuild of '{table}' could not parse its stored CREATE TABLE body"
        ))
    })?;
    let clauses = sqlite_table_clauses(&stored[open + 1..close]).ok_or_else(|| {
        DeclarativeError::Invalid(format!(
            "SQLite primary-key rebuild of '{table}' found malformed stored CREATE TABLE clauses"
        ))
    })?;

    let mut rewritten = Vec::with_capacity(clauses.len() + usize::from(target_columns.is_some()));
    for clause in clauses {
        let mut cursor = 0_usize;
        let quoted_name = sqlite_first_ddl_word_is_quoted(clause);
        let Some(first) = sqlite_ddl_word(clause, &mut cursor) else {
            return Err(DeclarativeError::Invalid(format!(
                "SQLite primary-key rebuild of '{table}' found an empty table clause"
            )));
        };
        let first_upper = first.to_ascii_uppercase();
        if !quoted_name
            && matches!(first_upper.as_str(), "PRIMARY" | "CONSTRAINT")
            && sqlite_table_primary_key_clause(clause)
        {
            continue;
        }
        if !quoted_name && matches!(first_upper.as_str(), "UNIQUE" | "CHECK" | "FOREIGN") {
            rewritten.push(clause.to_string());
            continue;
        }

        if let Some((start, end)) = sqlite_inline_primary_key_span(clause) {
            let mut ordinary = String::with_capacity(clause.len() + 9);
            ordinary.push_str(&clause[..start]);
            ordinary.push_str(&clause[end..]);
            if materialize_not_null.is_some_and(|column| first.eq_ignore_ascii_case(column))
                && !sqlite_column_clause_has_not_null(&ordinary)
            {
                ordinary.push_str(" NOT NULL");
            }
            rewritten.push(ordinary);
        } else if materialize_not_null.is_some_and(|column| first.eq_ignore_ascii_case(column)) {
            let mut ordinary = clause.to_string();
            if !sqlite_column_clause_has_not_null(&ordinary) {
                ordinary.push_str(" NOT NULL");
            }
            rewritten.push(ordinary);
        } else {
            rewritten.push(clause.to_string());
        }
    }

    if let Some(columns) = target_columns {
        let columns = columns
            .iter()
            .map(|column| backend.quote_ident(column))
            .collect::<Vec<_>>()
            .join(", ");
        rewritten.push(format!("PRIMARY KEY ({columns})"));
    }

    Ok(format!(
        "{}({}){}",
        &stored[..open],
        rewritten.join(","),
        &stored[close + 1..]
    ))
}

fn sqlite_column_clause_has_not_null(clause: &str) -> bool {
    sqlite_unquoted_top_level_words(clause)
        .windows(2)
        .any(|pair| pair[0].0.eq_ignore_ascii_case("NOT") && pair[1].0.eq_ignore_ascii_case("NULL"))
}

/// Whether the table-level suffix contains SQLite's `WITHOUT ROWID` option.
/// Only the bytes after the parsed outer body are inspected, so a string,
/// comment, default, or CHECK expression inside the body cannot spoof it.
pub(crate) fn sqlite_create_is_without_rowid(create_sql: &str) -> bool {
    let Some((_, close)) = sqlite_create_body_bounds(create_sql) else {
        return false;
    };
    sqlite_unquoted_top_level_words(&create_sql[close + 1..])
        .windows(2)
        .any(|pair| {
            pair[0].0.eq_ignore_ascii_case("WITHOUT") && pair[1].0.eq_ignore_ascii_case("ROWID")
        })
}

/// SQLite's historical exception: an inline `INTEGER PRIMARY KEY DESC` is not
/// a rowid alias. The table-constraint spelling does not have that exception.
pub(crate) fn sqlite_inline_primary_key_is_desc(create_sql: &str, column: &str) -> bool {
    let Some((open, close)) = sqlite_create_body_bounds(create_sql) else {
        return false;
    };
    let Some(clauses) = sqlite_table_clauses(&create_sql[open + 1..close]) else {
        return false;
    };
    clauses.into_iter().any(|clause| {
        let mut cursor = 0_usize;
        let Some(name) = sqlite_ddl_word(clause, &mut cursor) else {
            return false;
        };
        if !name.eq_ignore_ascii_case(column) {
            return false;
        }
        let words = sqlite_unquoted_top_level_words(&clause[cursor..]);
        words.windows(3).any(|triple| {
            triple[0].0.eq_ignore_ascii_case("PRIMARY")
                && triple[1].0.eq_ignore_ascii_case("KEY")
                && triple[2].0.eq_ignore_ascii_case("DESC")
        })
    })
}

fn sqlite_table_primary_key_clause(clause: &str) -> bool {
    let words = sqlite_unquoted_top_level_words(clause);
    words.windows(2).any(|pair| {
        pair[0].0.eq_ignore_ascii_case("PRIMARY") && pair[1].0.eq_ignore_ascii_case("KEY")
    })
}

/// Byte span of an inline `PRIMARY KEY ... [AUTOINCREMENT]` column constraint.
fn sqlite_inline_primary_key_span(clause: &str) -> Option<(usize, usize)> {
    let words = sqlite_unquoted_top_level_words(clause);
    let primary = words.windows(2).position(|pair| {
        pair[0].0.eq_ignore_ascii_case("PRIMARY") && pair[1].0.eq_ignore_ascii_case("KEY")
    })?;
    // A named column constraint may quote its name. Quoted identifiers are
    // deliberately absent from `sqlite_unquoted_top_level_words`, so these two
    // valid spellings have different preceding-word shapes:
    //
    //   CONSTRAINT pk_name PRIMARY KEY       => CONSTRAINT, pk_name, PRIMARY
    //   CONSTRAINT "pk name" PRIMARY KEY     => CONSTRAINT, PRIMARY
    //
    // Consume the whole named constraint in both cases. Leaving a quoted
    // `CONSTRAINT "name"` prefix behind would make the rebuilt CREATE TABLE
    // invalid after removing its PRIMARY KEY body.
    let start = if primary >= 2 && words[primary - 2].0.eq_ignore_ascii_case("CONSTRAINT") {
        words[primary - 2].1
    } else if primary >= 1 && words[primary - 1].0.eq_ignore_ascii_case("CONSTRAINT") {
        words[primary - 1].1
    } else {
        words[primary].1
    };
    let mut next = primary + 2;
    if words
        .get(next)
        .is_some_and(|word| matches!(word.0.to_ascii_uppercase().as_str(), "ASC" | "DESC"))
    {
        next += 1;
    }
    if words.get(next..next + 2).is_some_and(|pair| {
        pair[0].0.eq_ignore_ascii_case("ON") && pair[1].0.eq_ignore_ascii_case("CONFLICT")
    }) {
        next += 2;
        if words.get(next).is_some_and(|word| {
            matches!(
                word.0.to_ascii_uppercase().as_str(),
                "ROLLBACK" | "ABORT" | "FAIL" | "IGNORE" | "REPLACE"
            )
        }) {
            next += 1;
        }
    }
    if words
        .get(next)
        .is_some_and(|word| word.0.eq_ignore_ascii_case("AUTOINCREMENT"))
    {
        next += 1;
    }
    let end = words.get(next.saturating_sub(1))?.2;
    Some((start, end))
}

/// Unquoted SQL words at parenthesis depth zero, with byte spans. This is enough
/// to recognize SQLite column/table constraint grammar without mistaking text in
/// CHECK expressions, defaults, strings, identifiers, or comments for keywords.
fn sqlite_unquoted_top_level_words(sql: &str) -> Vec<(String, usize, usize)> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Normal,
        Single,
        Double,
        Backtick,
        Bracket,
        LineComment,
        BlockComment,
    }
    let bytes = sql.as_bytes();
    let mut state = State::Normal;
    let mut depth = 0_usize;
    let mut cursor = 0_usize;
    let mut words = Vec::new();
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        let next = bytes.get(cursor + 1).copied();
        match state {
            State::Normal => match (byte, next) {
                (b'-', Some(b'-')) => {
                    state = State::LineComment;
                    cursor += 2;
                    continue;
                }
                (b'/', Some(b'*')) => {
                    state = State::BlockComment;
                    cursor += 2;
                    continue;
                }
                (b'\'', _) => state = State::Single,
                (b'"', _) => state = State::Double,
                (b'`', _) => state = State::Backtick,
                (b'[', _) => state = State::Bracket,
                (b'(', _) => depth += 1,
                (b')', _) => depth = depth.saturating_sub(1),
                (_, _) if depth == 0 && (byte.is_ascii_alphabetic() || byte == b'_') => {
                    let start = cursor;
                    cursor += 1;
                    while bytes.get(cursor).is_some_and(|value| {
                        value.is_ascii_alphanumeric() || matches!(value, b'_' | b'$')
                    }) {
                        cursor += 1;
                    }
                    words.push((sql[start..cursor].to_string(), start, cursor));
                    continue;
                }
                _ => {}
            },
            State::Single if byte == b'\'' => {
                if next == Some(b'\'') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Double if byte == b'"' => {
                if next == Some(b'"') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Backtick if byte == b'`' => {
                if next == Some(b'`') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Bracket if byte == b']' => state = State::Normal,
            State::LineComment if matches!(byte, b'\n' | b'\r') => state = State::Normal,
            State::BlockComment if byte == b'*' && next == Some(b'/') => {
                state = State::Normal;
                cursor += 2;
                continue;
            }
            _ => {}
        }
        cursor += 1;
    }
    words
}

/// Reconcile only named table-level FOREIGN KEY clauses in SQLite's verbatim
/// stored `CREATE TABLE`, leaving column definitions (including defaults,
/// generated expressions, collations, and inline checks), unrelated table
/// constraints, comments, and trailing table options untouched.
fn rewrite_sqlite_stored_foreign_keys(
    table: &str,
    stored: &str,
    live: &TableSnapshot,
    desired: &TableSnapshot,
    backend: &dyn SchemaRenderer,
) -> Result<String, DeclarativeError> {
    let (open, close) = sqlite_create_body_bounds(stored).ok_or_else(|| {
        DeclarativeError::Invalid(format!(
            "SQLite constraint rebuild of '{table}' could not parse its stored CREATE TABLE body"
        ))
    })?;
    let clauses = sqlite_table_clauses(&stored[open + 1..close]).ok_or_else(|| {
        DeclarativeError::Invalid(format!(
            "SQLite constraint rebuild of '{table}' found malformed stored CREATE TABLE clauses"
        ))
    })?;
    let mut rewritten = Vec::with_capacity(clauses.len() + desired.constraints.len());
    let mut seen_names = BTreeSet::new();
    for clause in clauses {
        let Some(name) = sqlite_named_foreign_key_clause(clause) else {
            rewritten.push(clause.to_string());
            continue;
        };
        let folded = name.to_ascii_lowercase();
        if !seen_names.insert(folded) {
            return Err(DeclarativeError::Invalid(format!(
                "SQLite constraint rebuild of '{table}' found duplicate stored foreign key name {name:?}"
            )));
        }
        if let Some(constraint) = sqlite_constraint_by_name(&desired.constraints, &name) {
            rewritten.push(format!(
                "CONSTRAINT {} {}",
                backend.quote_ident(&constraint.name),
                constraint.definition
            ));
        } else if sqlite_constraint_by_name(&live.constraints, &name).is_none() {
            // A named FK absent from the structured snapshot is not ours to
            // reconcile. Preserve it rather than deleting unmanaged DDL.
            rewritten.push(clause.to_string());
        }
        // Otherwise this is a known live FK absent from desired: omit it.
    }

    for desired_fk in desired
        .constraints
        .iter()
        .filter(|constraint| constraint.kind == "FOREIGN KEY")
    {
        let folded = desired_fk.name.to_ascii_lowercase();
        if seen_names.contains(&folded) {
            continue;
        }
        match sqlite_constraint_by_name(&live.constraints, &desired_fk.name) {
            Some(live_fk) if live_fk.definition == desired_fk.definition => {
                // An unnamed/column-level live FK has only a synthetic catalog
                // name. Its original clause was preserved above; do not append a
                // second relationship merely because no declared name was parsed.
            }
            Some(_) => {
                return Err(DeclarativeError::Invalid(format!(
                    "SQLite constraint rebuild of '{table}' cannot replace foreign key {:?}: its named table-level clause was not found in stored CREATE TABLE",
                    desired_fk.name
                )));
            }
            None => rewritten.push(format!(
                "CONSTRAINT {} {}",
                backend.quote_ident(&desired_fk.name),
                desired_fk.definition
            )),
        }
    }
    for live_fk in live
        .constraints
        .iter()
        .filter(|constraint| constraint.kind == "FOREIGN KEY")
    {
        if sqlite_constraint_by_name(&desired.constraints, &live_fk.name).is_none()
            && !seen_names.contains(&live_fk.name.to_ascii_lowercase())
        {
            return Err(DeclarativeError::Invalid(format!(
                "SQLite constraint rebuild of '{table}' cannot drop foreign key {:?}: its named table-level clause was not found in stored CREATE TABLE",
                live_fk.name
            )));
        }
    }

    Ok(format!(
        "{}({}){}",
        &stored[..open],
        rewritten.join(","),
        &stored[close + 1..]
    ))
}

/// The virtual-table module of a stored `CREATE VIRTUAL TABLE … USING <module>(…)`
/// statement, or `None` when `sql` is not a virtual-table create.
///
/// Recognition is keyed on the `CREATE … VIRTUAL TABLE … USING <module>` token
/// SHAPE, never on a module allowlist or a table-name convention — an `fts5`
/// vtable, a `vec0` vtable and a module this engine has never heard of are all
/// recognised on identical terms. Only the header (everything ahead of the first
/// `(`) is tokenised, so a column or option named `virtual` inside the argument
/// list cannot promote an ordinary table into a virtual one.
///
/// Returns `None` on every non-SQLite snapshot, where `stored_create_sql` is
/// absent — PostgreSQL has no virtual tables and nothing to guard.
fn virtual_table_module(sql: &str) -> Option<String> {
    let head = &sql[..sql.find('(').unwrap_or(sql.len())];
    let lower = head.to_ascii_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    // `VIRTUAL TABLE` must appear as adjacent tokens, preceded by `CREATE`.
    let vt = tokens.windows(2).position(|w| w == ["virtual", "table"])?;
    if !tokens[..vt].contains(&"create") {
        return None;
    }
    // The module name is the token after `USING`, which follows `VIRTUAL TABLE`.
    let using = tokens.iter().position(|t| *t == "using")?;
    if using < vt {
        return None;
    }
    let module: String = tokens
        .get(using + 1)?
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!module.is_empty()).then_some(module)
}

#[cfg(test)]
mod virtual_table_module_tests {
    use super::virtual_table_module;

    /// The FTS5 shape the engine and plugin-db's data plane both emit.
    #[test]
    fn recognises_an_fts5_external_content_vtable() {
        assert_eq!(
            virtual_table_module(
                r#"CREATE VIRTUAL TABLE IF NOT EXISTS "posts__fts" USING fts5("body", content="posts", content_rowid="rowid")"#
            )
            .as_deref(),
            Some("fts5")
        );
    }

    /// The `vec0` shape plugin-db's runtime `ensure_vector_index` emits, verbatim.
    /// The guard cannot be exercised end-to-end against a real `vec0` table (the
    /// hardened connection refuses to load `sqlite-vec`), so its DDL is pinned here
    /// instead — recognition of the shape is proven; the live drop of one is not.
    #[test]
    fn recognises_a_vec0_vtable() {
        assert_eq!(
            virtual_table_module(
                r#"CREATE VIRTUAL TABLE IF NOT EXISTS "docs__vec_embedding" USING vec0("embedding" float[768] distance_metric=cosine)"#
            )
            .as_deref(),
            Some("vec0")
        );
    }

    /// The point of keying on the token shape: a module this engine has never heard
    /// of is recognised on identical terms, with no allowlist to fall out of date.
    #[test]
    fn recognises_a_module_the_engine_has_never_heard_of() {
        assert_eq!(
            virtual_table_module(r#"CREATE VIRTUAL TABLE "t" USING some_future_module(a, b)"#)
                .as_deref(),
            Some("some_future_module")
        );
        assert_eq!(
            virtual_table_module(r#"CREATE TEMP VIRTUAL TABLE "t" USING rtree(id, minX, maxX)"#)
                .as_deref(),
            Some("rtree")
        );
    }

    /// An ordinary table is not a virtual one, however it is spelled.
    #[test]
    fn an_ordinary_create_table_is_not_a_virtual_table() {
        assert_eq!(
            virtual_table_module(r#"CREATE TABLE "posts" ("body" TEXT)"#),
            None
        );
        // A shadow table: a PLAIN table, so the guard does not fire on it. The
        // parent vtable is what stops the drop pass.
        assert_eq!(
            virtual_table_module(
                r#"CREATE TABLE 'posts__fts_data'(id INTEGER PRIMARY KEY, block BLOB)"#
            ),
            None
        );
    }

    /// Only the header is tokenised, so a COLUMN named `virtual` cannot promote an
    /// ordinary table into a virtual one and block its legitimate drop.
    #[test]
    fn a_column_named_virtual_does_not_forge_a_virtual_table() {
        assert_eq!(
            virtual_table_module(r#"CREATE TABLE "t" ("virtual" TEXT, "table" TEXT)"#),
            None
        );
    }

    /// A virtual-table create with no `USING` clause is malformed; refuse to guess
    /// a module rather than reporting an empty one.
    #[test]
    fn a_vtable_create_without_a_using_clause_yields_no_module() {
        assert_eq!(virtual_table_module(r#"CREATE VIRTUAL TABLE "t""#), None);
    }
}

#[derive(Debug)]
pub(super) struct SqliteStoredDdl;

pub(super) static PARSER: SqliteStoredDdl = SqliteStoredDdl;

impl SqliteStoredDdl {
    /// Is the live column `lc` (being dropped) referenced by any
    /// index, constraint, or raw-DDL dependent of the live table `lt`, such that a
    /// native `SQLite` `DROP COLUMN` would ERROR? Returns `Some(reason)` to route the
    /// drop to the 12-step rebuild, `None` if the column drops cleanly per-op.
    ///
    /// Sources, in fail-closed order:
    ///   1. INDEX key columns (`IndexSnapshot::columns`) — a column in any index.
    ///   2. CONSTRAINT definitions (`ConstraintSnapshot::definition`) — the synthesised
    ///      FK / UNIQUE / PK bodies carry the member column names verbatim.
    ///   3. The verbatim `CREATE TABLE` text (`TableSnapshot::stored_create_sql`),
    ///      the ONLY source for CHECK predicates, generated-column expressions, and
    ///      partial-index predicates — none of which the `SQLite` drift PRAGMAs surface
    ///      into the structured snapshot. We do a CONSERVATIVE whole-word scan: if the
    ///      column name appears as a word ANYWHERE in the stored DDL beyond its own
    ///      definition, we rebuild. This can over-trigger a rebuild (a comment / a
    ///      coincidental match) but NEVER under-triggers — a rebuild is always
    ///      data-preserving, while a wrong native DROP COLUMN aborts the migration.
    fn sqlite_dropped_column_dependent(
        table: &str,
        lc: &ColumnSnapshot,
        lt: &TableSnapshot,
    ) -> Option<String> {
        let col = lc.name.as_str();

        // (1) Any index over this column.
        for idx in &lt.indexes {
            if idx.columns.iter().any(|c| c == col) {
                return Some(format!(
                    "drop column {table}.{col} referenced by index {}",
                    idx.name
                ));
            }
        }

        // (2) Any constraint whose definition names this column (FK / UNIQUE / PK).
        for c in &lt.constraints {
            if word_present_ci(&c.definition, col) {
                return Some(format!(
                    "drop column {table}.{col} referenced by constraint {} ({})",
                    c.name, c.kind
                ));
            }
        }

        // (3) The verbatim CREATE text — the only source for CHECK / generated /
        //     partial-index references. We scan the WHOLE statement (conservative:
        //     over-trigger acceptable, under-trigger never), as a whole word so a
        //     substring of another identifier does not false-match.
        if let Some(sql) = lt.stored_create_sql.as_deref() {
            // Strip this column's OWN definition clause is unnecessary for
            // correctness (a rebuild is always safe); a hit anywhere routes to the
            // rebuild. The column's own clause naturally matches, but the per-op
            // path is only taken when NO dependent exists — and a column always
            // appears in its own clause — so we must look for a SECOND occurrence
            // (a reference beyond the bare declaration) to avoid rebuilding EVERY
            // drop. Count whole-word occurrences; >1 means a reference exists.
            if word_count_ci(sql, col) > 1 {
                return Some(format!(
                    "drop column {table}.{col} referenced by a CHECK / generated / \
                     partial-index expression in the stored table DDL"
                ));
            }
        }

        None
    }
}

impl StoredDdl for SqliteStoredDdl {
    fn create_body_bounds(&self, sql: &str) -> Option<(usize, usize)> {
        sqlite_create_body_bounds(sql)
    }

    fn table_clauses<'a>(&self, body: &'a str) -> Option<Vec<&'a str>> {
        sqlite_table_clauses(body)
    }

    fn ddl_word(&self, sql: &str, cursor: &mut usize) -> Option<String> {
        sqlite_ddl_word(sql, cursor)
    }

    fn first_ddl_word_is_quoted(&self, sql: &str) -> bool {
        sqlite_first_ddl_word_is_quoted(sql)
    }

    fn generated_columns(&self, create_sql: &str) -> BTreeSet<String> {
        sqlite_generated_columns(create_sql)
    }

    fn rewrite_stored_primary_key(
        &self,
        table: &str,
        stored: &str,
        target_columns: Option<&[String]>,
        materialize_not_null: Option<&str>,
        backend: &dyn SchemaRenderer,
    ) -> Result<String, DeclarativeError> {
        rewrite_sqlite_stored_primary_key(
            table,
            stored,
            target_columns,
            materialize_not_null,
            backend,
        )
    }

    fn create_is_without_rowid(&self, create_sql: &str) -> bool {
        sqlite_create_is_without_rowid(create_sql)
    }

    fn inline_primary_key_is_desc(&self, create_sql: &str, column: &str) -> bool {
        sqlite_inline_primary_key_is_desc(create_sql, column)
    }

    fn rewrite_stored_foreign_keys(
        &self,
        table: &str,
        stored: &str,
        live: &TableSnapshot,
        desired: &TableSnapshot,
        backend: &dyn SchemaRenderer,
    ) -> Result<String, DeclarativeError> {
        rewrite_sqlite_stored_foreign_keys(table, stored, live, desired, backend)
    }

    fn virtual_table_module(&self, sql: &str) -> Option<String> {
        virtual_table_module(sql)
    }

    fn dropped_column_dependent(
        &self,
        table: &str,
        column: &ColumnSnapshot,
        live: &TableSnapshot,
    ) -> Option<String> {
        Self::sqlite_dropped_column_dependent(table, column, live)
    }
}

#[cfg(test)]
mod h1_word_scan_tests {
    use super::{word_count_ci, word_present_ci};

    // the whole-word, case-insensitive column scan used by the DROP-COLUMN
    // rebuild router. A column must NOT match as a substring of a larger identifier,
    // a quoted reference must match, and the case must be folded.
    #[test]
    fn word_scan_is_whole_word_and_case_insensitive() {
        // Whole-word: `id` does not match `idx` / `user_id` / `idle`.
        assert!(!word_present_ci("CREATE INDEX idx ON t (user_id)", "id"));
        assert_eq!(word_count_ci("idx idle paranoid", "id"), 0);
        // A bare and a quoted reference both match.
        assert!(word_present_ci("CHECK (age > 0)", "age"));
        assert!(word_present_ci("CHECK (\"age\" > 0)", "age"));
        // Case-insensitive.
        assert!(word_present_ci("CHECK (AGE > 0)", "age"));
        // Count counts each whole-word occurrence (declaration + CHECK reference).
        assert_eq!(
            word_count_ci("\"points\" INTEGER, CHECK (points >= 0)", "points"),
            2
        );
        // A column appearing ONLY in its own declaration counts once (drops natively).
        assert_eq!(word_count_ci("\"solo\" TEXT", "solo"), 1);
        // Empty needle counts zero (never matches).
        assert_eq!(word_count_ci("anything", ""), 0);
    }
}
