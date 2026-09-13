use super::*;

#[test]
fn native_declarations_accept_typed_columns_and_named_edges() {
    let declaration = syn::parse_str::<Input>(
        r#"pub models {
            authors {
                #[orm(primary_key)] id: Text,
                name: Text,
            }
            posts {
                #[orm(primary_key, assign(on = insert, by = typed_id))] id: Text,
                #[orm(references(authors::id), relation(author))] author_id: Nullable<Text>,
                #[orm(assign(on = insert, by = now))] created_at: Timestamp,
                #[orm(default = 1, assign(on = write, by = increment(1)), concurrency)] revision: Integer,
                #[orm(default = ["draft"])] labels: Array<Text>,
            }
        }"#,
    );
    assert!(declaration.is_ok(), "{}", declaration.err().unwrap());
}

#[test]
fn file_descriptor_form_is_not_a_schema_declaration() {
    assert!(syn::parse_str::<Input>(r#"pub models = "schema.runtime.json""#).is_err());
}

#[test]
fn native_declarations_retain_constraints_and_protection_storage() {
    let declaration = syn::parse_str::<Input>(
        r#"pub models {
        entries {
            #[orm(primary_key, max_length = 255, char_len = 12, pattern = "[a-z]+", id_prefix = "entry")]
            id: Text,
            #[orm(precision = 30, scale = 2, min = -1.5, max = 100, default = decimal("12.50"))]
            balance: Number,
            #[orm(enum_values = ["draft", "published"], case_sensitive = false, format = "email", aggregateable = false)]
            status: Text,
            #[orm(identity = { always: true }, generated = { expression: "counter" })]
            counter: BigInt,
            #[orm(encrypted, mask(kind = "full", classification = "pii"), raw_column = "sealed", raw_filterable = false, raw_sortable = false, raw_projectable = false)]
            secret: Nullable<Text>,
            #[orm(references(entries::id), on_delete = "cascade", on_update = "restrict", deferrable)]
            parent_id: Nullable<Text>,
            #[orm(vector_dims = 3, vector_metric = cosine)]
            embedding: Nullable<Vector>,
        }
    }"#,
    );
    assert!(declaration.is_ok(), "{}", declaration.err().unwrap());
}

#[test]
fn native_declarations_reject_invalid_contracts() {
    let cases = [
        ("pub models {}", "schema requires a collection"),
        ("pub models { entries { title: Text } }", "requires an 'id' primary key"),
        ("pub models { entries { id: Text } }", "must be declared as its primary key"),
        ("pub models { entries { #[orm(primary_key)] id: Nullable<Text> } }", "must be required and non-null"),
        ("pub models { entries { #[orm(primary_key)] id: Boolean } }", "must use text or integer storage"),
        ("pub models { entries { #[orm(primary_key, encrypted)] id: Text } }", "cannot be encrypted or masked"),
        ("pub models { entries { #[orm(primary_key, assign(on = write, by = now))] id: Text } }", "can only be assigned on insertion"),
        ("pub models { entries { #[orm(primary_key)] id: Text, #[orm(primary_key)] key: Text } }", "sole primary key"),
        ("pub models { entries { #[orm(primary_key)] id: Text, id: Integer } }", "duplicate column name"),
        ("pub models { entries { #[orm(primary_key)] id: Text } entries { #[orm(primary_key)] id: Text } }", "duplicate collection name"),
        ("pub models { entries { #[orm(primary_key, primary_key)] id: Text } }", "duplicate column option"),
        ("pub models { entries { #[orm(primary_key)] id: Text, #[orm(relation(parent))] parent_id: Text } }", "requires a reference"),
        ("pub models { entries { #[orm(primary_key)] id: Text, #[orm(references(absent::id))] parent_id: Text } }", "target collection does not exist"),
        ("pub models { entries { #[orm(primary_key)] id: Text, #[orm(references(entries::absent))] parent_id: Text } }", "target column does not exist"),
        ("pub models { entries { #[orm(primary_key)] id: Text, #[orm(references(entries::id), relation(id))] parent_id: Text } }", "collides"),
        ("pub models { entries { #[orm(primary_key)] id: Text, #[orm(references(entries::id), relation(_meta))] parent_id: Text } }", "reserved relation name"),
        ("pub models { entries { #[orm(primary_key)] id: Text, #[orm(readable = 1)] title: Text } }", "boolean literal"),
        ("pub models { entries { #[orm(primary_key)] id: Text, #[orm(unknown)] title: Text } }", "unsupported ORM column option"),
        ("pub models { entries { #[orm(primary_key)] id: Text, title: Unknown } }", "unsupported logical column type"),
    ];
    for (source, expected) in cases {
        let error = syn::parse_str::<Input>(source).err().expect(source);
        assert!(error.to_string().contains(expected), "{source}: {error}");
    }
}

fn module<'a>(items: &'a [syn::Item], name: &str) -> &'a [syn::Item] {
    items
        .iter()
        .find_map(|item| match item {
            syn::Item::Mod(module) if module.ident == name => {
                module.content.as_ref().map(|(_, items)| items.as_slice())
            }
            _ => None,
        })
        .expect(name)
}

fn implemented_traits(items: &[syn::Item], column: &str) -> std::collections::BTreeSet<String> {
    items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Impl(implementation) => {
                let syn::Type::Path(ty) = &*implementation.self_ty else {
                    return None;
                };
                if !ty.path.is_ident(column) {
                    return None;
                }
                implementation
                    .trait_
                    .as_ref()
                    .map(|(_, path, _)| path.segments.last().unwrap().ident.to_string())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn native_expansion_preserves_column_capabilities_and_insert_requirements() {
    let source: Input = syn::parse_str(
        r#"pub models {
        entries {
            #[orm(primary_key, assign(on = insert, by = typed_id))] id: Text,
            title: Text,
            created_at: Text,
            version: Integer,
            optional: Nullable<Bytes>,
            #[orm(default = 7)] counter: BigInt,
            #[orm(projectable = false, filterable = false, writable = false)] hidden: Text,
            #[orm(assign(on = insert, by = now))] inserted: Timestamp,
            #[orm(generated = "upper(title)")] computed: Text,
            #[orm(encrypted, mask(kind = "full", classification = "pii"))] secret: Nullable<Text>,
        }
    }"#,
    )
    .unwrap();
    let orm = syn::parse_quote!(::zeroship_data_orm::orm);
    let file: syn::File = syn::parse2(generate(source, &orm).unwrap()).unwrap();
    let entries = module(module(&file.items, "models"), "entries");
    let columns = module(entries, "columns");
    let traits = |name, expected: &[&str]| {
        assert_eq!(
            implemented_traits(columns, name),
            expected.iter().map(|s| (*s).to_string()).collect()
        );
    };
    traits("id", &["Column", "FilterableColumn", "ReadableColumn"]);
    traits(
        "title",
        &[
            "Column",
            "FilterableColumn",
            "ReadableColumn",
            "WritableColumn",
            "UpdatableColumn",
        ],
    );
    traits(
        "created_at",
        &[
            "Column",
            "FilterableColumn",
            "ReadableColumn",
            "WritableColumn",
            "UpdatableColumn",
        ],
    );
    traits(
        "version",
        &[
            "Column",
            "FilterableColumn",
            "ReadableColumn",
            "WritableColumn",
            "UpdatableColumn",
        ],
    );
    traits(
        "optional",
        &[
            "Column",
            "FilterableColumn",
            "ReadableColumn",
            "WritableColumn",
            "UpdatableColumn",
            "DefaultableColumn",
        ],
    );
    traits(
        "counter",
        &[
            "Column",
            "FilterableColumn",
            "ReadableColumn",
            "WritableColumn",
            "UpdatableColumn",
            "DefaultableColumn",
        ],
    );
    traits("hidden", &["Column"]);
    traits(
        "inserted",
        &["Column", "FilterableColumn", "ReadableColumn"],
    );
    traits(
        "computed",
        &["Column", "FilterableColumn", "ReadableColumn"],
    );
    traits(
        "secret",
        &[
            "Column",
            "ReadableColumn",
            "WritableColumn",
            "UpdatableColumn",
            "DefaultableColumn",
        ],
    );
    let complete = entries
        .iter()
        .find_map(|item| match item {
            syn::Item::Impl(item)
                if item
                    .trait_
                    .as_ref()
                    .is_some_and(|(_, path, _)| path.is_ident("CompleteInsert")) =>
            {
                Some(item)
            }
            _ => None,
        })
        .unwrap();
    let required: Vec<_> = complete
        .generics
        .where_clause
        .as_ref()
        .unwrap()
        .predicates
        .iter()
        .filter_map(|predicate| {
            let syn::WherePredicate::Type(predicate) = predicate else {
                return None;
            };
            Some(&predicate.bounds)
        })
        .flatten()
        .filter_map(|bound| {
            let syn::TypeParamBound::Trait(bound) = bound else {
                return None;
            };
            let syn::PathArguments::AngleBracketed(arguments) =
                &bound.path.segments.last()?.arguments
            else {
                return None;
            };
            let syn::GenericArgument::Type(syn::Type::Path(ty)) = arguments.args.first()? else {
                return None;
            };
            Some(ty.path.segments.last()?.ident.to_string())
        })
        .collect();
    assert_eq!(required, ["title", "created_at", "version"]);
}

#[test]
fn native_expansion_emits_collection_metadata_and_relation_targets() {
    let source: Input = syn::parse_str(
        r"pub models {
        authors { #[orm(primary_key)] id: Text }
        posts {
            #[orm(primary_key)] id: Text,
            #[orm(references(authors::id), relation(author))] author_id: Nullable<Text>,
        }
    }",
    )
    .unwrap();
    let orm = syn::parse_quote!(::zeroship_data_orm::orm);
    let file: syn::File = syn::parse2(generate(source, &orm).unwrap()).unwrap();
    let models = module(&file.items, "models");
    let posts = module(models, "posts");
    let entity = posts
        .iter()
        .find_map(|item| match item {
            syn::Item::Impl(item)
                if item.trait_.as_ref().is_some_and(|(_, path, _)| {
                    path.segments.last().unwrap().ident == "Entity"
                }) =>
            {
                Some(item)
            }
            _ => None,
        })
        .unwrap();
    let schema = entity
        .items
        .iter()
        .find_map(|item| match item {
            syn::ImplItem::Fn(item) if item.sig.ident == "schema" => Some(item),
            _ => None,
        })
        .unwrap();
    let syn::ReturnType::Type(_, ty) = &schema.sig.output else {
        panic!("schema must return metadata")
    };
    let syn::Type::Reference(reference) = &**ty else {
        panic!("schema must borrow metadata")
    };
    assert_eq!(reference.lifetime.as_ref().unwrap().ident, "static");
    let syn::Type::Path(ty) = &*reference.elem else {
        panic!("schema must return a named metadata type")
    };
    assert_eq!(ty.path.segments.last().unwrap().ident, "CollectionSchema");
    assert!(models
        .iter()
        .any(|item| matches!(item, syn::Item::Fn(item) if item.sig.ident == "schema")));
    let relations = module(posts, "relations");
    assert_eq!(
        implemented_traits(relations, "author"),
        ["Relation".to_string()].into()
    );
    let target = relations
        .iter()
        .find_map(|item| match item {
            syn::Item::Impl(item) => item.items.iter().find_map(|item| match item {
                syn::ImplItem::Type(item) if item.ident == "Target" => Some(&item.ty),
                _ => None,
            }),
            _ => None,
        })
        .unwrap();
    let syn::Type::Path(target) = target else {
        panic!("relation target must name an entity")
    };
    assert_eq!(
        target
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect::<Vec<_>>(),
        ["super", "super", "authors", "Entity"]
    );
}

#[test]
fn native_literals_preserve_numbers_bytes_and_nested_values() {
    use literal::Literal;
    let cases = [
        "null",
        "true",
        "-9223372036854775808",
        "18446744073709551615",
        "1.25",
        "b\"bytes\"",
        "[1, null, {nested: true}]",
        "decimal(\"9007199254740993.01\")",
        "timestamp(123)",
    ];
    let orm = syn::parse_quote!(::zeroship_data_orm::orm);
    for source in cases {
        let value = syn::parse_str::<Literal>(source).unwrap();
        syn::parse2::<syn::Expr>(value.tokens(&orm)).unwrap();
    }
    assert!(matches!(
        syn::parse_str::<Literal>("18446744073709551615").unwrap(),
        Literal::Unsigned(u64::MAX)
    ));
    assert!(matches!(
        syn::parse_str::<Literal>("-9223372036854775808").unwrap(),
        Literal::Signed(i64::MIN)
    ));
    assert!(
        matches!(syn::parse_str::<Literal>("b\"bytes\"").unwrap(), Literal::Bytes(value) if value == b"bytes")
    );
    for source in [
        "18446744073709551616",
        "-9223372036854775809",
        "{same: 1, same: 2}",
        "timestamp(1.5)",
        "unknown()",
        "1e999",
    ] {
        assert!(syn::parse_str::<Literal>(source).is_err(), "{source}");
    }
}

#[test]
fn nested_declarations_preserve_object_and_union_metadata() {
    let source = r#"pub models {
        entries {
            #[orm(primary_key)] id: Text,
            #[orm(shape(dates: Array<CalendarDate>, occurred: Timestamp))] payload: Object,
            #[orm(discriminator = "kind", variants(
                { #[orm(literal_value = "sent")] kind: Literal, sent_at: Timestamp },
                { #[orm(literal_value = "failed")] kind: Literal, message: Text }
            ))] event: Union,
        }
    }"#;
    let declaration = syn::parse_str::<Input>(source);
    assert!(declaration.is_ok(), "{}", declaration.err().unwrap());
    let declaration = syn::parse_str::<Input>(source).unwrap();
    let columns = &declaration.collections[0].columns;
    let Property::Shape(shape) = &columns[1].properties[0] else {
        panic!("object must retain its field shape")
    };
    assert_eq!(shape[0].items, Some(Kind::CalendarDate));
    assert_eq!(shape[1].kind, Kind::Timestamp);
    let Property::Variants(variants) = &columns[2].properties[1] else {
        panic!("union must retain its variants")
    };
    assert_eq!(variants[0][0].kind, Kind::Literal);
    assert!(
        matches!(&variants[0][0].properties[0], Property::Value(name, literal::Literal::Text(value)) if name == "literal_value" && value == "sent")
    );
    assert_eq!(variants[1][1].kind, Kind::Text);
    let orm = syn::parse_quote!(::zeroship_data_orm::orm);
    for (column, expected) in [(&columns[1], "shape"), (&columns[2], "variants")] {
        let expr: syn::ExprStruct = syn::parse2(column_schema(column, &orm)).unwrap();
        assert!(expr
            .fields
            .iter()
            .any(|field| matches!(&field.member, syn::Member::Named(name) if name == expected)));
    }
    syn::parse2::<syn::File>(generate(declaration, &orm).unwrap()).unwrap();
    for unsupported in ["Enum", "Literal"] {
        assert!(syn::parse_str::<Input>(&format!("pub models {{ entries {{ #[orm(primary_key)] id: Text, unsupported: {unsupported} }} }}")).is_err());
    }
    for nested in [
        "shape(name: Text, name: Integer)",
        "variants({kind: Literal, kind: Text})",
    ] {
        let error = syn::parse_str::<Input>(&format!("pub models {{ entries {{ #[orm(primary_key)] id: Text, #[orm({nested})] payload: Json }} }}")).err().expect("duplicate nested field must be rejected");
        assert_eq!(error.to_string(), "duplicate column name");
    }
}
