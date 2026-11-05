//! Pins the generated SQL per dialect and the CSDL of the test model.

mod common;

use odatabase::edm::{Model, to_csdl_xml};
use odatabase_sql::{Claims, Dialect, ExpandLimits, Page, QueryContext, ReadRequest, read_sql};
use odatabase_uri::{Target, parse};
use std::collections::HashMap;

fn model() -> Model {
    let pool = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(common::sqlite_pool(common::SCHEMA));
    common::builder().build(pool).unwrap().model().clone()
}

fn sql(model: &Model, dialect: Dialect, path: &str, query: &str) -> String {
    let uri = parse(model, path, query).unwrap();
    let (Target::EntityCollection(t) | Target::Entity(t)) = uri.target() else {
        panic!("{path} is not an entity read");
    };
    let claims = Claims::default();
    let extensions = HashMap::new();
    let ctx = QueryContext {
        model,
        dialect,
        claims: &claims,
        language: "en",
        extensions: &extensions,
    };
    read_sql(
        &ctx,
        ReadRequest {
            segments: &uri.path,
            type_name: t,
            options: &uri.options,
            page: Page {
                top: uri.options.top,
                skip: uri.options.skip,
            },
            limits: ExpandLimits::default(),
        },
    )
    .unwrap()
}

const QUERIES: [(&str, &str, &str); 8] = [
    ("key", "Roles(PartnerId='1',Code=2)", "$select=Label"),
    ("eq_ne", "Roles", "$filter=Label eq 'a' or Label ne 'b'"),
    (
        "functions",
        "Roles",
        "$filter=contains(Label,'a') and endswith(Label,'b')",
    ),
    ("to_one_path", "Roles", "$filter=Partner/Name eq 'x'"),
    (
        "lambda",
        "Projects",
        "$filter=Partners/all(p: p/Rating gt 1)",
    ),
    ("navigation", "Projects(1)/Partners", "$select=Id"),
    (
        "count_order",
        "Projects",
        "$orderby=Partners/$count desc,Title&$top=5&$skip=2",
    ),
    (
        "timestamps",
        "Divisions",
        "$filter=Population gt 1 and not (Code in ('a','b'))",
    ),
];

#[test]
fn sql_per_dialect() {
    let m = model();
    for (name, path, query) in QUERIES {
        insta::assert_snapshot!(
            format!("{name}_postgres"),
            sql(&m, Dialect::Postgres, path, query)
        );
        insta::assert_snapshot!(
            format!("{name}_sqlite"),
            sql(&m, Dialect::Sqlite, path, query)
        );
    }
}

#[test]
fn metadata_document() {
    insta::assert_snapshot!("metadata", to_csdl_xml(&model()));
}
