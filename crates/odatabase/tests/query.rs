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

async fn collection_options(app: App) {
    let comments = "BusinessPartners('1')/Comments";
    let phones = "BusinessPartners('1')/Phones";
    assert_eq!(
        app.ok(&format!("{comments}?$top=1")).await["value"],
        json!(["first"])
    );
    assert_eq!(
        app.ok(&format!("{comments}?$skip=1")).await["value"],
        json!(["second"])
    );
    let doc = app.ok(&format!("{comments}?$top=1&$count=true")).await;
    assert_eq!(doc["@odata.count"], 2, "before paging: {doc}");
    assert_eq!(
        app.ok(&format!("{comments}?$filter=contains($it,'ir')"))
            .await["value"],
        json!(["first"])
    );
    assert_eq!(
        app.ok(&format!("{comments}?$orderby=$it desc")).await["value"],
        json!(["second", "first"])
    );
    assert_eq!(
        app.get(&format!("{comments}/$count?$filter=contains($it,'ir')"))
            .await
            .text(),
        "1"
    );
    assert_eq!(
        app.ok(&format!("{comments}?$search=IR")).await["value"],
        json!(["first"]),
        "ignoring case"
    );
    assert_eq!(
        app.ok(&format!("{comments}?$search=NOT ir")).await["value"],
        json!(["second"])
    );
    let doc = app.ok(&format!("{phones}?$search=home")).await;
    assert_eq!(field(&doc, "Number"), [json!("222")], "complex items");
    let doc = app
        .ok("BusinessPartners('1')?$select=Comments($search=second)")
        .await;
    assert_eq!(doc["Comments"], json!(["second"]));
    let doc = app.ok(&format!("{phones}?$filter=Kind eq 'work'")).await;
    assert_eq!(field(&doc, "Number"), [json!("111")]);
    let doc = app.ok(&format!("{phones}?$orderby=Kind&$top=1")).await;
    assert_eq!(field(&doc, "Kind"), [json!("home")]);
    let doc = app.ok(&format!("{phones}?$select=Kind")).await;
    assert_eq!(field(&doc, "Number"), [Value::Null, Value::Null], "{doc}");

    let doc = app
        .ok("BusinessPartners('1')?$select=Comments($top=1)")
        .await;
    assert_eq!(doc["Comments"], json!(["first"]));
    let doc = app
        .ok("BusinessPartners('1')?$select=Comments($count=true;$orderby=$it desc)")
        .await;
    assert_eq!(doc["Comments"], json!(["second", "first"]));
    assert_eq!(doc["Comments@odata.count"], 2);
    let doc = app
        .ok("BusinessPartners?$filter=Id eq '1'&$select=Phones($filter=Kind eq 'home';$select=Number)")
        .await;
    assert_eq!(doc["value"][0]["Phones"], json!([{"Number": "222"}]));
    let doc = app
        .ok("BusinessPartners('1')?$select=Address($select=City)")
        .await;
    assert_eq!(doc["Address"], json!({"City": "Berlin"}));
    assert_eq!(app.status("BusinessPartners?$select=Test.*").await, 200);

    for (uri, status) in [
        ("BusinessPartners?$select=Name($top=1)", 400),
        ("BusinessPartners?$select=Comments($expand=x)", 400),
        ("BusinessPartners?$select=Address($top=1)", 400),
        ("BusinessPartners?$select=Comments($filter=Nope)", 400),
        ("BusinessPartners?$select=Other.*", 400),
        ("BusinessPartners('1')/Comments?$filter=Nope eq 1", 400),
    ] {
        assert_eq!(app.status(uri).await, status, "{uri}");
    }
}

async fn references(app: App) {
    let uri = "BusinessPartners('1')/Roles/$ref";
    let ids = |doc: &Value| doc["value"].as_array().unwrap().len();
    assert_eq!(ids(&app.ok(&format!("{uri}?$top=1")).await), 1);
    assert_eq!(ids(&app.ok(&format!("{uri}?$filter=Code gt 1")).await), 2);
    let doc = app
        .ok(&format!("{uri}?$orderby=Code desc&$count=true"))
        .await;
    assert_eq!(doc["@odata.count"], 3);
    assert!(
        doc["value"][0]["@odata.id"]
            .as_str()
            .unwrap()
            .contains("Code=3"),
        "{doc}"
    );
}

async fn levels_and_versions(app: App) {
    assert_eq!(
        app.status("BusinessPartners?$expand=Roles($levels=2)")
            .await,
        400,
        "Roles doesn't lead back to partners"
    );
    assert_eq!(
        app.status("BusinessPartners?$expand=*($levels=2)").await,
        200
    );
    let doc = app.ok("Divisions('EU')?$expand=Children($levels=2)").await;
    let children = doc["Children"].as_array().unwrap();
    let germany = children.iter().find(|c| c["Code"] == "DE").unwrap();
    assert_eq!(germany["Children"].as_array().unwrap().len(), 2, "{doc}");

    assert_eq!(app.status("BusinessPartners?$schemaversion=*").await, 200);
    assert_eq!(app.status("BusinessPartners?$schemaversion=2").await, 404);
    assert_eq!(app.status("$metadata?$schemaversion=2").await, 404);
}

async fn expand_pipelines(app: App) {
    let uri = "BusinessPartners?$orderby=Id&$expand=Roles";
    let doc = app
        .ok(&format!(
            "{uri}($compute=Code mul 2 as D;$orderby=D desc;$top=1;$select=Code,D)"
        ))
        .await;
    assert_eq!(doc["value"][0]["Roles"][0]["D"], 6, "{doc}");

    let doc = app
        .ok(&format!("{uri}($apply=aggregate($count as N))"))
        .await;
    let counts: Vec<&Value> = doc["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["Roles"][0]["N"])
        .collect();
    assert_eq!(
        counts,
        [&json!(3), &json!(1), &json!(1), &json!(0)],
        "per parent"
    );

    let doc = app
        .ok("BusinessPartners('1')?$expand=Roles($apply=groupby((Label),aggregate(Code with sum as T));$orderby=T desc;$top=2)")
        .await;
    assert_eq!(doc["Roles"].as_array().unwrap().len(), 2, "{doc}");
    assert_eq!(doc["Roles"][0]["Label"], "Agent");
    assert_eq!(doc["Roles"][0]["T"], 3);

    let doc = app
        .ok("BusinessPartners('1')?$select=Name&$expand=Roles($apply=orderby(Code desc)/top(1))")
        .await;
    assert_eq!(doc["Roles"], json!([doc["Roles"][0].clone()]));
    assert_eq!(
        doc["Roles"][0]["Code"], 3,
        "narrowed per parent, whatever $select left"
    );

    let doc = app
        .ok("BusinessPartners('1')?$expand=Roles($apply=topcount(2,Code);$count=true)")
        .await;
    assert_eq!(doc["Roles"].as_array().unwrap().len(), 2);
    assert_eq!(doc["Roles@odata.count"], 2);

    for uri in [
        "BusinessPartners?$expand=Owner($apply=aggregate($count as N))",
        "BusinessPartners?$expand=Roles($compute=Nope as D)",
        "BusinessPartners?$expand=Roles($apply=groupby((Label));$select=Code)",
    ] {
        assert_eq!(app.status(uri).await, 400, "{uri}");
    }
    assert_eq!(
        app.status("BusinessPartners?$expand=Roles($apply=groupby((Label));$expand=Partner)")
            .await,
        400
    );
}

async fn after_aggregation(app: App) {
    let doc = app
        .ok("BusinessPartners?$apply=groupby((Owner/Id),aggregate($count as N))&$expand=Owner($select=Name)&$orderby=N")
        .await;
    let rows = doc["value"].as_array().unwrap();
    let owned = rows.iter().find(|r| r["Owner"].is_object()).unwrap();
    assert_eq!(owned["Owner"]["Name"], "First Org", "{doc}");
    assert_eq!(owned["N"], 2);
    assert!(rows.iter().any(|r| r["Owner"].is_null()), "{doc}");
    for uri in [
        "BusinessPartners?$apply=groupby((Abc))&$expand=Roles",
        "BusinessPartners?$apply=groupby((Owner/Name))&$expand=Owner",
        "BusinessPartners?$apply=groupby((Abc))&$expand=*",
    ] {
        assert_eq!(app.status(uri).await, 400, "{uri}");
    }

    let names = "BusinessPartners?$apply=groupby((Name))&$search=";
    assert_eq!(
        field(&app.ok(&format!("{names}first")).await, "Name"),
        [json!("First Org")]
    );
    assert_eq!(
        app.ok(&format!("{names}NOT first")).await["value"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        app.ok(&format!("{names}first OR fourth")).await["value"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let doc = app
        .ok("BusinessPartners?$apply=groupby((Active),aggregate($count as N))&$search=x&$count=true")
        .await;
    assert_eq!(doc["@odata.count"], 0, "no text to search");
}

async fn across_sets(app: App) {
    let doc = app.ok("$all?$count=true").await;
    assert!(
        doc["@odata.context"]
            .as_str()
            .unwrap()
            .ends_with("#Collection(Edm.EntityType)"),
        "{doc}"
    );
    let rows = doc["value"].as_array().unwrap();
    assert_eq!(doc["@odata.count"].as_u64(), Some(rows.len() as u64));
    assert!(
        rows.iter()
            .all(|r| r["@odata.type"].is_string() && r["@odata.id"].is_string())
    );
    assert!(rows.iter().any(|r| r["Name"] == "First Org"));
    assert!(rows.iter().any(|r| r["Label"] == "Buyer") || rows.iter().any(|r| r["Code"] == "EU"));

    let doc = app.ok("$all?$filter=Name eq 'Fourth'").await;
    assert_eq!(
        field(&doc, "Name"),
        [json!("Fourth")],
        "sets without Name add nothing"
    );
    let doc = app.ok("$all/Test.Organization?$count=true").await;
    assert_eq!(doc["@odata.count"], 2);
    let doc = app.ok("$all?$top=2&$skip=3").await;
    assert_eq!(doc["value"].as_array().unwrap().len(), 2);
    let all = app.ok("$all").await;
    assert_eq!(doc["value"][0], all["value"][3], "paging runs across sets");
    assert_eq!(app.status("$all/Test.Nope").await, 404);
    assert_eq!(app.status("$all?$apply=aggregate($count as N)").await, 400);
    assert_eq!(
        app.status("$all?$filter=Nope eq 1").await,
        400,
        "fits no set"
    );
    let r = app.request(Method::POST, "$all", &[], "").await;
    assert_eq!(r.status.as_u16(), 405);

    let cross = "$crossjoin(BusinessPartners,Divisions)";
    assert_eq!(
        app.ok(&format!("{cross}?$count=true&$top=1")).await["@odata.count"],
        20
    );

    let join = "$crossjoin(BusinessPartners,Roles)?$filter=BusinessPartners/Id eq Roles/PartnerId";
    let doc = app
        .ok(&format!(
            "{join}&$count=true&$orderby=Roles/Code desc,Roles/PartnerId desc"
        ))
        .await;
    assert_eq!(doc["@odata.count"], 5);
    assert!(
        doc["@odata.context"]
            .as_str()
            .unwrap()
            .ends_with("#Collection(Edm.ComplexType)"),
        "{doc}"
    );
    assert_eq!(
        doc["value"][0],
        json!({
            "BusinessPartners@odata.navigationLink": "BusinessPartners('1')",
            "Roles@odata.navigationLink": "Roles(PartnerId='1',Code=3)",
        })
    );
    let doc = app
        .ok(&format!(
            "{join} and Roles/Code eq 3&$expand=BusinessPartners($select=Name),Roles"
        ))
        .await;
    assert_eq!(
        doc["value"][0]["BusinessPartners"]["Name"], "First Org",
        "{doc}"
    );
    assert_eq!(doc["value"][0]["Roles"]["Label"], "Agent");
    let doc = app.ok(&format!("{join}&$top=2&$skip=1")).await;
    assert_eq!(doc["value"].as_array().unwrap().len(), 2);
    for (uri, status) in [
        ("$crossjoin(Nope,Roles)", 404),
        ("$crossjoin(Roles,Roles)", 400),
        ("$crossjoin(BusinessPartners,Roles)?$select=Name", 400),
        (
            "$crossjoin(BusinessPartners,Roles)?$filter=Name eq 'x'",
            400,
        ),
        (
            "$crossjoin(BusinessPartners,Roles)?$filter=Roles/Nope eq 1",
            400,
        ),
        ("$crossjoin(BusinessPartners,Roles)?$expand=Divisions", 400),
    ] {
        assert_eq!(app.status(uri).await, status, "{uri}");
    }
}

async fn positional_insert(app: App) {
    let post = |uri: &'static str, body: Value| {
        let app = &app;
        async move {
            app.request(
                Method::POST,
                uri,
                &[("content-type", "application/json")],
                &body.to_string(),
            )
            .await
        }
    };
    let comments = "BusinessPartners('1')/Comments";
    let r = post(
        "BusinessPartners('1')/Comments?$index=0",
        json!({"value": "zero"}),
    )
    .await;
    assert_eq!(r.status.as_u16(), 204, "{}", r.text());
    assert_eq!(
        app.ok(comments).await["value"],
        json!(["zero", "first", "second"])
    );
    post(
        "BusinessPartners('1')/Comments?$index=1",
        json!({"value": "one"}),
    )
    .await;
    assert_eq!(
        app.ok(comments).await["value"],
        json!(["zero", "one", "first", "second"])
    );
    post(
        "BusinessPartners('1')/Comments?$index=-1",
        json!({"value": "last"}),
    )
    .await;
    post(
        "BusinessPartners('1')/Comments?$index=-2",
        json!({"value": "near"}),
    )
    .await;
    post("BusinessPartners('1')/Comments", json!({"value": "end"})).await;
    post(
        "BusinessPartners('1')/Comments?$index=99",
        json!({"value": "far"}),
    )
    .await;
    assert_eq!(
        app.ok(comments).await["value"],
        json!([
            "zero", "one", "first", "second", "near", "last", "end", "far"
        ])
    );
    let r = post(
        "BusinessPartners('1')/Phones?$index=0",
        json!({"Kind": "fax", "Number": "9"}),
    )
    .await;
    assert_eq!(r.status.as_u16(), 204, "{}", r.text());
    assert_eq!(
        app.ok("BusinessPartners('1')/Phones").await["value"][0]["Kind"],
        "fax"
    );
    assert_eq!(
        app.ok("BusinessPartners('1')").await["Version"],
        10,
        "each insert bumps the ETag"
    );

    assert_eq!(
        post("BusinessPartners('1')/Comments", json!({}))
            .await
            .status
            .as_u16(),
        400
    );
    assert_eq!(
        post("BusinessPartners('1')/Comments", json!({"value": 1}))
            .await
            .status
            .as_u16(),
        400
    );
    assert_eq!(
        post("BusinessPartners('nope')/Comments", json!({"value": "x"}))
            .await
            .status
            .as_u16(),
        404
    );
    assert_eq!(
        app.status("BusinessPartners('1')/Comments?$index=1").await,
        400
    );
    assert_eq!(app.status("BusinessPartners?$index=x").await, 400);
    let r = post("BusinessPartners?$index=0", json!({"Id": "9", "Kind": "x"})).await;
    assert_eq!(
        r.status.as_u16(),
        400,
        "only collection properties have positions"
    );
    let xml = app.get("$metadata").await.text();
    assert!(xml.contains("Core.PositionalInsert"), "{xml}");
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
        collection_options,
        references,
        levels_and_versions,
        expand_pipelines,
        after_aggregation,
        across_sets,
        positional_insert,
    );
}
