use zeroship_data_orm::orm::*;

schema! {
    pub models {
        authors {
            #[orm(primary_key)]
            id: Text,
            name: Text,
        }
        posts {
            #[orm(primary_key)]
            id: Text,
            title: Text,
            #[orm(references(authors::id), relation(author))]
            author_id: Nullable<Text>,
            #[orm(assign(on = insert, by = now))]
            created_at: Timestamp,
        }
    }
}

#[derive(Insertable)]
#[orm(entity = models::posts)]
struct NewPost<'a> {
    id: &'a str,
    title: &'a str,
    author_id: Option<&'a str>,
}

#[derive(FromRow)]
#[orm(entity = models::posts)]
struct Title {
    title: String,
}

fn main() {
    let _schema = models::schema();
    let record = NewPost {
        id: "chosen-id",
        title: "Native Rust",
        author_id: None,
    }
    .into_record()
    .unwrap();
    assert_eq!(record["id"].as_str(), Some("chosen-id"));
    assert!(record["author_id"].is_null());
    assert!(!record.contains_key("created_at"));
    assert_eq!(Title::COLUMNS, &["title"]);
}
