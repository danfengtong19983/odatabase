//! Access control on paths that bypassed it: joined inheritance under tenancy, hidden derived
//! types read through their base, hidden functions inside expressions, `outerjoin` through a
//! join table, and `%` and `_` in wildcard claims.
mod common;

use axum::Extension;
use axum::http::Method;
use common::App;
use odatabase::edm::Value;
use odatabase::{ClaimValue, Claims, Database, Nav, ODataEntity, Service, ServiceBuilder};
use serde_json::json;

const SCHEMA: &str = "
CREATE TABLE asset (
  tenant_id TEXT NOT NULL, id INTEGER NOT NULL, name TEXT NOT NULL, PRIMARY KEY (tenant_id, id)
);
CREATE TABLE machine (
  tenant_id TEXT NOT NULL, asset_id INTEGER NOT NULL, power INTEGER,
  PRIMARY KEY (tenant_id, asset_id)
);
INSERT INTO asset (tenant_id, id, name) VALUES ('a', 1, 'a-plain'), ('b', 1, 'b-machine');
INSERT INTO machine (tenant_id, asset_id, power) VALUES ('b', 1, 9000);

CREATE TABLE animal (
  id INTEGER PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL, venom TEXT, breed TEXT
);
INSERT INTO animal (id, kind, name, venom, breed) VALUES
  (1, 'D', 'Rex', NULL, 'collie'), (2, 'S', 'Cobra', 'neurotoxin', NULL), (3, 'A', 'Plain', NULL, NULL);

CREATE TABLE crew (id INTEGER PRIMARY KEY);
CREATE TABLE doc (id INTEGER PRIMARY KEY, level INTEGER NOT NULL);
CREATE TABLE crew_doc (crew_id INTEGER NOT NULL, doc_id INTEGER NOT NULL, PRIMARY KEY (crew_id, doc_id));
INSERT INTO crew (id) VALUES (1), (2);
INSERT INTO doc (id, level) VALUES (1, 1), (2, 2), (3, 3);
INSERT INTO crew_doc (crew_id, doc_id) VALUES (1, 1), (1, 2), (1, 3);

CREATE TABLE site (id INTEGER PRIMARY KEY, region TEXT NOT NULL);
INSERT INTO site (id, region) VALUES (1, 'eu_west'), (2, 'eu-west'), (3, 'euXwest'), (4, 'us_east');
";

#[derive(ODataEntity)]
#[odata(table = "asset")]
pub struct Asset {
    #[odata(tenant)]
    pub tenant_id: String,
    #[odata(key)]
    pub id: i32,
    pub name: String,
}

#[derive(ODataEntity)]
#[odata(
    table = "machine",
    base = Asset,
    joined(id = "asset_id", tenant_column = "tenant_id")
)]
pub struct Machine {
    pub power: Option<i32>,
}

/// The unsafe declaration: no tenant column on the joined table.
#[derive(ODataEntity)]
#[odata(table = "machine", base = Asset, joined(id = "asset_id"))]
pub struct LooseMachine {
    pub power: Option<i32>,
}

#[derive(ODataEntity)]
#[odata(table = "animal")]
pub struct Animal {
    #[odata(key)]
    pub id: i32,
    pub name: String,
}

#[derive(ODataEntity)]
#[odata(base = Animal, discriminator(column = "kind", value = "S"), visible_for("admin"))]
pub struct Snake {
    pub venom: Option<String>,
}

#[derive(ODataEntity)]
#[odata(base = Animal, discriminator(column = "kind", value = "D"))]
pub struct Dog {
    pub breed: Option<String>,
}

#[derive(ODataEntity)]
#[odata(table = "crew")]
pub struct Crew {
    #[odata(key)]
    pub id: i32,
    #[odata(has_many(
        target = Doc,
        join_table(table = "crew_doc", source(id = "crew_id"), target(doc_id = "id"))
    ))]
    pub docs: Nav,
}

#[derive(ODataEntity)]
#[odata(table = "doc")]
pub struct Doc {
    #[odata(key)]
    pub id: i32,
    #[odata(protected(claim = "level"))]
    pub level: i32,
}

#[derive(ODataEntity)]
#[odata(table = "site")]
pub struct Site {
    #[odata(key)]
    pub id: i32,
    #[odata(protected(claim = "region", wildcards))]
    pub region: String,
}

fn builder() -> ServiceBuilder {
    Service::builder("P")
        .entity::<Asset>()
        .entity::<Machine>()
        .entity::<Animal>()
        .entity::<Snake>()
        .entity::<Dog>()
        .entity::<Crew>()
        .entity::<Doc>()
        .entity::<Site>()
        .function("Hidden", |f| {
            f.param::<i32>("x")
                .returns::<i32>()
                .composable()
                .database_function("abs")
                .visible_for("admin")
        })
}

fn string(s: &str) -> ClaimValue {
    ClaimValue::Equals(Value::String(s.to_owned()))
}

/// A caller of `tenant` without groups, at level 1, in region `eu_west`.
fn user(tenant: &str) -> Claims {
    Claims::default()
        .with("tenant", string(tenant))
        .with("level", ClaimValue::Equals(Value::Int32(1)))
        .with("region", string("eu_west"))
}

fn as_user(app: &App, claims: Claims) -> App {
    App {
        router: app.router.clone().layer(Extension(claims)),
    }
}

struct Callers {
    a: App,
    b: App,
    admin: App,
    plain: App,
}

fn callers(service: Service) -> Callers {
    let plain = App::new(service);
    Callers {
        a: as_user(&plain, user("a")),
        b: as_user(&plain, user("b")),
        admin: as_user(&plain, user("a").with_group("admin")),
        plain,
    }
}

async fn send(app: &App, method: Method, uri: &str, body: serde_json::Value) -> common::Response {
    app.request(
        method,
        uri,
        &[("content-type", "application/json")],
        &body.to_string(),
    )
    .await
}

/// Finding 1: a key is only unique within a tenant, so the joined table is matched on both.
async fn joined_inheritance_keeps_tenants_apart(c: &Callers) {
    let doc = c.a.ok("Assets").await;
    assert_eq!(doc["value"], json!([{"Id": 1, "Name": "a-plain"}]), "{doc}");
    assert_eq!(c.a.ok("Assets/P.Machine").await["value"], json!([]));
    assert_eq!(
        c.a.ok("Assets?$filter=isof(P.Machine)").await["value"],
        json!([])
    );
    assert_eq!(
        c.a.ok("Assets?$filter=P.Machine/Power gt 0").await["value"],
        json!([])
    );
    let r = send(
        &c.a,
        Method::PATCH,
        "Assets(1)/P.Machine",
        json!({"Power": 1}),
    )
    .await;
    assert_eq!(r.status, 404, "{}", r.text());
    assert_eq!(
        c.b.ok("Assets(1)").await["Power"],
        9000,
        "b's row is untouched"
    );

    // Updates of b's machine stay in b.
    let r = send(
        &c.b,
        Method::PATCH,
        "Assets(1)/P.Machine",
        json!({"Power": 9001}),
    )
    .await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert_eq!(c.b.ok("Assets(1)").await["Power"], 9001);
    assert_eq!(c.a.ok("Assets(1)").await.get("Power"), None);

    // Inserts stamp the tenant on the joined table, and deletes are scoped by it.
    for (app, power) in [(&c.a, 7), (&c.b, 50)] {
        let r = send(
            app,
            Method::POST,
            "Assets",
            json!({"@odata.type": "#P.Machine", "Id": 5, "Name": "m", "Power": power}),
        )
        .await;
        assert_eq!(r.status, 201, "{}", r.text());
    }
    assert_eq!(c.a.ok("Assets(5)").await["Power"], 7);
    assert_eq!(c.b.ok("Assets(5)").await["Power"], 50);
    let r = send(&c.a, Method::DELETE, "Assets(5)", json!({})).await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert_eq!(c.a.status("Assets(5)").await, 404);
    assert_eq!(
        c.b.ok("Assets(5)").await["Power"],
        50,
        "b's machine 5 survives"
    );
    assert_eq!(c.b.ok("Assets/P.Machine/$count").await, 2);
}

/// Finding 2: the rows and properties of a hidden derived type stay hidden through its base.
async fn hidden_derived_types(c: &Callers) {
    let doc = c.a.ok("Animals?$orderby=Id").await;
    let ids: Vec<_> = doc["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["Id"].clone())
        .collect();
    assert_eq!(ids, [json!(1), json!(3)], "no snake: {doc}");
    assert!(!doc.to_string().contains("Venom"), "{doc}");
    assert_eq!(c.a.get("Animals/$count").await.text(), "2");
    assert_eq!(c.a.status("Animals(2)").await, 404);
    assert_eq!(
        c.a.ok("Animals?$apply=aggregate($count as N)").await["value"][0]["N"],
        2
    );
    assert_eq!(
        c.a.ok("Animals?$filter=Name eq 'Cobra'").await["value"],
        json!([])
    );
    for uri in [
        "Animals?$filter=P.Snake/Venom eq 'neurotoxin'",
        "Animals?$select=P.Snake/Venom",
        "Animals?$orderby=P.Snake/Venom",
        "Animals/P.Snake",
        "Animals(2)/P.Snake/Venom",
    ] {
        let status = c.a.status(uri).await;
        assert!(status == 403 || status == 400, "{uri}: {status}");
    }
    // Visible derived types are unaffected.
    assert_eq!(c.a.ok("Animals(1)").await["Breed"], "collie");
    assert_eq!(c.a.ok("Animals/P.Dog/$count").await, 1);

    let doc = c.admin.ok("Animals(2)").await;
    assert_eq!(doc["Venom"], "neurotoxin", "{doc}");
    assert_eq!(c.admin.get("Animals/$count").await.text(), "3");
    assert_eq!(
        c.admin
            .ok("Animals?$filter=P.Snake/Venom eq 'neurotoxin'")
            .await["value"][0]["Id"],
        2
    );
}

/// Finding 3: a function hidden from the caller can't run inside an expression either.
async fn hidden_functions_in_expressions(c: &Callers) {
    assert_eq!(c.a.status("Hidden(x=-1)").await, 403);
    for uri in [
        "Animals?$filter=P.Hidden(x=Id) eq 1",
        "Animals?$orderby=P.Hidden(x=Id)",
        "Animals?$compute=P.Hidden(x=Id) as H",
    ] {
        assert_eq!(c.a.status(uri).await, 403, "{uri}");
    }
    assert_eq!(c.admin.ok("Hidden(x=-1)").await["value"], 1);
    let doc = c.admin.ok("Animals?$filter=P.Hidden(x=Id) eq 1").await;
    assert_eq!(doc["value"][0]["Id"], 1);
}

/// Finding 4: `outerjoin` through a join table counts only visible related rows.
async fn outerjoin_through_a_join_table(c: &Callers) {
    let count =
        |uri: &'static str| async move { c.a.ok(uri).await["value"][0]["N"].as_i64().unwrap() };
    assert_eq!(
        count("Crews?$apply=filter(Id eq 1)/join(Docs as D)/aggregate($count as N)").await,
        1
    );
    assert_eq!(
        count("Crews?$apply=filter(Id eq 1)/outerjoin(Docs as D)/aggregate($count as N)").await,
        1,
        "links to hidden documents are not rows"
    );
    assert_eq!(
        count("Crews?$apply=outerjoin(Docs as D)/aggregate($count as N)").await,
        2,
        "one visible document, and crew 2 kept without documents"
    );
    let doc = c.a.ok("Crews?$expand=Docs/$count&$orderby=Id").await;
    assert_eq!(doc["value"][0]["Docs@odata.count"], 1, "{doc}");
}

/// Finding 5: only `*` and `+` are wildcards; `%` and `_` in a claim match themselves.
async fn wildcard_claims_escape_like(c: &Callers) {
    let ids = |claims: Claims| {
        let app = as_user(&c.plain, claims);
        async move {
            app.column("Sites?$orderby=Id", "Id")
                .await
                .into_iter()
                .map(|v| v.as_i64().unwrap())
                .collect::<Vec<_>>()
        }
    };
    let with = |region: &str| Claims::default().with("region", string(region));
    assert_eq!(
        ids(with("eu_west")).await,
        [1],
        "an underscore is not a wildcard"
    );
    assert_eq!(
        ids(with("eu%")).await,
        Vec::<i64>::new(),
        "neither is a percent sign"
    );
    assert_eq!(
        ids(with("eu+west")).await,
        [1, 2, 3],
        "+ is any one character"
    );
    assert_eq!(
        ids(with("*_*")).await,
        [1, 4],
        "* with a literal underscore"
    );
    assert_eq!(ids(with("*")).await, [1, 2, 3, 4]);
}

async fn all(db: Database) {
    let c = callers(builder().build(db).unwrap());
    joined_inheritance_keeps_tenants_apart(&c).await;
    hidden_derived_types(&c).await;
    hidden_functions_in_expressions(&c).await;
    outerjoin_through_a_join_table(&c).await;
    wildcard_claims_escape_like(&c).await;
}

#[tokio::test]
async fn on_sqlite() {
    all(common::sqlite_pool(SCHEMA).await.into()).await;
}

#[tokio::test]
async fn on_postgres() {
    if !common::postgres_can_run() {
        return;
    }
    let server = common::PostgresServer::start().await;
    all(server.pool(SCHEMA).await.into()).await;
}

#[tokio::test]
async fn a_joined_type_with_a_tenant_needs_a_tenant_column() {
    let db: Database = common::sqlite_pool(SCHEMA).await.into();
    let err = Service::builder("P")
        .entity::<Asset>()
        .entity::<LooseMachine>()
        .build(db)
        .err()
        .expect("build refuses the model");
    assert!(err.to_string().contains("tenant_column"), "{err}");
}
