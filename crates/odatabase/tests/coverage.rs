//! Features from the README that no other suite exercised: the remaining capability
//! annotations, service URLs behind a proxy, `Service::handle` without axum, `now()`,
//! `matchesPattern`, the configurable `$levels=max` limit, the body size limit, and the
//! statuses the README promises for what isn't implemented.
mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, header};
use common::{App, Division};
use odatabase::edm::{Annotation, AnnotationValue as A};
use odatabase::{
    Config, Database, ExpandLimits, ODataError, ODataRequest, OperationContext, OperationHandler,
    OperationResult, Service,
};
use serde_json::json;

fn record(fields: &[(&str, A)]) -> A {
    A::Record(
        fields
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
    )
}

struct Numbers;

#[async_trait::async_trait]
impl OperationHandler for Numbers {
    async fn call(&self, _: &OperationContext<'_>) -> Result<OperationResult, ODataError> {
        Ok(OperationResult::Values(vec![
            odatabase::edm::Value::Int32(2),
            odatabase::edm::Value::Int32(1),
        ]))
    }
}

fn service(db: Database) -> Service {
    common::builder()
        .annotate::<Division>(Annotation::new(
            "Capabilities.ExpandRestrictions",
            record(&[("Expandable", A::Bool(false))]),
        ))
        .annotate::<common::Ticket>(Annotation::new(
            "Capabilities.SkipSupported",
            A::Bool(false),
        ))
        .annotate::<common::Project>(Annotation::new(
            "Capabilities.InsertRestrictions",
            record(&[("Insertable", A::Bool(false))]),
        ))
        .annotate::<common::Project>(Annotation::new(
            "Capabilities.DeleteRestrictions",
            record(&[("Deletable", A::Bool(false))]),
        ))
        .annotate::<Division>(Annotation::new(
            "Capabilities.UpdateRestrictions",
            record(&[("Updatable", A::Bool(false))]),
        ))
        .function("Numbers", |f| f.returns::<Vec<i32>>().handler(Numbers))
        .build(db)
        .unwrap()
}

async fn expand_and_skip_restrictions(app: App) {
    assert_eq!(app.status("Divisions?$expand=Children").await, 400);
    assert_eq!(app.status("Divisions('EU')?$expand=Parent").await, 400);
    assert_eq!(app.status("Divisions").await, 200, "plain reads still work");
    assert_eq!(
        app.status("BusinessPartners?$expand=Roles").await,
        200,
        "other types can expand"
    );
    assert_eq!(app.status("Tickets?$skip=1").await, 501);
    assert_eq!(app.status("Tickets?$top=1").await, 200);
    let doc = app.ok("$metadata?$format=json").await;
    assert_eq!(
        doc["Test"]["Division"]["@Capabilities.ExpandRestrictions"]["Expandable"], false,
        "{}",
        doc["Test"]["Division"]
    );
    assert_eq!(doc["Test"]["Ticket"]["@Capabilities.SkipSupported"], false);
}

async fn now_in_filters(app: App) {
    let ids = app
        .ids("BusinessPartners?$filter=CreatedAt lt now()&$orderby=Id")
        .await;
    assert_eq!(ids, ["1", "2", "4"]);
    assert!(
        app.ids("BusinessPartners?$filter=CreatedAt gt now()")
            .await
            .is_empty()
    );
}

async fn unimplemented_features_answer_with_a_status(app: App) {
    for (query, status) in [
        ("BusinessPartners?$filter=hassubset(Comments,Comments)", 501),
        (
            "BusinessPartners?$apply=ancestors(H,Id,filter(Id eq '1'),keep start)",
            501,
        ),
        (
            "BusinessPartners?$apply=descendants(H,Id,filter(Id eq '1'),keep start)",
            501,
        ),
        (
            "BusinessPartners?$apply=aggregate(Rating with sum from Abc from Active as T)",
            501,
        ),
        (
            "BusinessPartners?$expand=Roles($apply=groupby((Partner/Id,Code));$expand=Partner)",
            501,
        ),
        ("Numbers()?$filter=$it gt 1", 501),
        ("Numbers()?$orderby=$it", 501),
    ] {
        assert_eq!(app.status(query).await, status, "{query}");
    }
    let r = app
        .request(
            Method::GET,
            "BusinessPartners",
            &[("accept", "application/xml")],
            "",
        )
        .await;
    assert_eq!(r.status, 406, "data formats other than JSON");
}

async fn delta_payloads_refuse_links(app: App) {
    let r = app
        .request(
            Method::PATCH,
            "BusinessPartners('1')",
            &[("content-type", "application/json")],
            &json!({"Roles@delta": [
                {"@odata.context": "#$link", "source": "BusinessPartners('1')",
                 "relationship": "Roles", "target": "Roles(PartnerId='2',Code=1)"}
            ]})
            .to_string(),
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.text());
    let r = app
        .request(
            Method::PATCH,
            "BusinessPartners('1')",
            &[("content-type", "application/json")],
            &json!({"Roles@delta": [
                {"@odata.context": "#$deletedLink", "source": "BusinessPartners('1')",
                 "relationship": "Roles", "target": "Roles(PartnerId='1',Code=1)"}
            ]})
            .to_string(),
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(
        app.column("BusinessPartners('1')/Roles?$orderby=Code", "Code")
            .await,
        [1, 2, 3],
        "nothing changed"
    );
}

async fn async_preference_is_answered_synchronously(app: App) {
    // The protocol lets a service ignore `respond-async`, and this one does.
    let r = app
        .request(
            Method::GET,
            "BusinessPartners('1')",
            &[("prefer", "respond-async")],
            "",
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.header("preference-applied").is_none());
    assert_eq!(r.json()["Id"], "1");
}

async fn proxy_headers_shape_service_urls(app: App) {
    let r = app
        .request(
            Method::GET,
            "BusinessPartners('1')",
            &[("x-forwarded-proto", "https")],
            "",
        )
        .await;
    assert_eq!(
        r.json()["@odata.context"],
        "https://test.local/odata/$metadata#BusinessPartners/$entity"
    );
    let doc = app.ok("").await;
    assert_eq!(doc["@odata.context"], "http://test.local/odata/$metadata");
}

async fn trailing_slash_root(app: App) {
    // `App::get("")` asks for `/odata/`, the form .NET clients send.
    assert_eq!(app.status("").await, 200);
    let r = app
        .send(
            Request::builder()
                .uri("/odata")
                .header(header::HOST, "test.local")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 200);
    assert!(r.json()["value"].as_array().unwrap().len() > 3);
}

async fn oversized_bodies_are_refused(app: App) {
    let body = format!(r#"{{"Id":"9","Name":"{}"}}"#, "x".repeat(17 * 1024 * 1024));
    let r = app
        .request(
            Method::POST,
            "BusinessPartners",
            &[("content-type", "application/json")],
            &body,
        )
        .await;
    assert_eq!(r.status, 413);
    assert_eq!(app.status("BusinessPartners('9')").await, 404);
}

/// A `$ref` adds or removes a link, not the target: Insert and Delete restrictions of the
/// target don't apply, while the owner, whose navigation changes, must be updatable.
async fn links_follow_the_owner_restrictions(app: App) {
    let send = |method: Method, uri: &'static str, body: serde_json::Value| {
        let app = app.clone();
        async move {
            app.request(
                method,
                uri,
                &[("content-type", "application/json")],
                &body.to_string(),
            )
            .await
        }
    };
    let r = send(
        Method::POST,
        "BusinessPartners('4')/Projects/$ref",
        json!({"@odata.id": "Projects(30)"}),
    )
    .await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert_eq!(
        app.column("BusinessPartners('4')/Projects", "Id").await,
        [30]
    );
    let r = send(
        Method::DELETE,
        "BusinessPartners('4')/Projects/$ref?$id=Projects(30)",
        json!({}),
    )
    .await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert!(
        app.column("BusinessPartners('4')/Projects", "Id")
            .await
            .is_empty()
    );
    assert_eq!(app.status("Projects(30)").await, 200, "the project stays");
    let r = send(Method::POST, "Projects", json!({"Id": 40, "Title": "New"})).await;
    assert_eq!(r.status, 405, "creating a project is still refused");

    // Divisions can't be updated, so neither their own links nor ones that write their rows.
    let r = send(
        Method::PUT,
        "Divisions('BE')/Parent/$ref",
        json!({"@odata.id": "Divisions('FR')"}),
    )
    .await;
    assert_eq!(r.status, 405, "{}", r.text());
    let r = send(
        Method::POST,
        "Divisions('FR')/Children/$ref",
        json!({"@odata.id": "Divisions('BE')"}),
    )
    .await;
    assert_eq!(r.status, 405, "{}", r.text());
    let r = send(Method::DELETE, "Divisions('BE')/Parent/$ref", json!({})).await;
    assert_eq!(r.status, 405, "{}", r.text());
    assert_eq!(app.ok("Divisions('BE')").await["ParentCode"], "DE");
}

mod run {
    use super::common;
    use odatabase::{Database, Service};

    fn build() -> impl FnOnce(Database) -> Service {
        super::service
    }

    crate::scenarios!(build();
        expand_and_skip_restrictions,
        now_in_filters,
        unimplemented_features_answer_with_a_status,
        delta_payloads_refuse_links,
        async_preference_is_answered_synchronously,
        proxy_headers_shape_service_urls,
        trailing_slash_root,
        oversized_bodies_are_refused,
        links_follow_the_owner_restrictions,
    );
}

/// The service root includes routers the mount is nested in.
#[tokio::test]
async fn nested_mounts_keep_their_prefix() {
    let pool = common::sqlite_pool(common::SCHEMA).await;
    let service = common::builder().build(pool).unwrap();
    let router = Router::new().nest("/api", odatabase::mount("/odata", service));
    let app = App { router };
    let r = app
        .send(
            Request::builder()
                .uri("/api/odata/BusinessPartners('1')")
                .header(header::HOST, "example.org:8080")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        r.json()["@odata.context"],
        "http://example.org:8080/api/odata/$metadata#BusinessPartners/$entity"
    );
}

/// `Service::handle` serves frameworks other than axum.
#[tokio::test]
async fn service_handle_without_axum() {
    let pool = common::sqlite_pool(common::SCHEMA).await;
    let service = common::builder().build(pool).unwrap();
    let response = service
        .handle(ODataRequest::get(
            "https://svc.example/root",
            "BusinessPartners",
            "$filter=Address/City eq 'Berlin'&$select=Id&$orderby=Id",
        ))
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(
        response.headers.get("odata-version").unwrap(),
        "4.0",
        "every response names the version"
    );
    let doc: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(
        doc["@odata.context"],
        "https://svc.example/root/$metadata#BusinessPartners(Id)"
    );
    let ids: Vec<_> = doc["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["Id"].clone())
        .collect();
    assert_eq!(ids, [json!("1"), json!("4")]);
    assert!(doc["value"][0].get("Name").is_none(), "$select applies");

    let response = service
        .handle(ODataRequest::get("https://svc.example/root", "Nope", ""))
        .await;
    assert_eq!(response.status, 404);
    let doc: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert!(doc["error"]["message"].is_string(), "{doc}");
}

/// `ExpandLimits::max_levels` bounds `$levels=max` and explicit levels alike.
#[tokio::test]
async fn levels_max_follows_the_config() {
    let app = common::sqlite(|db| {
        common::builder()
            .config(Config {
                expand_limits: ExpandLimits {
                    max_levels: 1,
                    ..ExpandLimits::default()
                },
                ..Config::default()
            })
            .build(db)
            .unwrap()
    })
    .await;
    for query in ["$levels=max", "$levels=5"] {
        let doc = app
            .ok(&format!("Divisions('EU')?$expand=Children({query})"))
            .await;
        let de = doc["Children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["Code"] == "DE")
            .unwrap_or_else(|| panic!("{doc}"));
        assert!(de.get("Children").is_none(), "{query}: {de}");
    }
}

/// `matchesPattern` is a regular expression on Postgres and a 501 on SQLite.
#[tokio::test]
async fn matches_pattern_per_dialect() {
    let query = "BusinessPartners?$filter=matchesPattern(Name,'^(First|Second) ')&$orderby=Id";
    let sqlite = common::sqlite_default().await;
    assert_eq!(sqlite.status(query).await, 501);
    if !common::postgres_can_run() {
        return;
    }
    let server = common::PostgresServer::start().await;
    let pool = server.pool(common::SCHEMA).await;
    let app = App::new(common::builder().build(pool).unwrap());
    assert_eq!(app.ids(query).await, ["1", "2"]);
}
