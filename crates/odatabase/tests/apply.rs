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

/// Rows of `$apply` that keep their entities, by key.
async fn kept(app: &App, query: &str) -> Vec<String> {
    app.ids(&format!("BusinessPartners?$apply={query}")).await
}

async fn entity_narrowing(app: App) {
    assert_eq!(kept(&app, "top(2)").await, ["1", "2"], "keys order top");
    assert_eq!(kept(&app, "skip(3)").await, ["4"]);
    assert_eq!(kept(&app, "skip(1)/top(2)").await, ["2", "3"]);
    assert_eq!(kept(&app, "top(3)/skip(1)").await, ["2", "3"]);
    assert_eq!(kept(&app, "top(2)/top(5)").await, ["1", "2"]);
    assert_eq!(
        kept(&app, "orderby(Name desc)/top(2)").await,
        ["3", "2"],
        "the order reaches the result"
    );
    assert_eq!(
        kept(&app, "orderby(Rating desc)/filter(Active)/top(1)").await,
        ["1"]
    );
    assert_eq!(
        kept(&app, "top(2)/filter(Active)").await,
        ["1"],
        "filter acts on what top kept"
    );
    assert_eq!(
        kept(&app, "orderby(Rating)/top(2)/orderby(Id desc)").await,
        ["4", "3"],
        "the last sort wins, over the rows the first one chose"
    );
    let doc = app
        .ok("BusinessPartners?$apply=orderby(Rating desc)/top(2)&$count=true&$orderby=Id desc")
        .await;
    assert_eq!(doc["@odata.count"], 2);
    assert_eq!(doc["value"][0]["Id"], "2", "$orderby comes last");
    let doc = app
        .ok("BusinessPartners?$apply=compute(Rating mul 2 as D)/top(1)&$select=Id,D")
        .await;
    assert_eq!(doc["value"][0]["D"], 10, "compute survives narrowing");
    let doc = app
        .ok("BusinessPartners?$apply=orderby(Rating desc)/top(1)&$expand=Roles($select=Code)")
        .await;
    assert_eq!(doc["value"][0]["Roles"].as_array().unwrap().len(), 3);
    let doc = app
        .ok("Roles?$apply=orderby(Code desc,PartnerId desc)/skip(1)/top(1)")
        .await;
    assert_eq!(doc["value"][0]["PartnerId"], "3", "a composite key");
    assert_eq!(doc["value"].as_array().unwrap().len(), 1);
    assert_eq!(doc["value"][0]["Code"], 2);
    let doc = app
        .ok("BusinessPartners('1')/Roles?$apply=orderby(Code desc)/top(1)")
        .await;
    assert_eq!(doc["value"][0]["Code"], 3, "from a navigation path");
    let r = rows(&app, "top(2)/aggregate(Rating with sum as T)").await;
    assert_eq!(r, [json!({"T": 8})]);
    let r = rows(&app, "groupby((Abc))/skip(1)/top(1)").await;
    assert_eq!(r, [json!({"Abc": "A"})], "null groups sort first");
}

async fn ranks(app: App) {
    assert_eq!(kept(&app, "topcount(2,Rating)").await, ["1", "2"]);
    assert_eq!(kept(&app, "bottomcount(1,Rating)").await, ["4"]);
    assert_eq!(
        kept(&app, "bottomcount(9,Rating)").await,
        ["4", "2", "1"],
        "rows without a value have no rank"
    );
    assert_eq!(kept(&app, "topsum(5,Rating)").await, ["1"]);
    assert_eq!(
        kept(&app, "topsum(6,Rating)").await,
        ["1", "2"],
        "up to the row that reaches the amount"
    );
    assert_eq!(kept(&app, "bottomsum(4,Rating)").await, ["4", "2"]);
    assert_eq!(
        kept(&app, "toppercent(50,Rating)").await,
        ["1"],
        "5 of 9 reaches half"
    );
    assert_eq!(
        kept(&app, "bottompercent(50,Rating)").await,
        ["4", "2", "1"],
        "1 and 3 of 9 fall short of half"
    );
    assert_eq!(
        kept(&app, "filter(Id ne '1')/topcount(1,Rating)").await,
        ["2"]
    );
    assert_eq!(
        kept(&app, "topcount(2,Rating)/filter(Id ne '1')").await,
        ["2"]
    );
    assert_eq!(kept(&app, "topcount(2,Rating mul -1)").await, ["4", "2"]);
    let groups = "groupby((Abc),aggregate(Rating with sum as T))";
    let r = rows(&app, &format!("{groups}/topcount(1,T)")).await;
    assert_eq!(r, [json!({"Abc": "A", "T": 5})]);
    let r = rows(&app, &format!("{groups}/topsum(6,T)")).await;
    assert_eq!(
        r,
        [json!({"Abc": "A", "T": 5}), json!({"Abc": "B", "T": 3})]
    );
    let r = rows(&app, &format!("{groups}/bottompercent(10,T)")).await;
    assert_eq!(r, [json!({"Abc": "C", "T": 1})]);
    let r = rows(
        &app,
        &format!("{groups}/topsum(6,T)/aggregate(T with sum as All)"),
    )
    .await;
    assert_eq!(r, [json!({"All": 8})]);
}

async fn concatenation(app: App) {
    let groups = "groupby((Abc),aggregate(Rating with sum as T))";
    let r = rows(
        &app,
        &format!("filter(Abc ne null)/concat({groups},aggregate(Rating with sum as T))"),
    )
    .await;
    assert_eq!(r.len(), 4, "{r:?}");
    assert_eq!(r[0]["Abc"], "A");
    assert_eq!(r[2]["T"], 1);
    assert_eq!(r[3]["T"], 9, "the total comes after its groups");
    assert!(r[3]["Abc"].is_null());

    let r = rows(
        &app,
        "concat(filter(Id eq '2'),aggregate(Rating with sum as T))",
    )
    .await;
    assert_eq!(r.len(), 2);
    assert_eq!(r[0]["Name"], "Second Org", "entities keep their properties");
    assert!(r[0]["T"].is_null());
    assert_eq!(r[1]["T"], 9);

    let doc = app
        .ok("BusinessPartners?$apply=concat(orderby(Rating desc)/top(1),orderby(Id desc)/top(1))")
        .await;
    let ids: Vec<&Value> = doc["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["Id"])
        .collect();
    assert_eq!(ids, [&json!("1"), &json!("4")], "each arm keeps its order");

    let twice = "concat(filter(Active),filter(not Active))";
    let r = rows(&app, &format!("{twice}/aggregate($count as N)")).await;
    assert_eq!(r, [json!({"N": 4})]);
    let r = rows(
        &app,
        "concat(identity,identity)/groupby((Id),aggregate($count as N))/top(1)",
    )
    .await;
    assert_eq!(r, [json!({"Id": "1", "N": 2})], "no duplicates are removed");
    let filtered = app
        .ok(
            "BusinessPartners?$apply=concat(groupby((Abc)),groupby((Active)))\
             &$filter=Abc eq 'A'&$count=true",
        )
        .await;
    assert_eq!(filtered["@odata.count"], 1);
    let page = app
        .ok("BusinessPartners?$apply=concat(groupby((Abc)),groupby((Active)))&$top=2&$skip=3")
        .await;
    assert_eq!(page["value"][0]["Abc"], "C", "paging follows the arms");
    assert_eq!(page["value"][1]["Active"], false);
    let ordered = app
        .ok("BusinessPartners?$apply=concat(groupby((Abc)),aggregate($count as N))&$orderby=Abc desc&$top=1")
        .await;
    assert_eq!(ordered["value"][0]["Abc"], "C");
    let filter_after = rows(
        &app,
        "concat(aggregate($count as N),groupby((Abc),aggregate($count as N)))/filter(N gt 1)",
    )
    .await;
    assert_eq!(filter_after.len(), 1);
    assert_eq!(filter_after[0]["N"], 4);
}

async fn rollups(app: App) {
    let r = rows(&app, "groupby((rollup(Abc,Active)),aggregate($count as N))").await;
    assert_eq!(r.len(), 8, "{r:?}");
    let r = rows(
        &app,
        "filter(Abc eq 'A')/groupby((rollup($all,Abc)),aggregate(Rating with sum as T))",
    )
    .await;
    assert_eq!(r.len(), 2, "{r:?}");
    assert!(r.iter().all(|row| row["T"] == 5));
    assert!(r.iter().any(|row| row["Abc"] == "A"));
    let r = rows(
        &app,
        "filter(Abc eq 'A')/groupby((Active,rollup($all,Abc)),aggregate($count as N))",
    )
    .await;
    assert_eq!(r.len(), 2, "{r:?}");
    assert!(r.iter().all(|row| row["Active"] == true && row["N"] == 1));
}

async fn from_and_navigation_counts(app: App) {
    let r = rows(
        &app,
        "aggregate(Rating with sum from Abc with average as Mean)",
    )
    .await;
    assert_eq!(r[0]["Mean"].as_f64(), Some(3.0), "the sums are 5, 3 and 1");
    let r = rows(&app, "aggregate(Rating with sum from Abc with max as High)").await;
    assert_eq!(r, [json!({"High": 5})]);
    let r = rows(&app, "aggregate(Roles/$count as N)").await;
    assert_eq!(r, [json!({"N": 5})]);
    let r = rows(
        &app,
        "groupby((Abc),aggregate(Roles/$count as N))/filter(Abc ne null)",
    )
    .await;
    assert_eq!(
        r,
        [
            json!({"Abc": "A", "N": 3}),
            json!({"Abc": "B", "N": 1}),
            json!({"Abc": "C", "N": 0})
        ]
    );
}

async fn joins(app: App) {
    let r = rows(&app, "join(Roles as R)/aggregate($count as N)").await;
    assert_eq!(r, [json!({"N": 5})]);
    let r = rows(&app, "outerjoin(Roles as R)/aggregate($count as N)").await;
    assert_eq!(r, [json!({"N": 6})], "partner 4 has no roles");
    let r = rows(
        &app,
        "join(Roles as R)/groupby((R/Code),aggregate($count as N))",
    )
    .await;
    assert_eq!(
        r,
        [
            json!({"R": {"Code": 1}, "N": 2}),
            json!({"R": {"Code": 2}, "N": 2}),
            json!({"R": {"Code": 3}, "N": 1})
        ]
    );
    let r = rows(&app, "join(Roles as R)/filter(R/Code eq 1)/groupby((Id))").await;
    assert_eq!(r, [json!({"Id": "1"}), json!({"Id": "2"})]);
    let r = rows(
        &app,
        "outerjoin(Roles as R)/filter(R/Code eq null)/groupby((Id))",
    )
    .await;
    assert_eq!(r, [json!({"Id": "4"})]);
    let r = rows(
        &app,
        "join(Projects as P)/groupby((P/Title),aggregate($count as N))",
    )
    .await;
    assert_eq!(
        r,
        [
            json!({"P": {"Title": "Alpha"}, "N": 1}),
            json!({"P": {"Title": "Beta"}, "N": 2})
        ],
        "many to many goes through the join table"
    );
    let r = rows(
        &app,
        "join(Roles as R)/join(R/Partner/Projects as P)/aggregate($count as N)",
    )
    .await;
    assert_eq!(r, [json!({"N": 7})], "2 projects x 3 roles, and 1 x 1");
}

async fn expansion(app: App) {
    let doc = app
        .ok("BusinessPartners?$apply=filter(Id eq '1')/expand(Roles,filter(Code gt 1))")
        .await;
    assert_eq!(doc["value"][0]["Roles"].as_array().unwrap().len(), 2);
    let doc = app
        .ok("BusinessPartners?$apply=filter(Id eq '1')&$expand=Roles($orderby=Code desc)")
        .await;
    assert_eq!(doc["value"][0]["Roles"][0]["Code"], 3);
    let doc = app
        .ok("BusinessPartners?$expand=Roles($apply=filter(Code eq 1))&$filter=Id eq '1'")
        .await;
    assert_eq!(doc["value"][0]["Roles"].as_array().unwrap().len(), 1);
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
        ("concat(identity)", 400),
        (
            "concat(aggregate($count as A),aggregate(Name with min as A))",
            400,
        ),
        ("topcount(1,Address)", 400),
        ("topcount(x,Rating)", 400),
        ("topsum(1,Name)", 400),
        ("join(Roles)", 400),
        ("join(Name as N)", 400),
        ("join(Roles as Name)", 400),
        ("join(Roles as R)", 501),
        ("concat(join(Roles as R),identity)", 501),
        ("expand(Roles)/aggregate($count as N)", 501),
        ("concat(expand(Roles),identity)", 501),
        ("nest(aggregate($count as N) as X)", 501),
        ("traverse(H,N,Id,preorder)", 501),
        ("aggregate(Rating with custom.median as T)", 501),
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
        400
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
        entity_narrowing,
        ranks,
        concatenation,
        rollups,
        from_and_navigation_counts,
        joins,
        expansion,
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
    let doc = app
        .ok("BusinessPartners?$apply=orderby(Rating desc)/top(3)")
        .await;
    let first: Vec<&Value> = doc["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["Id"])
        .collect();
    assert_eq!(first, [&json!("1"), &json!("2")]);
    let next = doc["@odata.nextLink"].as_str().unwrap().to_owned();
    let doc = app
        .ok(next.trim_start_matches("http://test.local/odata/"))
        .await;
    let last: Vec<&Value> = doc["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["Id"])
        .collect();
    assert_eq!(last, [&json!("4")], "the pages follow the pipeline's order");
}
