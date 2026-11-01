mod common;

use axum::http::{Method, StatusCode};
use common::App;
use odatabase::OffsetPaging;
use serde_json::json;

async fn entity_sets_and_keys(app: App) {
    assert_eq!(app.ids("BusinessPartners").await, ["1", "2", "3", "4"]);
    let doc = app.ok("BusinessPartners('1')").await;
    assert_eq!(doc["Id"], "1");
    assert_eq!(doc["Name"], "First Org");
    assert_eq!(doc["Rating"], 5);
    assert_eq!(doc["Active"], true);
    assert_eq!(doc["Abc"], "A");
    assert_eq!(doc["Flags"], "Read,Write");
    assert_eq!(doc["Revenue"], json!(1200.5));
    assert_eq!(doc["Score"], json!(1.5));
    assert_eq!(doc["Founded"], "1999-03-01");
    assert_eq!(doc["CreatedAt"], "2024-01-15T08:30:00Z");
    assert_eq!(doc["ExternalId"], "01234567-89ab-cdef-0123-456789abcdef");
    assert_eq!(
        doc["Address"],
        json!({"Street": "Main 1", "City": "Berlin", "Country": "DEU"})
    );
    assert_eq!(
        doc["Comments"],
        json!(["first", "second"]),
        "ordered by position"
    );
    assert_eq!(doc["Phones"][1], json!({"Kind": "home", "Number": "222"}));
    assert_eq!(
        doc["@odata.context"],
        "http://test.local/odata/$metadata#BusinessPartners/$entity"
    );
    assert_eq!(doc["@odata.etag"], "W/\"3\"");
    assert_eq!(doc["@odata.type"], "#Test.Organization", "derived instance");
    assert_eq!(doc["Size"], 100, "derived property");
    let doc = app.ok("Roles(PartnerId='1',Code=2)").await;
    assert_eq!(doc["Label"], "Seller");
    let doc = app.ok("BusinessPartners('3')").await;
    assert_eq!(
        doc["Address"],
        json!(null),
        "an empty embedded type is null"
    );
    assert_eq!(doc["Rating"], json!(null));
    assert_eq!(doc["Comments"], json!([]));
    assert_eq!(app.status("BusinessPartners('nope')").await, 404);
    assert_eq!(app.status("Nope").await, 404);
}

async fn singletons_and_documents(app: App) {
    let doc = app.ok("Settings").await;
    assert_eq!(doc["Theme"], "dark");
    assert_eq!(
        doc["@odata.context"],
        "http://test.local/odata/$metadata#Settings"
    );
    let doc = app.ok("").await;
    assert_eq!(doc["@odata.context"], "http://test.local/odata/$metadata");
    let without_slash = axum::http::Request::get("/odata")
        .header("host", "test.local")
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(app.send(without_slash).await.status, StatusCode::OK);
    let names: Vec<&str> = doc["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"BusinessPartners"));
    assert!(names.contains(&"Settings"));
    let r = app.get("$metadata").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-type"), Some("application/xml"));
    assert!(r.text().contains("<EntitySet Name=\"BusinessPartners\""));
    let r = app
        .request(
            Method::GET,
            "$metadata",
            &[("accept", "application/json")],
            "",
        )
        .await;
    assert_eq!(r.json()["$EntityContainer"], "Test.Container");
    assert_eq!(app.get("").await.header("odata-version"), Some("4.0"));
}

async fn filter_comparisons(app: App) {
    let cases = [
        ("Rating gt 2", vec!["1", "2"]),
        ("Rating ge 3 and Rating le 3", vec!["2"]),
        ("Rating lt 2 or Rating eq 5", vec!["1", "4"]),
        ("Rating eq null", vec!["3"]),
        ("Rating ne null", vec!["1", "2", "4"]),
        ("Rating ne 5", vec!["2", "3", "4"]),
        ("not (Rating eq 5)", vec!["2", "3", "4"]),
        ("not (Rating gt 2)", vec!["4"]),
        ("Name eq 'Third O''Neil'", vec!["3"]),
        ("Active eq false", vec!["2"]),
        ("Active", vec!["1", "3", "4"]),
        ("not Active", vec!["2"]),
        ("Revenue gt 100.5", vec!["1"]),
        ("Score lt 2", vec!["1", "4"]),
        ("Founded gt 2000-01-01", vec!["2", "4"]),
        ("CreatedAt ge 2024-01-15T08:30:00Z", vec!["1", "2"]),
        ("CreatedAt lt 2024-01-15T09:30:00%2B02:00", vec!["4"]),
        (
            "ExternalId eq 01234567-89ab-cdef-0123-456789abcdef",
            vec!["1"],
        ),
        ("Abc eq Test.Abc'B'", vec!["2"]),
        ("Abc eq 'A'", vec!["1"]),
        ("Flags has Test.Access'Write'", vec!["1"]),
        ("Rating in (1,5)", vec!["1", "4"]),
        ("Name in ('Fourth')", vec!["4"]),
        ("Rating in ()", vec![]),
        ("Rating add 1 eq 6", vec!["1"]),
        ("Rating mul 2 sub 1 eq 5", vec!["2"]),
        ("Rating div 2 eq 2", vec!["1"]),
        ("Rating mod 2 eq 1", vec!["1", "2", "4"]),
        ("Rating divby 2 eq 2.5", vec!["1"]),
        ("-Rating lt -4", vec!["1"]),
        ("Rating eq Rating", vec!["1", "2", "3", "4"]),
        ("Address/City eq 'Berlin'", vec!["1", "4"]),
        ("Address/City ne 'Berlin'", vec!["2", "3"]),
        ("Id eq '1'", vec!["1"]),
        ("$it/Id eq '1'", vec!["1"]),
        ("Test.Organization/Size gt 50", vec!["1"]),
        ("isof(Test.Person)", vec!["3", "4"]),
    ];
    for (filter, expected) in cases {
        assert_eq!(
            app.ids(&format!("BusinessPartners?$filter={filter}")).await,
            expected,
            "{filter}"
        );
    }
}

async fn filter_functions(app: App) {
    let cases = [
        ("contains(Name,'Org')", vec!["1", "2"]),
        ("contains(Name,'org')", vec![]),
        ("startswith(Name,'First')", vec!["1"]),
        ("startswith(Name,'first')", vec![]),
        ("endswith(Name,'Org')", vec!["1", "2"]),
        ("endswith(Name,'')", vec!["1", "2", "3", "4"]),
        ("length(Name) eq 6", vec!["4"]),
        ("indexof(Name,'Org') eq 6", vec!["1"]),
        ("substring(Name,1) eq 'irst Org'", vec!["1"]),
        ("substring(Name,1,4) eq 'irst'", vec!["1"]),
        ("tolower(Name) eq 'fourth'", vec!["4"]),
        ("toupper(Name) eq 'FOURTH'", vec!["4"]),
        ("trim(concat('  ',Name)) eq 'Fourth'", vec!["4"]),
        ("concat(Name,'!') eq 'Fourth!'", vec!["4"]),
        ("year(Founded) eq 1999", vec!["1"]),
        ("month(Founded) eq 2", vec!["4"]),
        ("day(Founded) eq 20", vec!["2"]),
        ("year(CreatedAt) eq 2023", vec!["4"]),
        ("hour(CreatedAt) eq 8", vec!["1"]),
        ("minute(CreatedAt) eq 59", vec!["4"]),
        ("second(CreatedAt) eq 59", vec!["4"]),
        ("round(Score) eq 2", vec!["1"]),
        ("floor(Score) eq 2", vec!["2"]),
        ("ceiling(Score) eq 1", vec!["4"]),
        ("CreatedAt lt now()", vec!["1", "2", "4"]),
        ("cast(Rating,Edm.String) eq '5'", vec!["1"]),
    ];
    for (filter, expected) in cases {
        assert_eq!(
            app.ids(&format!("BusinessPartners?$filter={filter}")).await,
            expected,
            "{filter}"
        );
    }
}

async fn filter_over_navigation(app: App) {
    let cases = [
        ("Owner/Name eq 'First Org'", vec!["2", "3"]),
        ("Owner/Address/City eq 'Berlin'", vec!["2", "3"]),
        ("Owner/Owner/Id eq '1'", vec![]),
        ("Owner/Id eq null", vec!["1", "4"]),
        ("Owner eq null", vec!["1", "4"]),
        ("Owner ne null", vec!["2", "3"]),
        ("Roles/$count gt 1", vec!["1"]),
        ("Roles/$count eq 0", vec!["4"]),
        ("Comments/$count eq 1", vec!["2"]),
        ("Projects/$count eq 2", vec!["1"]),
        ("Roles/any()", vec!["1", "2", "3"]),
        ("not Roles/any()", vec!["4"]),
        ("Roles/any(r: r/Code eq 3)", vec!["1"]),
        ("Roles/any(r: r/Label eq 'Buyer')", vec!["1", "2"]),
        ("Roles/all(r: r/Label eq 'Buyer')", vec!["2", "4"]),
        ("Roles/all(r: r/Code gt 0)", vec!["1", "2", "3", "4"]),
        ("Comments/any(c: c eq 'only')", vec!["2"]),
        ("Comments/any(c: contains(c,'ir'))", vec!["1"]),
        ("Phones/any(p: p/Kind eq 'home')", vec!["1"]),
        ("Projects/any(p: p/Title eq 'Beta')", vec!["1", "2"]),
        (
            "Projects/any(p: p/Partners/any(q: q/Id eq '2'))",
            vec!["1", "2"],
        ),
        (
            "Roles/any(r: r/Partner/Rating eq 5 and r/Code eq 1)",
            vec!["1"],
        ),
        ("Owner/Roles/any(r: r/Code eq 3)", vec!["2", "3"]),
    ];
    for (filter, expected) in cases {
        assert_eq!(
            app.ids(&format!("BusinessPartners?$filter={filter}")).await,
            expected,
            "{filter}"
        );
    }
}

async fn select_order_page(app: App) {
    let doc = app
        .ok("BusinessPartners?$select=Name,Address/City&$top=1")
        .await;
    let item = &doc["value"][0];
    assert_eq!(item["Name"], "First Org");
    assert_eq!(item["Id"], "1", "keys stay selected");
    assert_eq!(item["Address"], json!({"City": "Berlin"}));
    assert!(item.get("Rating").is_none());
    assert!(item.get("Comments").is_none());
    assert_eq!(
        doc["@odata.context"],
        "http://test.local/odata/$metadata#BusinessPartners(Name,Address/City)"
    );
    let doc = app.ok("BusinessPartners('1')?$select=Comments").await;
    assert_eq!(doc["Comments"], json!(["first", "second"]));
    assert!(doc.get("Name").is_none());

    assert_eq!(
        app.ids("BusinessPartners?$orderby=Name desc").await,
        ["3", "2", "4", "1"]
    );
    assert_eq!(
        app.ids("BusinessPartners?$orderby=Address/City,Id desc")
            .await,
        ["3", "4", "1", "2"]
    );
    assert_eq!(
        app.ids("BusinessPartners?$orderby=Address/City desc,Id")
            .await,
        ["2", "1", "4", "3"]
    );
    assert_eq!(
        app.ids("BusinessPartners?$orderby=Roles/$count desc&$top=1")
            .await,
        ["1"]
    );
    assert_eq!(
        app.ids("BusinessPartners?$orderby=Owner/Name desc,Id&$top=2&$filter=Owner ne null")
            .await
            .len(),
        2
    );
    assert_eq!(app.ids("BusinessPartners?$top=2&$skip=1").await, ["2", "3"]);
    assert_eq!(app.ids("BusinessPartners?$skip=3").await, ["4"]);
    assert_eq!(app.ids("BusinessPartners?$top=0").await.len(), 0);
    assert_eq!(
        app.ids("BusinessPartners?$top=9223372036854775807")
            .await
            .len(),
        4
    );
    assert_eq!(
        app.ids("BusinessPartners?$skip=9223372036854775807")
            .await
            .len(),
        0
    );
    let huge = "BusinessPartners?$expand=Roles($top=9223372036854775807;$skip=9223372036854775807)";
    assert_eq!(
        app.status(huge).await,
        200,
        "extreme windows do not overflow"
    );
    assert_eq!(
        app.status("BusinessPartners?$top=9223372036854775808")
            .await,
        400
    );
    let doc = app
        .ok("BusinessPartners?$count=true&$top=1&$filter=Active")
        .await;
    assert_eq!(doc["@odata.count"], 3);
    assert_eq!(doc["value"].as_array().unwrap().len(), 1);
    assert_eq!(app.get("BusinessPartners/$count").await.text(), "4");
    assert_eq!(
        app.get("BusinessPartners/$count?$filter=Active")
            .await
            .text(),
        "3"
    );
    assert_eq!(
        app.get("BusinessPartners('1')/Roles/$count").await.text(),
        "3"
    );
    assert_eq!(
        app.get("BusinessPartners('1')/Comments/$count")
            .await
            .text(),
        "2"
    );
}

async fn paths_and_properties(app: App) {
    assert_eq!(
        app.column("BusinessPartners('1')/Roles", "Code").await,
        [1, 2, 3]
    );
    assert_eq!(
        app.column(
            "BusinessPartners('1')/Roles?$filter=Code gt 1&$orderby=Code desc",
            "Code"
        )
        .await,
        [3, 2]
    );
    assert_eq!(
        app.ok("BusinessPartners('1')/Roles(PartnerId='1',Code=2)")
            .await["Label"],
        "Seller"
    );
    assert_eq!(
        app.ok("Roles(PartnerId='2',Code=1)/Partner").await["Id"],
        "2"
    );
    assert_eq!(
        app.ok("Roles(PartnerId='2',Code=1)/Partner/Owner").await["Id"],
        "1"
    );
    assert_eq!(
        app.column("BusinessPartners('1')/Projects", "Title").await,
        ["Alpha", "Beta"]
    );
    assert_eq!(app.column("Projects(20)/Partners", "Id").await, ["1", "2"]);
    assert_eq!(
        app.status("BusinessPartners('1')/Owner").await,
        204,
        "no related entity"
    );
    assert_eq!(
        app.status("BusinessPartners('nope')/Roles(PartnerId='nope',Code=1)")
            .await,
        404
    );
    assert_eq!(app.ids("BusinessPartners('nope')/Roles").await.len(), 0);

    let doc = app.ok("BusinessPartners('1')/Name").await;
    assert_eq!(doc["value"], "First Org");
    assert_eq!(
        doc["@odata.context"],
        "http://test.local/odata/$metadata#BusinessPartners('1')/Name"
    );
    assert_eq!(
        app.status("BusinessPartners('3')/Rating").await,
        204,
        "null property"
    );
    assert_eq!(app.status("BusinessPartners('nope')/Rating").await, 404);
    assert_eq!(
        app.ok("BusinessPartners('1')/Address").await["City"],
        "Berlin"
    );
    assert_eq!(
        app.ok("BusinessPartners('1')/Address/City").await["value"],
        "Berlin"
    );
    assert_eq!(
        app.ok("BusinessPartners('1')/Comments").await["value"],
        json!(["first", "second"])
    );
    assert_eq!(
        app.ok("BusinessPartners('1')/Phones").await["value"][0]["Number"],
        "111"
    );
    assert_eq!(
        app.get("BusinessPartners('1')/Name/$value").await.text(),
        "First Org"
    );
    assert_eq!(app.status("BusinessPartners('3')/Rating/$value").await, 204);

    let doc = app.ok("BusinessPartners('1')/Roles/$ref").await;
    assert_eq!(
        doc["value"][0]["@odata.id"],
        "http://test.local/odata/Roles(PartnerId='1',Code=1)"
    );
    let doc = app.ok("Roles(PartnerId='2',Code=1)/Partner/$ref").await;
    assert_eq!(
        doc["@odata.id"],
        "http://test.local/odata/BusinessPartners('2')"
    );
}

async fn casts(app: App) {
    let doc = app.ok("BusinessPartners/Test.Person").await;
    assert_eq!(
        doc["@odata.context"],
        "http://test.local/odata/$metadata#BusinessPartners/Test.Person"
    );
    let doc = app.ok("BusinessPartners('3')/Test.Person").await;
    assert_eq!(
        doc["@odata.context"],
        "http://test.local/odata/$metadata#BusinessPartners/Test.Person/$entity"
    );
    assert_eq!(app.ids("BusinessPartners/Test.Person").await, ["3", "4"]);
    assert_eq!(
        app.ok("BusinessPartners('3')/Test.Person").await["FirstName"],
        "Ann"
    );
    assert_eq!(app.status("BusinessPartners('1')/Test.Person").await, 404);
    assert_eq!(
        app.ok("BusinessPartners('3')/Test.Person/FirstName").await["value"],
        "Ann"
    );
    assert_eq!(
        app.get("BusinessPartners/Test.Organization/$count")
            .await
            .text(),
        "2"
    );
    let doc = app
        .ok("BusinessPartners?$select=Id,Test.Person/FirstName&$orderby=Id")
        .await;
    assert_eq!(doc["value"][2]["FirstName"], "Ann");
    assert!(doc["value"][0].get("FirstName").is_none());
}

async fn metadata_levels_and_formats(app: App) {
    let full = "application/json;odata.metadata=full";
    let r = app
        .request(
            Method::GET,
            "BusinessPartners('1')",
            &[("accept", full)],
            "",
        )
        .await;
    let doc = r.json();
    assert_eq!(doc["@odata.type"], "#Test.Organization");
    assert_eq!(
        doc["@odata.id"],
        "http://test.local/odata/BusinessPartners('1')"
    );
    assert_eq!(
        doc["Roles@odata.navigationLink"],
        "BusinessPartners('1')/Roles"
    );
    assert_eq!(doc["Rating@odata.type"], "#Int32");
    let none = "application/json;odata.metadata=none";
    let doc = app
        .request(
            Method::GET,
            "BusinessPartners('1')",
            &[("accept", none)],
            "",
        )
        .await
        .json();
    assert!(doc.get("@odata.context").is_none());
    assert!(doc.get("@odata.etag").is_none());
    let ieee = "application/json;IEEE754Compatible=true";
    let doc = app
        .request(
            Method::GET,
            "BusinessPartners('1')",
            &[("accept", ieee)],
            "",
        )
        .await
        .json();
    assert_eq!(doc["Version"], "3");
    assert_eq!(doc["Revenue"], "1200.5");
    let r = app
        .request(
            Method::GET,
            "BusinessPartners",
            &[("accept", "application/xml")],
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_ACCEPTABLE);
    assert_eq!(
        app.ok("BusinessPartners('1')?$format=json").await["Id"],
        "1"
    );
    assert_eq!(app.status("BusinessPartners?$format=atom").await, 406);
    let doc = app.get("Nope").await.json();
    assert_eq!(doc["error"]["code"], "404");
    let r = app
        .request(Method::GET, "", &[("odata-maxversion", "3.0")], "")
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

async fn preconditions(app: App) {
    let with = |h: &'static str, v: &'static str| (h, v);
    let r = app
        .request(Method::GET, "BusinessPartners('1')", &[], "")
        .await;
    assert_eq!(r.header("etag"), Some("W/\"3\""));
    let r = app
        .request(
            Method::GET,
            "BusinessPartners('1')",
            &[with("if-none-match", "W/\"3\"")],
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
    assert!(r.body.is_empty());
    let r = app
        .request(
            Method::GET,
            "BusinessPartners('1')",
            &[with("if-none-match", "W/\"2\"")],
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = app
        .request(
            Method::GET,
            "BusinessPartners('1')",
            &[with("if-none-match", "*")],
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
    let r = app
        .request(
            Method::GET,
            "BusinessPartners('1')",
            &[with("if-match", "W/\"2\"")],
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::PRECONDITION_FAILED);
}

async fn injection_is_inert(app: App) {
    let attacks = [
        "'; DROP TABLE business_partner; --",
        "' OR '1'='1",
        "\" OR 1=1 --",
        "\\' OR 1=1 --",
        "%' OR '%'='",
        "x') OR ('1'='1",
    ];
    for attack in attacks {
        let quoted = attack.replace('\'', "''").replace('%', "%25");
        let u = quoted
            .replace(' ', "%20")
            .replace('#', "%23")
            .replace(';', "%3B");
        assert_eq!(
            app.ids(&format!("BusinessPartners?$filter=Name eq '{u}'"))
                .await
                .len(),
            0,
            "{attack}"
        );
        assert_eq!(
            app.ids(&format!("BusinessPartners?$filter=contains(Name,'{u}')"))
                .await
                .len(),
            0
        );
        assert_eq!(app.status(&format!("BusinessPartners('{u}')")).await, 404);
        assert_eq!(
            app.ids(&format!("BusinessPartners('{u}')/Roles"))
                .await
                .len(),
            0
        );
    }
    for option in [
        "$orderby=Name;DROP TABLE business_partner",
        "$orderby=Name desc, (select 1)",
        "$select=Name;DROP",
        "$select=* from business_partner --",
        "$filter=1 eq 1;DROP TABLE x",
        "$expand=Roles;DROP",
        "$filter=Name eq 'a' UNION SELECT 1",
    ] {
        assert_eq!(
            app.status(&format!("BusinessPartners?{option}")).await,
            400,
            "{option}"
        );
    }
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
        entity_sets_and_keys,
        singletons_and_documents,
        filter_comparisons,
        filter_functions,
        filter_over_navigation,
        select_order_page,
        paths_and_properties,
        casts,
        metadata_levels_and_formats,
        preconditions,
        injection_is_inert,
    );
}

#[tokio::test]
async fn server_driven_paging() {
    let app = common::sqlite(|db| {
        common::builder()
            .paging(OffsetPaging { page_size: Some(3) })
            .build(db)
            .unwrap()
    })
    .await;
    let doc = app
        .ok("BusinessPartners?$filter=Active eq true or Active eq false&$orderby=Id")
        .await;
    assert_eq!(doc["value"].as_array().unwrap().len(), 3);
    let next = doc["@odata.nextLink"].as_str().unwrap().to_owned();
    assert!(
        next.starts_with("http://test.local/odata/BusinessPartners?"),
        "{next}"
    );
    assert!(next.contains("$skiptoken=3"), "{next}");
    let relative = next.trim_start_matches("http://test.local/odata/");
    let doc = app.ok(relative).await;
    assert_eq!(doc["value"][0]["Id"], "4");
    assert!(doc.get("@odata.nextLink").is_none());

    let doc = app.ok("BusinessPartners?$top=2").await;
    assert!(doc.get("@odata.nextLink").is_none());
    assert_eq!(doc["value"].as_array().unwrap().len(), 2);
    let doc = app.ok("Roles?$top=4").await;
    assert_eq!(doc["value"].as_array().unwrap().len(), 3);
    assert!(doc["@odata.nextLink"].as_str().unwrap().contains("$top=4"));
}

#[tokio::test]
async fn max_page_size_preference() {
    let app = common::sqlite_default().await;
    let r = app
        .request(
            Method::GET,
            "BusinessPartners",
            &[("prefer", "odata.maxpagesize=2")],
            "",
        )
        .await;
    assert_eq!(r.header("preference-applied"), Some("odata.maxpagesize=2"));
    let doc = r.json();
    assert_eq!(doc["value"].as_array().unwrap().len(), 2);
    assert!(doc["@odata.nextLink"].as_str().is_some());
    assert_eq!(app.status("BusinessPartners?$skiptoken=abc").await, 400);
}
