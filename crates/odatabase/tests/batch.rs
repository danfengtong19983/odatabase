mod common;

use axum::http::{Method, StatusCode};
use common::App;
use serde_json::json;

const MULTIPART: (&str, &str) = ("content-type", "multipart/mixed; boundary=b");

fn get(uri: &str) -> String {
    format!(
        "--b\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\n\r\nGET {uri} HTTP/1.1\r\nAccept: application/json\r\n\r\n\r\n"
    )
}

fn change(id: u32, method: &str, uri: &str, body: serde_json::Value) -> String {
    format!(
        "--c\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\nContent-ID: {id}\r\n\r\n{method} {uri} HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{body}\r\n"
    )
}

fn change_set(parts: &[String]) -> String {
    format!(
        "--b\r\nContent-Type: multipart/mixed; boundary=c\r\n\r\n{}--c--\r\n",
        parts.concat()
    )
}

async fn batch(app: &App, body: String) -> common::Response {
    app.request(
        Method::POST,
        "$batch",
        &[MULTIPART],
        &format!("{body}--b--\r\n"),
    )
    .await
}

async fn reads(app: App) {
    let r = batch(&app, get("BusinessPartners('1')") + &get("Nope") + "").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.header("content-type")
            .unwrap()
            .starts_with("multipart/mixed; boundary=")
    );
    let text = r.text();
    assert!(text.contains("HTTP/1.1 200 OK"), "{text}");
    assert!(text.contains("First Org"), "{text}");
    assert!(text.contains("HTTP/1.1 404 Not Found"), "{text}");
    let r = batch(&app, get("Nope") + &get("BusinessPartners('1')")).await;
    assert!(
        !r.text().contains("First Org"),
        "stops after the first failure"
    );
    let r = app
        .request(
            Method::POST,
            "$batch",
            &[MULTIPART, ("prefer", "odata.continue-on-error")],
            &format!("{}{}--b--\r\n", get("Nope"), get("BusinessPartners('1')")),
        )
        .await;
    assert!(r.text().contains("First Org"), "continues on request");
    assert_eq!(r.header("preference-applied"), Some("continue-on-error"));
}

async fn change_sets_commit_together(app: App) {
    let set = change_set(&[
        change(1, "POST", "Roles", json!({"PartnerId": "4", "Code": 50})),
        change(
            2,
            "PATCH",
            "BusinessPartners('4')",
            json!({"Name": "Batched"}),
        ),
    ]);
    let r = batch(&app, set).await;
    assert_eq!(r.status, StatusCode::OK);
    let text = r.text();
    assert!(text.contains("HTTP/1.1 201 Created"), "{text}");
    assert!(text.contains("HTTP/1.1 204 No Content"), "{text}");
    assert_eq!(app.ok("BusinessPartners('4')").await["Name"], "Batched");
    assert_eq!(app.status("Roles(PartnerId='4',Code=50)").await, 200);
}

async fn change_sets_roll_back_together(app: App) {
    let set = change_set(&[
        change(1, "POST", "Roles", json!({"PartnerId": "4", "Code": 60})),
        change(2, "PATCH", "BusinessPartners('4')", json!({"Name": "Lost"})),
        change(
            3,
            "POST",
            "Roles",
            json!({"PartnerId": "missing", "Code": 1}),
        ),
    ]);
    let r = batch(&app, set).await;
    assert_eq!(r.status, StatusCode::OK);
    let text = r.text();
    assert_eq!(
        text.matches("HTTP/1.1").count(),
        1,
        "one failure for the set: {text}"
    );
    assert!(text.contains("HTTP/1.1 409 Conflict"), "{text}");
    assert_eq!(app.status("Roles(PartnerId='4',Code=60)").await, 404);
    assert_eq!(app.ok("BusinessPartners('4')").await["Name"], "Fourth");
}

async fn content_id_references(app: App) {
    let set = change_set(&[
        change(
            1,
            "POST",
            "BusinessPartners",
            json!({"@odata.type": "#Test.Person", "Id": "70", "Active": true}),
        ),
        change(2, "POST", "$1/Roles", json!({"Code": 1})),
        change(
            3,
            "POST",
            "BusinessPartners",
            json!({
                "@odata.type": "#Test.Person", "Id": "71", "Active": true, "Owner@odata.bind": "$1"
            }),
        ),
    ]);
    let r = batch(&app, set).await;
    assert_eq!(r.text().matches("HTTP/1.1 201").count(), 3, "{}", r.text());
    assert_eq!(
        app.column("BusinessPartners('70')/Roles", "PartnerId")
            .await,
        ["70"]
    );
    assert_eq!(app.ok("BusinessPartners('71')/Owner").await["Id"], "70");
}

async fn get_in_a_change_set_fails(app: App) {
    let set = change_set(&[change(1, "GET", "Roles", json!({}))]);
    assert!(
        batch(&app, set)
            .await
            .text()
            .contains("HTTP/1.1 400 Bad Request")
    );
}

async fn json_format(app: App) {
    let body = json!({"requests": [
        {"id": "r1", "method": "get", "url": "BusinessPartners('1')?$select=Name"},
        {"id": "w1", "atomicityGroup": "g", "method": "post", "url": "/odata/BusinessPartners",
         "headers": {"content-type": "application/json"},
         "body": {"@odata.type": "#Test.Person", "Id": "80", "Active": true}},
        {"id": "w2", "atomicityGroup": "g", "method": "post", "url": "$w1/Roles",
         "headers": {"content-type": "application/json"}, "body": {"Code": 1}},
        {"id": "r2", "method": "get", "url": "http://test.local/odata/BusinessPartners('80')/Roles/$count"}
    ]});
    let r = app
        .request(
            Method::POST,
            "$batch",
            &[("content-type", "application/json")],
            &body.to_string(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let doc = r.json();
    let responses = doc["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 4);
    assert_eq!(responses[0]["id"], "r1");
    assert_eq!(responses[0]["status"], 200);
    assert_eq!(responses[0]["body"]["Name"], "First Org");
    assert_eq!(responses[1]["status"], 201);
    assert_eq!(responses[2]["status"], 201);
    assert_eq!(responses[3]["body"], 1);

    let body = json!({"requests": [
        {"id": "a", "atomicityGroup": "g", "method": "post", "url": "Roles",
         "headers": {"content-type": "application/json"}, "body": {"PartnerId": "4", "Code": 61}},
        {"id": "b", "atomicityGroup": "g", "method": "post", "url": "Roles",
         "headers": {"content-type": "application/json"}, "body": {"PartnerId": "nope", "Code": 1}}
    ]});
    let r = app
        .request(
            Method::POST,
            "$batch",
            &[("content-type", "application/json")],
            &body.to_string(),
        )
        .await;
    let doc = r.json();
    assert_eq!(doc["responses"].as_array().unwrap().len(), 1);
    assert_eq!(doc["responses"][0]["status"], 409);
    assert_eq!(app.status("Roles(PartnerId='4',Code=61)").await, 404);
}

async fn malformed_batches(app: App) {
    let send = |ct: &'static str, body: &'static str| {
        let app = app.clone();
        async move {
            app.request(Method::POST, "$batch", &[("content-type", ct)], body)
                .await
                .status
        }
    };
    assert_eq!(send("multipart/mixed", "x").await, StatusCode::BAD_REQUEST);
    assert_eq!(
        send("multipart/mixed; boundary=b", "--b\r\nnope").await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send("text/plain", "x").await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(
        send("application/json", "{}").await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.status("$batch").await, 405);
    let many = get("Roles").repeat(1001);
    assert_eq!(batch(&app, many).await.status, StatusCode::BAD_REQUEST);
}

mod run {
    use super::common;
    use odatabase::{Database, Service};

    fn build() -> impl FnOnce(Database) -> Service {
        |db| common::builder().build(db).unwrap()
    }

    crate::scenarios!(build();
        reads,
        change_sets_commit_together,
        change_sets_roll_back_together,
        content_id_references,
        get_in_a_change_set_fails,
        json_format,
        malformed_batches,
    );
}
