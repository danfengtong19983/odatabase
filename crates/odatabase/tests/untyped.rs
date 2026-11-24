//! `Edm.Untyped` properties: `serde_json::Value` fields holding any JSON value, stored whole.

mod common;

use axum::http::{Method, StatusCode};
use common::App;
use odatabase::{Database, ODataEntity, Service};
use serde_json::{Value as Json, json};

/// `definition` is `jsonb` on Postgres, `notes` is `json` and `raw` is `text`, to show that all
/// three work. SQLite keeps JSON as text. `form` is an open type with an untyped `meta` column
/// beside its dynamic properties, the shape of a table with custom fields and metadata.
const SCHEMA: &str = "
CREATE TABLE rule (
  id INTEGER PRIMARY KEY, name TEXT NOT NULL,
  definition JSONB NOT NULL, tags JSONB, notes JSON, raw TEXT
);
CREATE TABLE form (
  id INTEGER PRIMARY KEY, title TEXT NOT NULL,
  custom JSONB NOT NULL DEFAULT '{}', meta JSONB NOT NULL DEFAULT '{}'
);
INSERT INTO rule (id, name, definition, tags, notes, raw) VALUES
  (1, 'limit', '{\"max\": 10, \"steps\": [{\"to\": \"open\"}]}', '[\"a\", \"b\"]', '\"plain\"', '{\"k\": 1}'),
  (2, 'flag', 'true', NULL, NULL, 'not json'),
  (3, 'count', '42', '[]', '{\"n\": null}', NULL);
INSERT INTO form (id, title, custom, meta) VALUES
  (1, 'Claim', '{\"claim_no\": \"RG-1\"}', '{\"source\": \"import\"}');
";

#[derive(ODataEntity)]
#[odata(table = "rule", rename_all = "snake_case")]
pub struct Rule {
    #[odata(key)]
    pub id: i32,
    pub name: String,
    pub definition: Json,
    pub tags: Option<Json>,
    pub notes: Option<Json>,
    pub raw: Option<Json>,
}

#[derive(ODataEntity)]
#[odata(
    table = "form",
    rename_all = "snake_case",
    open_type(column = "custom")
)]
pub struct Form {
    #[odata(key)]
    pub id: i32,
    pub title: String,
    pub meta: Json,
}

fn service(db: Database) -> Service {
    Service::builder("Untyped")
        .entity::<Rule>()
        .entity::<Form>()
        .build(db)
        .unwrap()
}

async fn write(app: &App, method: Method, uri: &str, body: Json) -> common::Response {
    app.request(
        method,
        uri,
        &[("content-type", "application/json")],
        &body.to_string(),
    )
    .await
}

async fn metadata(app: App) {
    let xml = app.get("$metadata").await.text();
    assert!(
        xml.contains("<Property Name=\"definition\" Type=\"Edm.Untyped\" Nullable=\"false\"/>"),
        "{xml}"
    );
    assert!(
        xml.contains("<Property Name=\"tags\" Type=\"Edm.Untyped\"/>"),
        "{xml}"
    );
    let doc = app.ok("$metadata?$format=json").await;
    assert_eq!(doc["Untyped"]["Rule"]["definition"]["$Type"], "Edm.Untyped");
}

async fn reads(app: App) {
    let r = app.ok("Rules(1)").await;
    assert_eq!(
        r["definition"],
        json!({"max": 10, "steps": [{"to": "open"}]})
    );
    assert_eq!(r["tags"], json!(["a", "b"]));
    assert_eq!(r["notes"], json!("plain"), "a json column");
    assert_eq!(r["raw"], json!({"k": 1}), "a text column");

    let r = app.ok("Rules(2)").await;
    assert_eq!(r["definition"], json!(true));
    assert_eq!(r["tags"], Json::Null);
    assert_eq!(
        r["raw"],
        json!("not json"),
        "text that isn't JSON reads as a string"
    );

    let r = app.ok("Rules(3)").await;
    assert_eq!(r["definition"], json!(42));
    assert_eq!(r["tags"], json!([]));
    assert_eq!(r["notes"], json!({"n": null}));

    let doc = app.ok("Rules?$select=definition&$orderby=id").await;
    assert_eq!(doc["value"][1]["definition"], json!(true));
    assert!(doc["value"][1].get("name").is_none(), "{doc}");

    let f = app.ok("Forms(1)").await;
    assert_eq!(f["claim_no"], "RG-1", "a dynamic property");
    assert_eq!(
        f["meta"],
        json!({"source": "import"}),
        "the untyped property"
    );
    assert!(f["meta"].get("claim_no").is_none());
}

async fn filters(app: App) {
    assert_eq!(
        app.ids("Rules?$filter=tags eq null").await.len(),
        1,
        "null checks work"
    );
    assert_eq!(app.ids("Rules?$filter=tags ne null").await.len(), 2);
    for refused in [
        "Rules?$filter=definition eq 42",
        "Rules?$filter=definition eq 'x'",
        "Rules?$orderby=definition",
        "Rules?$apply=groupby((definition))",
    ] {
        let status = app.status(refused).await;
        assert_eq!(status, 400, "{refused}");
    }
}

async fn writes(app: App) {
    let r = write(
        &app,
        Method::POST,
        "Rules",
        json!({"id": 9, "name": "new", "definition": {"a": [1, {"b": null}]},
               "tags": ["x"], "notes": {"n": 1}, "raw": "text"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let d = r.json();
    assert_eq!(d["definition"], json!({"a": [1, {"b": null}]}));
    assert_eq!(d["notes"], json!({"n": 1}));
    assert_eq!(d["raw"], json!("text"));

    let r = write(
        &app,
        Method::PATCH,
        "Rules(9)",
        json!({"definition": {"c": 3}, "tags": null}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    let d = app.ok("Rules(9)").await;
    assert_eq!(
        d["definition"],
        json!({"c": 3}),
        "replaced whole, not merged"
    );
    assert_eq!(d["tags"], Json::Null, "null clears the property");
    assert_eq!(d["notes"], json!({"n": 1}), "untouched");

    let r = write(&app, Method::PATCH, "Rules(9)", json!({"definition": null})).await;
    assert_eq!(
        r.status,
        StatusCode::BAD_REQUEST,
        "not nullable: {}",
        r.text()
    );

    let r = write(
        &app,
        Method::PATCH,
        "Forms(1)",
        json!({"meta": {"source": "api"}, "claim_no": "RG-2"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    let f = app.ok("Forms(1)").await;
    assert_eq!(f["meta"], json!({"source": "api"}));
    assert_eq!(f["claim_no"], "RG-2");
    assert_eq!(app.ids("Forms?$filter=claim_no eq 'RG-2'").await.len(), 1);
}

macro_rules! untyped_scenarios {
    ($($name:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() {
                let schema = super::SCHEMA.replace("JSONB", "TEXT").replace("JSON", "TEXT");
                let pool = common::sqlite_pool(&schema).await;
                super::$name(App::new(super::service(pool.into()))).await;
            }
        )*

        #[tokio::test]
        async fn on_postgres() {
            if !common::postgres_can_run() {
                return;
            }
            let server = common::PostgresServer::start().await;
            $(
                let pool = server.pool(super::SCHEMA).await;
                eprintln!("postgres: {}", stringify!($name));
                super::$name(App::new(super::service(pool.into()))).await;
            )*
        }
    };
}

mod scenario {
    use super::*;
    untyped_scenarios!(metadata, reads, filters, writes);
}
