use super::*;

#[derive(Debug, PartialEq)]
struct ExternalKey(String);

fn decode_key(value: String) -> Result<ExternalKey, DbError> {
    if value.starts_with("key:") {
        Ok(ExternalKey(value))
    } else {
        Err(DbError::validation(
            "invalid_key",
            "expected an external key",
        ))
    }
}

fn encode_key(value: &ExternalKey) -> Result<&str, DbError> {
    if value.0.starts_with("key:") {
        Ok(&value.0)
    } else {
        Err(DbError::validation(
            "invalid_key",
            "expected an external key",
        ))
    }
}

fn encode_owned_key(value: ExternalKey) -> Result<String, DbError> {
    encode_key(&value)?;
    Ok(value.0)
}

fn encode_optional_key(value: Option<&ExternalKey>) -> Result<Option<&str>, DbError> {
    value.map(encode_key).transpose()
}

fn decode_optional_key(value: Option<String>) -> Result<Option<ExternalKey>, DbError> {
    value.map(decode_key).transpose()
}

#[derive(Debug, FromRow, Insertable)]
#[orm(entity = posts)]
struct OwnedConvertedPost {
    #[orm(decode_with = decode_key, encode_with = encode_owned_key)]
    title: ExternalKey,
}

#[derive(Insertable)]
#[orm(entity = posts)]
struct NewDefaultedPost<'a> {
    title: &'a str,
    #[orm(default, encode_with = encode_optional_key)]
    nickname: Defaulted<Option<&'a ExternalKey>>,
}

#[derive(Debug, FromRow)]
#[orm(entity = posts)]
struct NullablePost {
    #[orm(decode_with = decode_optional_key)]
    nickname: Option<ExternalKey>,
}

#[derive(Debug, FromRow)]
#[orm(entity = posts)]
struct ConvertedPost {
    #[orm(column = "title", decode_with = decode_key)]
    key: ExternalKey,
}

#[derive(Insertable)]
#[orm(entity = posts)]
struct NewConvertedPost<'a> {
    #[orm(encode_with = encode_key)]
    title: &'a ExternalKey,
}

#[derive(Changeset)]
#[orm(entity = posts)]
struct ChangeConvertedPost<'a> {
    #[orm(encode_with = encode_key)]
    title: Change<&'a ExternalKey>,
    counter: Change<i64>,
}

#[test]
fn conversion_hooks_keep_domain_types_outside_native_codecs() {
    let key = ExternalKey("key:chosen".into());
    let record = NewConvertedPost { title: &key }.into_record().unwrap();
    assert_eq!(record["title"], Value::String(key.0.clone()));
    let decoded = ConvertedPost::from_row(Row::new(record)).unwrap();
    assert_eq!(decoded.key, key);
    assert_eq!(ConvertedPost::COLUMNS, &["title"]);
    let record = OwnedConvertedPost { title: key }.into_record().unwrap();
    assert_eq!(
        OwnedConvertedPost::from_row(Row::new(record))
            .unwrap()
            .title
            .0,
        "key:chosen"
    );
}

#[test]
fn conversions_preserve_defaults_and_explicit_nulls() {
    let default = NewDefaultedPost {
        title: "ordinary",
        nickname: Defaulted::Default,
    }
    .into_record()
    .unwrap();
    assert!(!default.contains_key("nickname"));
    let null = NewDefaultedPost {
        title: "ordinary",
        nickname: Defaulted::Value(None),
    }
    .into_record()
    .unwrap();
    assert_eq!(null["nickname"], Value::Null);
    assert_eq!(
        NullablePost::from_row(Row::new(null)).unwrap().nickname,
        None
    );
    let key = ExternalKey("key:nickname".into());
    let record = NewDefaultedPost {
        title: "ordinary",
        nickname: Defaulted::Value(Some(&key)),
    }
    .into_record()
    .unwrap();
    assert_eq!(
        NullablePost::from_row(Row::new(record)).unwrap().nickname,
        Some(key)
    );
}

#[test]
fn conversion_failures_keep_the_column_context() {
    let error = ConvertedPost::from_row(Row::new(Record::from([(
        "title".into(),
        Value::from("invalid"),
    )])))
    .unwrap_err();
    assert!(error.to_string().contains("posts.title"), "{error}");
    assert!(
        error.to_string().contains("expected an external key"),
        "{error}"
    );
    let invalid = ExternalKey("invalid".into());
    let error = NewConvertedPost { title: &invalid }
        .into_record()
        .unwrap_err();
    assert!(error.to_string().contains("posts.title"), "{error}");
    let wrong_storage = Record::from([("title".into(), Value::from(42))]);
    assert!(ConvertedPost::from_row(Row::new(wrong_storage)).is_err());
}

#[compio::test]
async fn converted_changes_preserve_keep_and_set() {
    let (db, _directory) = database().await;
    let first = ExternalKey("key:first".into());
    let second = ExternalKey("key:second".into());
    let posts = db.entity::<posts::Entity>().unwrap();
    let created: Post = posts
        .insert(NewConvertedPost { title: &first })
        .await
        .unwrap();
    let unchanged: Option<ConvertedPost> = posts
        .update(
            posts::id.eq(created.id.as_str()).unwrap(),
            ChangeConvertedPost {
                title: Change::Keep,
                counter: Change::Set(8),
            },
        )
        .await
        .unwrap();
    assert_eq!(unchanged.unwrap().key, first);
    let changed: Option<ConvertedPost> = posts
        .update(
            posts::id.eq(created.id.as_str()).unwrap(),
            ChangeConvertedPost {
                title: Change::Set(&second),
                counter: Change::Keep,
            },
        )
        .await
        .unwrap();
    assert_eq!(changed.unwrap().key, second);
}
