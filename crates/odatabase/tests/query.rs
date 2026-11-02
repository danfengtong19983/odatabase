mod common;

use axum::http::Method;
use common::App;
use serde_json::{Value, json};

/// A field of each row.
fn field(doc: &Value, name: &str) -> Vec<Value> {
    let rows = doc["value"].as_array().unwrap();
    rows.iter().map(|r| r[name].clone()).collect()
}

async fn compute(app: App) {
    let doc = app
        .ok("BusinessPartners?$compute=Rating mul 2 as D&$filter=D gt 5&$orderby=D desc&$select=Id,D")
        .await;
    assert_eq!(field(&doc, "Id"), [json!("1"), json!("2")]);
    assert_eq!(
        field(&doc, "D"),
        [json!(10), json!(6)],
        "filter and order see it"
    );
    assert!(
        doc["@odata.context"]
            .as_str()
            .unwrap()
            .ends_with("#BusinessPartners(Id,D)"),
        "{doc}"
    );

    let doc = app
        .ok("BusinessPartners?$compute=Rating mul 2 as D&$filter=Id eq '1'")
        .await;
    assert_eq!(doc["value"][0]["D"], 10);
    assert_eq!(doc["value"][0]["Name"], "First Org", "added to the entity");
    assert!(
        doc["@odata.context"]
            .as_str()
            .unwrap()
            .ends_with("#BusinessPartners(*,D)"),
        "{doc}"
    );

    let doc = app
        .ok("BusinessPartners?$compute=Rating mul 2 as D&$filter=Id eq '1'&$select=Id")
        .await;
    assert!(
        doc["value"][0]["D"].is_null(),
        "$select leaves it out: {doc}"
    );

    let doc = app
        .ok("BusinessPartners?$compute=Rating mul 2 as D,D add 1 as E&$filter=Id eq '2'&$select=E")
        .await;
    assert_eq!(doc["value"][0]["E"], 7, "a later item sees an earlier one");

    let doc = app
        .ok("BusinessPartners?$compute=concat(Name,'!') as N,year(Founded) as Y&$filter=Id eq '2'&$select=N,Y")
        .await;
    assert_eq!(doc["value"][0]["N"], "Second Org!");
    assert_eq!(doc["value"][0]["Y"], 2010);

    let doc = app
        .ok("BusinessPartners('1')?$compute=Rating mul 2 as D")
        .await;
    assert_eq!(doc["D"], 10, "a single entity");
    assert!(
        doc["@odata.context"]
            .as_str()
            .unwrap()
            .ends_with("#BusinessPartners(*,D)/$entity"),
        "{doc}"
    );

    let r = app
        .get("BusinessPartners/$count?$compute=Rating mul 2 as D&$filter=D gt 5")
        .await;
    assert_eq!(r.text(), "2");

    let doc = app
        .ok("BusinessPartners?$compute=Rating mul 2 as D&$filter=Id eq '1'&$expand=Roles($select=Code)")
        .await;
    assert_eq!(doc["value"][0]["Roles"].as_array().unwrap().len(), 3);

    let doc = app
        .ok("BusinessPartners?$apply=groupby((Abc),aggregate(Rating with sum as T))&$compute=T mul 2 as D&$filter=D gt 5&$orderby=D")
        .await;
    assert_eq!(field(&doc, "Abc"), [json!("B"), json!("A")]);
    assert_eq!(
        field(&doc, "D"),
        [json!(6), json!(10)],
        "$compute follows $apply"
    );

    for (uri, status) in [
        ("BusinessPartners?$compute=Nope as D", 400),
        ("BusinessPartners?$compute=Rating", 400),
        ("BusinessPartners?$compute=Rating as Name", 400),
        ("BusinessPartners?$compute=Rating as 1x", 400),
        ("BusinessPartners?$compute=Rating as D,Rating as D", 400),
        (
            "BusinessPartners?$compute=Rating as D&$compute=Rating as E",
            400,
        ),
        ("BusinessPartners?$filter=D gt 1", 400),
        ("BusinessPartners?$compute=Rating as D;DROP TABLE x", 400),
        (
            "BusinessPartners?$expand=Roles($compute=Code mul 2 as D)",
            501,
        ),
    ] {
        assert_eq!(app.status(uri).await, status, "{uri}");
    }
}

async fn ids(app: &App, filter: &str) -> Vec<String> {
    app.ids(&format!("BusinessPartners?$filter={filter}&$orderby=Id"))
        .await
}

async fn date_arithmetic(app: App) {
    for (filter, expected) in [
        (
            "CreatedAt add duration'P1D' gt 2024-06-02T11:00:00Z",
            vec!["2"],
        ),
        (
            "CreatedAt sub duration'PT1H' lt 2023-12-31T23:00:00Z",
            vec!["4"],
        ),
        ("Founded add duration'P1D' eq 2000-03-01", vec!["4"]),
        ("Founded sub duration'P1D' eq 1999-02-28", vec!["1"]),
        ("Founded sub 2000-02-28 eq duration'P1D'", vec!["4"]),
        (
            "CreatedAt sub 2024-01-15T08:00:00Z eq duration'PT30M'",
            vec!["1"],
        ),
        ("now() sub CreatedAt gt duration'P1D'", vec!["1", "2", "4"]),
        ("CreatedAt lt maxdatetime()", vec!["1", "2", "4"]),
        ("CreatedAt gt mindatetime()", vec!["1", "2", "4"]),
        (
            "duration'PT1H' add duration'PT1H' eq duration'PT2H' and Id eq '3'",
            vec!["3"],
        ),
        (
            "duration'PT1H' mul 2 eq duration'PT2H' and Id eq '3'",
            vec!["3"],
        ),
        (
            "2 mul duration'PT1H' eq duration'PT2H' and Id eq '3'",
            vec!["3"],
        ),
        (
            "duration'PT1H' div 2 eq duration'PT30M' and Id eq '3'",
            vec!["3"],
        ),
        (
            "-duration'PT1H' add duration'PT1H' eq duration'PT0S' and Id eq '3'",
            vec!["3"],
        ),
        ("CreatedAt add null eq null and Id eq '3'", vec!["3"]),
    ] {
        assert_eq!(ids(&app, filter).await, expected, "{filter}");
    }

    let doc = app
        .ok("BusinessPartners('1')?$select=L,F,G,H,I,J&$compute=\
             CreatedAt add duration'PT90S' as L,\
             Founded add duration'P1D' as F,\
             CreatedAt sub 2024-01-14T08:30:00Z as G,\
             G mul 2 as H,\
             G div 4 as I,\
             Founded sub 1999-02-28 as J")
        .await;
    let values: Vec<&Value> = ["L", "F", "G", "H", "I", "J"]
        .iter()
        .map(|k| &doc[*k])
        .collect();
    assert_eq!(
        values,
        [
            &json!("2024-01-15T08:31:30Z"),
            &json!("1999-03-02"),
            &json!("P1D"),
            &json!("P2D"),
            &json!("PT6H"),
            &json!("P1D"),
        ],
        "typed results: {doc}"
    );
    let doc = app
        .ok("BusinessPartners('3')?$compute=CreatedAt add duration'P1D' as L")
        .await;
    assert!(doc["L"].is_null(), "null stays null: {doc}");

    for filter in [
        "Founded add Founded eq Founded",
        "CreatedAt add CreatedAt eq CreatedAt",
        "Founded sub CreatedAt eq duration'P1D'",
        "duration'P1D' sub Founded eq Founded",
        "duration'P1D' add Founded eq Founded",
        "duration'P1D' mul duration'P1D' eq duration'P1D'",
        "2 div duration'P1D' eq 2",
        "duration'P1D' mod 2 eq duration'P1D'",
        "duration'P1D' divby 2 eq duration'P1D'",
        "Founded mul 2 eq Founded",
        "Name add duration'P1D' eq Name",
        "-Founded eq Founded",
    ] {
        assert_eq!(
            app.status(&format!("BusinessPartners?$filter={filter}"))
                .await,
            400,
            "{filter}"
        );
    }
}

async fn entity_by_id(app: App) {
    let doc = app.ok("$entity?$id=BusinessPartners('1')").await;
    assert_eq!(doc["Name"], "First Org");
    assert!(
        doc["@odata.context"]
            .as_str()
            .unwrap()
            .ends_with("#BusinessPartners/Test.Organization/$entity")
            || doc["@odata.context"]
                .as_str()
                .unwrap()
                .ends_with("#BusinessPartners/$entity"),
        "{doc}"
    );
    let doc = app
        .ok("$entity?$id=http://test.local/odata/BusinessPartners('1')")
        .await;
    assert_eq!(doc["Id"], "1");
    let doc = app
        .ok("%24entity?%24id=BusinessPartners%28%272%27%29")
        .await;
    assert_eq!(doc["Id"], "2", "encoded");
    let doc = app
        .ok("$entity?$id=BusinessPartners('1')&$select=Name&$expand=Roles($select=Code)")
        .await;
    assert!(doc["Rating"].is_null(), "{doc}");
    assert_eq!(doc["Roles"].as_array().unwrap().len(), 3);
    let doc = app
        .ok("$entity/Test.Organization?$id=BusinessPartners('1')")
        .await;
    assert_eq!(doc["Id"], "1", "a cast");
    let doc = app.ok("$entity?$id=Roles(PartnerId='1',Code=2)").await;
    assert_eq!(doc["Label"], "Seller", "a composite key");

    for (uri, status) in [
        ("$entity", 400),
        ("$entity?$select=Name", 400),
        ("$entity?$id=BusinessPartners", 400),
        ("$entity?$id=BusinessPartners('1')/Name", 400),
        (
            "$entity?$id=http://other.example/odata/BusinessPartners('1')",
            400,
        ),
        ("$entity?$id=BusinessPartners('1')?$select=Name", 400),
        (
            "$entity?$id=BusinessPartners('1')&$id=BusinessPartners('2')",
            400,
        ),
        ("$entity?$id=BusinessPartners('nope')", 404),
        ("$entity?$id=Nope(1)", 404),
        ("$entity/Test.Person?$id=BusinessPartners('1')", 404),
    ] {
        assert_eq!(app.status(uri).await, status, "{uri}");
    }
    let r = app
        .request(Method::DELETE, "$entity?$id=BusinessPartners('1')", &[], "")
        .await;
    assert_eq!(r.status.as_u16(), 405);
    assert_eq!(app.status("BusinessPartners('1')").await, 200);

    let batch = json!({"requests": [
        {"id": "1", "method": "GET", "url": "$entity?$id=BusinessPartners('4')"},
    ]});
    let r = app
        .request(
            Method::POST,
            "$batch",
            &[("content-type", "application/json")],
            &batch.to_string(),
        )
        .await;
    assert_eq!(r.json()["responses"][0]["body"]["Name"], "Fourth");
}

mod run {
    use super::common;
    use odatabase::{Database, Service};

    fn build() -> impl FnOnce(Database) -> Service {
        |db| common::builder().build(db).unwrap()
    }

    crate::scenarios!(build();
        compute,
        date_arithmetic,
        entity_by_id,
    );
}
