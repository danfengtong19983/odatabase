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
            "BusinessPartners?$apply=aggregate(Rating with sum from Abc with max from Active with average as T)",
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

/// Item-level `$top` and `$skip` on a collection property page each owner's items apart.
async fn collection_items_are_paged_per_owner(app: App) {
    let comments = |doc: &serde_json::Value| {
        doc["value"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p["Id"].clone(), p["Comments"].clone()))
            .collect::<Vec<_>>()
    };
    let doc = app
        .ok("BusinessPartners?$filter=Id in ('1','2','4')&$orderby=Id&$select=Id,Comments($orderby=$it desc;$top=1)")
        .await;
    assert_eq!(
        comments(&doc),
        [
            (json!("1"), json!(["second"])),
            (json!("2"), json!(["only"])),
            (json!("4"), json!([])),
        ]
    );
    let doc = app
        .ok("BusinessPartners?$filter=Id in ('1','2')&$orderby=Id&$select=Id,Comments($skip=1)")
        .await;
    assert_eq!(
        comments(&doc),
        [(json!("1"), json!(["second"])), (json!("2"), json!([]))]
    );
    let doc = app
        .ok("BusinessPartners?$filter=Id in ('1','2')&$orderby=Id&$select=Id,Comments($top=1;$count=true)")
        .await;
    assert_eq!(doc["value"][0]["Comments"], json!(["first"]));
    assert_eq!(
        doc["value"][0]["Comments@odata.count"], 2,
        "the count covers every item"
    );
    assert_eq!(doc["value"][1]["Comments@odata.count"], 1);
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
        collection_items_are_paged_per_owner,
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

async fn with_config(config: Config) -> App {
    common::sqlite(|db| common::builder().config(config).build(db).unwrap()).await
}

/// `Config::public_root` replaces the URL taken from request headers.
#[tokio::test]
async fn public_root_overrides_request_headers() {
    let app = with_config(Config {
        public_root: Some("https://api.example.com/v1/odata/".into()),
        ..Config::default()
    })
    .await;
    let r = app
        .request(
            Method::GET,
            "BusinessPartners?$top=1&$orderby=Id",
            &[("host", "evil.example"), ("x-forwarded-proto", "gopher")],
            "",
        )
        .await;
    let doc = r.json();
    assert_eq!(
        doc["@odata.context"],
        "https://api.example.com/v1/odata/$metadata#BusinessPartners"
    );
    let r = app
        .request(
            Method::POST,
            "Tickets",
            &[
                ("content-type", "application/json"),
                ("host", "evil.example"),
            ],
            &json!({"Title": "t"}).to_string(),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert!(
        r.header("location")
            .unwrap()
            .starts_with("https://api.example.com/v1/odata/Tickets("),
        "{:?}",
        r.header("location")
    );
}

/// Body and batch sizes come from `Config`.
#[tokio::test]
async fn body_and_batch_limits_are_configurable() {
    let app = with_config(Config {
        max_body_size: 64,
        max_batch_requests: 2,
        ..Config::default()
    })
    .await;
    let r = app
        .request(
            Method::POST,
            "Tickets",
            &[("content-type", "application/json")],
            &json!({"Title": "x", "Note": "y".repeat(100)}).to_string(),
        )
        .await;
    assert_eq!(r.status, 413);

    let app = with_config(Config {
        max_batch_requests: 2,
        ..Config::default()
    })
    .await;
    let part = "--b\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\n\r\nGET Settings HTTP/1.1\r\n\r\n\r\n";
    let send = |n: usize| {
        let app = app.clone();
        async move {
            app.request(
                Method::POST,
                "$batch",
                &[("content-type", "multipart/mixed; boundary=b")],
                &format!("{}--b--\r\n", part.repeat(n)),
            )
            .await
            .status
        }
    };
    assert_eq!(send(2).await, 200);
    assert_eq!(send(3).await, 400);
}

/// The sizes of `in` lists, `$orderby` and `$select`, and `matchesPattern`, are bounded by
/// `UriLimits`.
#[tokio::test]
async fn uri_limits_bound_list_sizes() {
    let app = common::sqlite_default().await;
    let list = |n: usize| {
        (0..n)
            .map(|i| format!("'{i}'"))
            .collect::<Vec<_>>()
            .join(",")
    };
    assert_eq!(
        app.status(&format!("BusinessPartners?$filter=Id in ({})", list(1000)))
            .await,
        200
    );
    assert_eq!(
        app.status(&format!("BusinessPartners?$filter=Id in ({})", list(1001)))
            .await,
        400
    );
    let terms = |n: usize| vec!["Id"; n].join(",");
    assert_eq!(
        app.status(&format!("BusinessPartners?$orderby={}", terms(32)))
            .await,
        200
    );
    assert_eq!(
        app.status(&format!("BusinessPartners?$orderby={}", terms(33)))
            .await,
        400
    );
    assert_eq!(
        app.status(&format!("BusinessPartners?$select={}", terms(256)))
            .await,
        200
    );
    assert_eq!(
        app.status(&format!("BusinessPartners?$select={}", terms(257)))
            .await,
        400
    );

    let app = with_config(Config {
        uri_limits: odatabase::UriLimits {
            max_in_items: 2,
            matches_pattern: false,
            ..odatabase::UriLimits::default()
        },
        ..Config::default()
    })
    .await;
    assert_eq!(
        app.status("BusinessPartners?$filter=Id in ('1','2','3')")
            .await,
        400
    );
    assert_eq!(
        app.status("BusinessPartners?$filter=matchesPattern(Name,'^F')")
            .await,
        501
    );
    assert_eq!(
        app.status("BusinessPartners?$apply=filter(matchesPattern(Name,'^F'))")
            .await,
        501
    );
}
