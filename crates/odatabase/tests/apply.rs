mod common;

use common::App;
use odatabase::OffsetPaging;
use serde_json::{Value, json};

/// The rows of a `$apply` read, without annotations.
async fn rows(app: &App, query: &str) -> Vec<Value> {
    let uri = format!("BusinessPartners?$apply={query}");
    let doc = app.ok(&uri).await;
    doc["value"]
        .as_array()
        .unwrap_or_else(|| panic!("{uri}: {doc}"))
        .iter()
        .map(|row| {
            let kept = row
                .as_object()
                .unwrap()
                .iter()
                .filter(|(k, _)| !k.contains('@'));
            Value::Object(kept.map(|(k, v)| (k.clone(), v.clone())).collect())
        })
        .collect()
}

async fn aggregate_methods(app: App) {
    let r = rows(
        &app,
        "aggregate(Rating with sum as Total,Rating with average as Mean,Rating with min as Low,\
         Rating with max as High,Rating with countdistinct as Kinds,$count as Rows)",
    )
    .await;
    assert_eq!(
        r,
        [json!({"Total": 9, "Mean": 3.0, "Low": 1, "High": 5, "Kinds": 3, "Rows": 4})],
        "nulls are ignored, and $count counts every row"
    );
    let r = rows(&app, "aggregate(Revenue with sum as R,Score with sum as S)").await;
    assert_eq!(r, [json!({"R": 1290.75, "S": 4.5})]);
    let r = rows(
        &app,
        "aggregate(Founded with max as Last,Name with min as First)",
    )
    .await;
    assert_eq!(r, [json!({"Last": "2010-07-20", "First": "First Org"})]);
    let r = rows(&app, "aggregate(Rating mul 2 with sum as Twice)").await;
    assert_eq!(
        r,
        [json!({"Twice": 18})],
        "any expression can be aggregated"
    );
    let r = rows(
        &app,
        "filter(Rating gt 99)/aggregate(Rating with sum as T,$count as N)",
    )
    .await;
    assert_eq!(
        r,
        [json!({"T": null, "N": 0})],
        "an empty input still has one result"
    );
}

async fn annotations_and_context(app: App) {
    let doc = app.ok("BusinessPartners?$apply=groupby((Abc),aggregate(Rating with sum as Total))&$filter=Abc eq 'A'").await;
    assert_eq!(
        doc["@odata.context"],
        "http://test.local/odata/$metadata#BusinessPartners(Abc,Total)"
    );
    let row = &doc["value"][0];
    assert_eq!(
        row["@odata.id"],
        json!(null),
        "aggregated rows have no identity"
    );
    assert_eq!(
        row["Total@odata.type"], "#Decimal",
        "aliases are not in the model"
    );
    assert!(
        row.get("Abc@odata.type").is_none(),
        "declared properties need no annotation"
    );
    assert_eq!(row["Abc"], "A", "enum keys keep their member name");
    let doc = app
        .ok("BusinessPartners?$apply=groupby((Address/City,Owner/Name))")
        .await;
    assert_eq!(
        doc["@odata.context"],
        "http://test.local/odata/$metadata#BusinessPartners(Address(City),Owner(Name))"
    );
    let r = app
        .request(
            axum::http::Method::GET,
            "BusinessPartners?$apply=aggregate($count as N)",
            &[("accept", "application/json;odata.metadata=none")],
            "",
        )
        .await;
    assert_eq!(r.json(), json!({"value": [{"N": 4}]}));
}

async fn grouping(app: App) {
    let r = rows(
        &app,
        "groupby((Address/City),aggregate(Rating with sum as T))",
    )
    .await;
    assert_eq!(
        r,
        [
            json!({"Address": null, "T": null}),
            json!({"Address": {"City": "Berlin"}, "T": 6}),
            json!({"Address": {"City": "Paris"}, "T": 3}),
        ],
        "null groups sort first"
    );
    assert_eq!(
        rows(&app, "groupby((Active))").await,
        [json!({"Active": false}), json!({"Active": true})]
    );
    let r = rows(
        &app,
        "groupby((Address/City,Active),aggregate($count as N))",
    )
    .await;
    assert_eq!(r.len(), 3);
    assert_eq!(
        r[1],
        json!({"Address": {"City": "Berlin"}, "Active": true, "N": 2})
    );
    let r = rows(&app, "groupby((Owner/Name),aggregate($count as N))").await;
    assert_eq!(
        r,
        [
            json!({"Owner": null, "N": 2}),
            json!({"Owner": {"Name": "First Org"}, "N": 2})
        ],
        "keys can follow a to-one navigation"
    );
    let r = rows(
        &app,
        "groupby((Test.Organization/Size),aggregate($count as N))",
    )
    .await;
    assert_eq!(r.len(), 3, "keys can go through a cast: {r:?}");
    assert_eq!(rows(&app, "groupby((Founded))").await.len(), 4);
}

async fn pipelines(app: App) {
    let r = rows(
        &app,
        "filter(Active)/groupby((Address/City),aggregate(Revenue with sum as R))/filter(R gt 20)",
    )
    .await;
    assert_eq!(
        r,
        [json!({"Address": {"City": "Berlin"}, "R": 1210.5})],
        "filter on an aggregate"
    );
    let r = rows(
        &app,
        "groupby((Address/City,Abc),aggregate($count as N))/groupby((Address/City),aggregate(N with sum as Total))/orderby(Total desc)/top(1)",
    )
    .await;
    assert_eq!(
        r,
        [json!({"Address": {"City": "Berlin"}, "Total": 2})],
        "aggregate an aggregate"
    );
    let r = rows(
        &app,
        "orderby(Rating desc)/top(2)/aggregate(Rating with sum as T)",
    )
    .await;
    assert_eq!(r, [json!({"T": 8})], "top limits the rows being aggregated");
    let r = rows(&app, "orderby(Id)/skip(1)/top(1)/aggregate($count as N)").await;
    assert_eq!(r, [json!({"N": 1})]);
    let r = rows(
        &app,
        "orderby(Rating desc)/top(3)/skip(1)/aggregate(Rating with sum as T)",
    )
    .await;
    assert_eq!(r, [json!({"T": 4})], "top then skip: ratings 3 and 1");
    let r = rows(
        &app,
        "orderby(Rating desc)/skip(1)/top(1)/aggregate(Rating with sum as T)",
    )
    .await;
    assert_eq!(r, [json!({"T": 3})], "skip then top");
    let r = rows(
        &app,
        "groupby((Abc),aggregate(Rating with sum as T))/orderby(T desc)/top(2)",
    )
    .await;
    assert_eq!(
        r,
        [json!({"Abc": "A", "T": 5}), json!({"Abc": "B", "T": 3})],
        "the order of the pipeline reaches the result"
    );
    let r = rows(
        &app,
        "groupby((Abc),aggregate(Rating with sum as T))/orderby(T desc)/top(3)/skip(2)",
    )
    .await;
    assert_eq!(r, [json!({"Abc": "C", "T": 1})]);
    let r = rows(
        &app,
        "groupby((Abc),aggregate(Rating with sum as T))/top(2)/orderby(T desc)",
    )
    .await;
    assert_eq!(r.len(), 2, "orderby after top sorts what top kept");
    let r = rows(&app, "identity/aggregate($count as N)").await;
    assert_eq!(r, [json!({"N": 4})]);
}

async fn compute_step(app: App) {
    let r = rows(
        &app,
        "compute(Rating mul 2 as Twice)/aggregate(Twice with sum as T)",
    )
    .await;
    assert_eq!(r, [json!({"T": 18})]);
    let r = rows(
        &app,
        "compute(year(Founded) as Y)/groupby((Y),aggregate($count as N))",
    )
    .await;
    assert_eq!(r.len(), 4, "{r:?}");
    assert_eq!(r[1], json!({"Y": 1999, "N": 1}));
    let r = rows(
        &app,
        "groupby((Abc),aggregate(Rating with sum as T))/compute(T mul 2 as D)/filter(D gt 5)",
    )
    .await;
    assert_eq!(
        r,
        [
            json!({"Abc": "A", "T": 5, "D": 10}),
            json!({"Abc": "B", "T": 3, "D": 6})
        ]
    );
    let doc = app.ok("BusinessPartners?$apply=compute(Rating add 1 as Next)&$filter=Next gt 4&$orderby=Next desc").await;
    assert_eq!(doc["value"][0]["Id"], "1", "entities keep their properties");
    assert_eq!(doc["value"][0]["Next"], 6);
    assert_eq!(doc["value"][0]["Name"], "First Org");
    assert_eq!(
        doc["@odata.context"],
        "http://test.local/odata/$metadata#BusinessPartners(*,Next)"
    );
    assert_eq!(
        app.ids("BusinessPartners?$apply=filter(Active)&$orderby=Id")
            .await,
        ["1", "3", "4"]
    );
    assert_eq!(
        app.get("BusinessPartners/$count?$apply=filter(Active)")
            .await
            .text(),
        "3"
    );
}

async fn options_after_apply(app: App) {
    let base = "BusinessPartners?$apply=groupby((Abc),aggregate(Rating with sum as T))";
    let doc = app
        .ok(&format!(
            "{base}&$filter=T gt 2&$orderby=T desc&$count=true"
        ))
        .await;
    assert_eq!(doc["@odata.count"], 2, "$count sees the result of $apply");
    assert_eq!(doc["value"][0]["Abc"], "A");
    assert_eq!(doc["value"][1]["Abc"], "B");
    let doc = app
        .ok(&format!("{base}&$orderby=T desc&$top=1&$skip=1"))
        .await;
    assert_eq!(doc["value"][0]["Abc"], "B");
    assert_eq!(doc["value"].as_array().unwrap().len(), 1);
    let doc = app.ok(&format!("{base}&$select=T&$orderby=T")).await;
    assert!(doc["value"][0].get("Abc").is_none(), "{doc}");
    assert_eq!(
        app.get("BusinessPartners/$count?$apply=groupby((Abc))&$filter=Abc ne null")
            .await
            .text(),
        "3"
    );
}

async fn paths(app: App) {
    let doc = app
        .ok("BusinessPartners('1')/Roles?$apply=aggregate($count as N,Code with max as Top)")
        .await;
    assert_eq!(doc["value"][0]["N"], 3);
    assert_eq!(doc["value"][0]["Top"], 3);
    let doc = app
        .ok("BusinessPartners/Test.Person?$apply=aggregate($count as N)")
        .await;
    assert_eq!(doc["value"][0]["N"], 2);
    let doc = app
        .ok("Projects(20)/Partners?$apply=aggregate(Rating with sum as T)")
        .await;
    assert_eq!(doc["value"][0]["T"], 8, "through a join table");
}

async fn rejected(app: App) {
    for (query, status) in [
        ("aggregate(Nope with sum as T)", 400),
        ("aggregate(Rating with sum)", 400),
        ("aggregate(Rating as T)", 400),
        ("aggregate(Name with sum as T)", 400),
        ("aggregate(Active with max as T)", 400),
        ("aggregate(Rating with sum as Rating)", 400),
        ("aggregate(Rating with sum as T,Score with sum as T)", 400),
        ("aggregate(Rating with sum as 1x)", 400),
        ("groupby((Address))", 400),
        ("groupby((Roles/Code))", 400),
        ("groupby(Abc)", 400),
        ("groupby((Abc),aggregate($count as Abc))", 400),
        ("groupby((Abc))/filter(Name eq 'a')", 400),
        ("groupby((Abc))/search(x)", 400),
        ("filter(Rating)", 400),
        ("nope(1)", 400),
        ("search(x)", 400),
        (
            "aggregate(Rating with sum as T;DROP TABLE business_partner)",
            400,
        ),
        ("aggregate(Rating with sum as T)/DROP", 400),
        ("concat(identity,identity)", 501),
        ("groupby((rollup(Abc,Active)))", 501),
        ("aggregate(Rating with custom.median as T)", 501),
        ("aggregate(Rating with sum from Abc as T)", 501),
        ("aggregate(Roles/$count as N)", 501),
        ("top(1)", 501),
        ("orderby(Id)", 501),
        ("top(1)/filter(Active)/aggregate($count as N)", 501),
        ("top(1)/orderby(Id)/aggregate($count as N)", 501),
    ] {
        assert_eq!(
            app.status(&format!("BusinessPartners?$apply={query}"))
                .await,
            status,
            "{query}"
        );
    }
    assert_eq!(
        app.status("BusinessPartners('1')?$apply=aggregate($count as N)")
            .await,
        400
    );
    assert_eq!(
        app.status("BusinessPartners?$apply=groupby((Abc))&$orderby=Name")
            .await,
        400
    );
    assert_eq!(
        app.status("BusinessPartners?$apply=groupby((Abc))&$select=Name")
            .await,
        400
    );
    assert_eq!(
        app.status("BusinessPartners?$apply=groupby((Abc))&$expand=Roles")
            .await,
        501
    );
    assert_eq!(
        app.status("BusinessPartners?$expand=Roles($apply=aggregate($count as N))")
            .await,
        501
    );
    assert_eq!(
        app.ids("BusinessPartners").await.len(),
        4,
        "the table survived"
    );
}

mod run {
    use super::common;
    use odatabase::{Database, Service};

    fn build() -> impl FnOnce(Database) -> Service {
        |db| common::builder().build(db).unwrap()
    }

    crate::scenarios!(build();
        aggregate_methods,
        annotations_and_context,
        grouping,
        pipelines,
        compute_step,
        options_after_apply,
        paths,
        rejected,
    );
}

#[tokio::test]
async fn paging_keeps_the_pipeline() {
    let app = common::sqlite(|db| {
        common::builder()
            .paging(OffsetPaging { page_size: Some(2) })
            .build(db)
            .unwrap()
    })
    .await;
    let doc = app.ok("BusinessPartners?$apply=groupby((Abc))").await;
    assert_eq!(doc["value"].as_array().unwrap().len(), 2);
    let next = doc["@odata.nextLink"].as_str().unwrap().to_owned();
    assert!(next.contains("$apply="), "{next}");
    let doc = app
        .ok(next.trim_start_matches("http://test.local/odata/"))
        .await;
    let values: Vec<&Value> = doc["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["Abc"])
        .collect();
    assert_eq!(
        values,
        [&json!("B"), &json!("C")],
        "null sorts first, on the first page"
    );
}
