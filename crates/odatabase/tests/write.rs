mod common;

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use common::App;
use odatabase::{ChangeContext, ChangeHandler, EntityKey, EntityPayload, ODataError};
use serde_json::json;

const JSON: (&str, &str) = ("content-type", "application/json");

async fn post(app: &App, uri: &str, body: serde_json::Value) -> common::Response {
    app.request(Method::POST, uri, &[JSON], &body.to_string())
        .await
}

async fn patch(app: &App, uri: &str, body: serde_json::Value) -> common::Response {
    app.request(Method::PATCH, uri, &[JSON], &body.to_string())
        .await
}

async fn create_and_read_back(app: App) {
    let r = post(
        &app,
        "BusinessPartners",
        json!({
            "@odata.type": "#Test.Organization",
            "Id": "9",
            "Name": "New",
            "Rating": 7,
            "Revenue": 19.99,
            "Active": true,
            "Abc": "B",
            "Flags": "Read,Write",
            "CreatedAt": "2025-01-02T03:04:05+01:00",
            "Founded": "2020-02-02",
            "ExternalId": "11111111-2222-3333-4444-555555555555",
            "Address": {"City": "Rome", "Country": "ITA"},
            "Comments": ["a", "b"],
            "Phones": [{"Kind": "work", "Number": "9"}],
            "Size": 3
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(
        r.header("location"),
        Some("http://test.local/odata/BusinessPartners('9')")
    );
    assert_eq!(r.header("etag"), Some("W/\"1\""));
    let doc = r.json();
    assert_eq!(doc["Id"], "9");
    assert_eq!(doc["Version"], 1, "the version starts at one");
    assert_eq!(doc["@odata.type"], "#Test.Organization");
    let doc = app.ok("BusinessPartners('9')").await;
    assert_eq!(doc["Revenue"], json!(19.99));
    assert_eq!(doc["Flags"], "Read,Write");
    assert_eq!(doc["CreatedAt"], "2025-01-02T02:04:05Z");
    assert_eq!(
        doc["Address"],
        json!({"Street": null, "City": "Rome", "Country": "ITA"})
    );
    assert_eq!(doc["Comments"], json!(["a", "b"]));
    assert_eq!(doc["Phones"][0]["Number"], "9");
    assert_eq!(doc["Size"], 3);

    let r = app
        .request(
            Method::POST,
            "BusinessPartners",
            &[JSON, ("prefer", "return=minimal")],
            &json!({"@odata.type": "#Test.Person", "Id": "10", "FirstName": "Zoe", "Active": true})
                .to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        r.header("odata-entityid"),
        Some("http://test.local/odata/BusinessPartners('10')")
    );
    assert_eq!(r.header("preference-applied"), Some("return=minimal"));
    assert_eq!(
        app.ok("BusinessPartners('10')").await["@odata.type"],
        "#Test.Person"
    );
}

async fn generated_keys(app: App) {
    let r = post(&app, "Tickets", json!({"Title": "one", "Tags": ["x", "y"]})).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let first = r.json()["Id"].as_i64().unwrap();
    let r = post(&app, "Tickets", json!({"Title": "two", "Id": 999})).await;
    let second = r.json()["Id"].as_i64().unwrap();
    assert_eq!(second, first + 1, "a computed key ignores client input");
    assert_eq!(
        app.ok(&format!("Tickets({first})/Tags")).await["value"],
        json!(["x", "y"])
    );
}

async fn rejected_payloads(app: App) {
    let cases = [
        (
            json!({"Id": "x", "Active": "yes"}),
            "Active: expected Edm.Boolean",
        ),
        (
            json!({"Id": "x", "Active": true, "Nope": 1}),
            "unknown property Nope",
        ),
        (json!({"Id": "x", "Active": null}), "Active cannot be null"),
        (json!({"Active": true}), "Id is required"),
        (json!({"Id": "x"}), "Active is required"),
        (
            json!({"Id": "x", "Active": true, "Rating": "a"}),
            "Rating: expected Edm.Int32",
        ),
        (
            json!({"Id": "x", "Active": true, "Rating": 9999999999i64}),
            "Rating: expected Edm.Int32",
        ),
        (
            json!({"Id": "x", "Active": true, "Founded": "soon"}),
            "Founded",
        ),
        (
            json!({"Id": "x", "Active": true, "Abc": "Z"}),
            "Z is not a member",
        ),
        (
            json!({"Id": "x", "Active": true, "Address": 5}),
            "must be an object",
        ),
        (
            json!({"Id": "x", "Active": true, "Address": {"Nope": 1}}),
            "unknown property Nope",
        ),
        (
            json!({"Id": "x", "Active": true, "Comments": "a"}),
            "must be an array",
        ),
        (
            json!({"Id": "x", "Active": true, "@odata.type": "#Test.Division"}),
            "does not derive",
        ),
    ];
    for (body, message) in cases {
        let r = post(&app, "BusinessPartners", body.clone()).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{body}: {}", r.text());
        let text = r.json()["error"]["message"].as_str().unwrap().to_owned();
        assert!(text.contains(message), "{body}: {text}");
    }
    let r = post(&app, "Tickets", json!({"Title": "x".repeat(21)})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = app
        .request(Method::POST, "BusinessPartners", &[JSON], "{not json")
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = post(
        &app,
        "BusinessPartners",
        json!({"Id": "1", "Active": true, "@odata.type": "#Test.Person"}),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::CONFLICT,
        "duplicate key: {}",
        r.text()
    );
    assert!(
        !r.text().contains("business_partner"),
        "no schema leaks: {}",
        r.text()
    );
    let r = post(&app, "BusinessPartners('1')", json!({})).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    let r = app
        .request(Method::DELETE, "BusinessPartners", &[], "")
        .await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
}

async fn patch_and_put(app: App) {
    let r = patch(
        &app,
        "BusinessPartners('1')",
        json!({"Name": "Renamed", "Address": {"City": "Bonn"}}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(r.header("etag"), Some("W/\"4\""), "the version advances");
    let doc = app.ok("BusinessPartners('1')").await;
    assert_eq!(doc["Name"], "Renamed");
    assert_eq!(doc["Rating"], 5, "PATCH leaves other properties");
    assert_eq!(doc["Address"]["City"], "Bonn");
    assert_eq!(
        doc["Address"]["Street"], "Main 1",
        "PATCH merges complex values"
    );
    assert_eq!(doc["Version"], 4);

    let r = app
        .request(
            Method::PATCH,
            "BusinessPartners('1')",
            &[JSON, ("prefer", "return=representation")],
            &json!({"Rating": 6}).to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["Rating"], 6);

    let r = app
        .request(
            Method::PUT,
            "BusinessPartners('2')",
            &[JSON],
            &json!({"Active": true, "Name": "Only"}).to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    let doc = app.ok("BusinessPartners('2')").await;
    assert_eq!(doc["Name"], "Only");
    assert_eq!(doc["Rating"], json!(null), "PUT resets what the body omits");
    assert_eq!(doc["Address"], json!(null));
    let r = app
        .request(
            Method::PUT,
            "BusinessPartners('2')",
            &[JSON],
            &json!({"Name": "x"}).to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    let r = patch(&app, "BusinessPartners('nope')", json!({"Name": "x"})).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = patch(&app, "BusinessPartners('1')", json!({"Id": "2"})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = patch(
        &app,
        "BusinessPartners('1')",
        json!({"Id": "1", "Name": "same key"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = patch(&app, "BusinessPartners('1')", json!({"Comments": ["only"]})).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.ok("BusinessPartners('1')").await["Comments"],
        json!(["only"])
    );
}

async fn optimistic_locking(app: App) {
    let with = |etag: &'static str| [JSON, ("if-match", etag)];
    let body = json!({"Name": "x"}).to_string();
    let r = app
        .request(
            Method::PATCH,
            "BusinessPartners('1')",
            &with("W/\"2\""),
            &body,
        )
        .await;
    assert_eq!(r.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(app.ok("BusinessPartners('1')").await["Name"], "First Org");
    let r = app
        .request(
            Method::PATCH,
            "BusinessPartners('1')",
            &with("W/\"3\""),
            &body,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    let r = app
        .request(
            Method::PATCH,
            "BusinessPartners('1')",
            &with("W/\"3\""),
            &body,
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::PRECONDITION_FAILED,
        "the tag is now stale"
    );
    let r = app
        .request(Method::PATCH, "BusinessPartners('1')", &with("*"), &body)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app
        .request(
            Method::DELETE,
            "BusinessPartners('4')",
            &[("if-match", "W/\"9\"")],
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(app.status("BusinessPartners('4')").await, 200);
}

async fn delete_entities(app: App) {
    let r = app
        .request(Method::DELETE, "BusinessPartners('4')", &[], "")
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(app.status("BusinessPartners('4')").await, 404);
    let r = app
        .request(Method::DELETE, "BusinessPartners('4')", &[], "")
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = app
        .request(Method::DELETE, "BusinessPartners('1')", &[], "")
        .await;
    assert_eq!(
        r.status,
        StatusCode::CONFLICT,
        "referenced by roles: {}",
        r.text()
    );
    assert_eq!(
        app.ok("BusinessPartners('1')").await["Comments"],
        json!(["first", "second"]),
        "rolled back"
    );
    let r = app
        .request(
            Method::DELETE,
            "BusinessPartners('3')/Roles(PartnerId='3',Code=2)",
            &[],
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app
        .request(Method::DELETE, "BusinessPartners('3')", &[], "")
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
}

async fn property_updates(app: App) {
    let put = |uri: &'static str, body: serde_json::Value| {
        let app = app.clone();
        async move {
            app.request(Method::PUT, uri, &[JSON], &body.to_string())
                .await
        }
    };
    let r = put("BusinessPartners('1')/Name", json!({"value": "Direct"})).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(
        app.ok("BusinessPartners('1')/Name").await["value"],
        "Direct"
    );
    assert_eq!(app.ok("BusinessPartners('1')").await["Version"], 4);
    let r = app
        .request(
            Method::PUT,
            "BusinessPartners('1')/Rating/$value",
            &[("content-type", "text/plain")],
            "42",
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(
        app.get("BusinessPartners('1')/Rating/$value").await.text(),
        "42"
    );
    let r = patch(
        &app,
        "BusinessPartners('1')/Address",
        json!({"City": "Kiel"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(
        app.ok("BusinessPartners('1')/Address").await["Street"],
        "Main 1"
    );
    let r = put("BusinessPartners('1')/Address", json!({"City": "Ulm"})).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.ok("BusinessPartners('1')/Address").await["Street"],
        json!(null)
    );
    let r = put(
        "BusinessPartners('1')/Address/Country",
        json!({"value": "AUT"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(
        app.ok("BusinessPartners('1')/Address").await["Country"],
        "AUT"
    );
    let r = put(
        "BusinessPartners('1')/Comments",
        json!({"value": ["p", "q", "r"]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(
        app.ok("BusinessPartners('1')/Comments").await["value"],
        json!(["p", "q", "r"])
    );

    let r = app
        .request(Method::DELETE, "BusinessPartners('1')/Rating", &[], "")
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(app.status("BusinessPartners('1')/Rating").await, 204);
    let r = app
        .request(Method::DELETE, "BusinessPartners('1')/Active", &[], "")
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = app
        .request(Method::DELETE, "BusinessPartners('1')/Address", &[], "")
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(app.status("BusinessPartners('1')/Address").await, 204);
    let r = app
        .request(Method::DELETE, "BusinessPartners('1')/Comments", &[], "")
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.ok("BusinessPartners('1')/Comments").await["value"],
        json!([])
    );
    let r = put("BusinessPartners('1')/Id", json!({"value": "z"})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

async fn create_below_parent(app: App) {
    let r = post(
        &app,
        "BusinessPartners('4')/Roles",
        json!({"Code": 8, "Label": "New"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(
        r.json()["PartnerId"],
        "4",
        "the parent fills the foreign key"
    );
    let r = post(
        &app,
        "BusinessPartners('4')/Roles",
        json!({"Code": 9, "PartnerId": "1"}),
    )
    .await;
    assert_eq!(
        r.json()["PartnerId"],
        "4",
        "the body cannot pick another parent"
    );
    let r = post(
        &app,
        "BusinessPartners('1')/Projects",
        json!({"Id": 40, "Title": "Delta"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(
        app.column("BusinessPartners('1')/Projects?$orderby=Id", "Id")
            .await,
        [10, 20, 40]
    );
    let r = post(&app, "BusinessPartners('nope')/Roles", json!({"Code": 8})).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

async fn deep_insert(app: App) {
    let r = post(&app, "BusinessPartners", json!({
        "@odata.type": "#Test.Person",
        "Id": "20",
        "Active": true,
        "Roles": [{"Code": 1, "Label": "x"}, {"Code": 2}],
        "Projects": [{"Id": 50, "Title": "Nested", "Partners@odata.bind": ["BusinessPartners('2')"]}],
        "Owner": {"@odata.type": "#Test.Person", "Id": "21", "Active": false}
    })).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let doc = r.json();
    assert_eq!(doc["Roles"].as_array().unwrap().len(), 2, "{doc}");
    assert_eq!(doc["Owner"]["Id"], "21");
    assert_eq!(doc["Projects"][0]["Title"], "Nested");
    assert_eq!(
        app.column("Projects(50)/Partners?$orderby=Id", "Id").await,
        ["2", "20"]
    );

    let r = post(
        &app,
        "BusinessPartners",
        json!({
            "@odata.type": "#Test.Person",
            "Id": "22",
            "Active": true,
            "Roles": [{"Code": 1}, {"Code": 1}]
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.text());
    assert_eq!(
        app.status("BusinessPartners('22')").await,
        404,
        "the whole insert rolled back"
    );
}

async fn links(app: App) {
    let r = post(&app, "Roles", json!({"PartnerId": "4", "Code": 7})).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    let r = post(
        &app,
        "BusinessPartners",
        json!({
            "@odata.type": "#Test.Person", "Id": "30", "Active": true,
            "Owner@odata.bind": "BusinessPartners('1')",
            "Projects@odata.bind": ["Projects(10)", "http://test.local/odata/Projects(20)"]
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(app.ok("BusinessPartners('30')/Owner").await["Id"], "1");
    assert_eq!(
        app.column("BusinessPartners('30')/Projects", "Id").await,
        [10, 20]
    );
    let r = post(
        &app,
        "BusinessPartners",
        json!({
            "@odata.type": "#Test.Person", "Id": "31", "Active": true,
            "Owner@odata.bind": "BusinessPartners('missing')"
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    let id = |url: &str| json!({"@odata.id": url}).to_string();
    let r = app
        .request(
            Method::PUT,
            "BusinessPartners('30')/Owner/$ref",
            &[JSON],
            &id("BusinessPartners('2')"),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(app.ok("BusinessPartners('30')/Owner").await["Id"], "2");
    let r = app
        .request(Method::DELETE, "BusinessPartners('30')/Owner/$ref", &[], "")
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(app.status("BusinessPartners('30')/Owner").await, 204);

    let r = app
        .request(
            Method::POST,
            "Projects(30)/Partners/$ref",
            &[JSON],
            &id("BusinessPartners('4')"),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(app.column("Projects(30)/Partners", "Id").await, ["4"]);
    let r = app
        .request(Method::DELETE, "Projects(30)/Partners('4')/$ref", &[], "")
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(app.column("Projects(30)/Partners", "Id").await.len(), 0);
    app.request(
        Method::POST,
        "Projects(30)/Partners/$ref",
        &[JSON],
        &id("BusinessPartners('4')"),
    )
    .await;
    let r = app
        .request(
            Method::DELETE,
            "Projects(30)/Partners/$ref?$id=BusinessPartners('4')",
            &[],
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(app.column("Projects(30)/Partners", "Id").await.len(), 0);

    let r = app
        .request(
            Method::POST,
            "BusinessPartners('4')/Owned/$ref",
            &[JSON],
            &id("BusinessPartners('3')"),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text());
    assert_eq!(app.ok("BusinessPartners('3')/Owner").await["Id"], "4");
}

mod run {
    use super::common;
    use odatabase::{Database, Service};

    fn build() -> impl FnOnce(Database) -> Service {
        |db| common::builder().build(db).unwrap()
    }

    crate::scenarios!(build();
        create_and_read_back,
        generated_keys,
        rejected_payloads,
        patch_and_put,
        optimistic_locking,
        delete_entities,
        property_updates,
        create_below_parent,
        deep_insert,
        links,
    );
}

struct Rules;

#[async_trait]
impl ChangeHandler for Rules {
    async fn before_create(
        &self,
        _ctx: &ChangeContext<'_>,
        entity: &mut EntityPayload,
    ) -> Result<(), ODataError> {
        if entity.type_name == "Ticket" {
            entity.values.push((
                "Note".into(),
                odatabase::PropertyValue::Value(odatabase::edm::Value::String("stamped".into())),
            ));
        }
        Ok(())
    }

    async fn before_delete(
        &self,
        _ctx: &ChangeContext<'_>,
        type_name: &str,
        _key: &EntityKey,
    ) -> Result<(), ODataError> {
        if type_name == "Ticket" {
            return Err(ODataError::new(403, "tickets stay"));
        }
        Ok(())
    }

    async fn validate_changes(&self, ctx: &ChangeContext<'_>) -> Result<(), ODataError> {
        if ctx.method == Method::PATCH {
            return Err(ODataError::new(422, "rejected at the end"));
        }
        Ok(())
    }
}

#[tokio::test]
async fn change_handler_hooks() {
    let app = common::sqlite(|db| common::builder().change_handler(Rules).build(db).unwrap()).await;
    let r = post(&app, "Tickets", json!({"Title": "t"})).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text());
    assert_eq!(r.json()["Note"], "stamped");
    let id = r.json()["Id"].as_i64().unwrap();
    let r = app
        .request(Method::DELETE, &format!("Tickets({id})"), &[], "")
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = patch(&app, &format!("Tickets({id})"), json!({"Title": "changed"})).await;
    assert_eq!(r.status.as_u16(), 422);
    assert_eq!(
        app.ok(&format!("Tickets({id})")).await["Title"],
        "t",
        "rolled back"
    );
}
