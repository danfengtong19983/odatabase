mod common;

use common::App;
use serde_json::json;

async fn to_many_and_to_one(app: App) {
    let doc = app.ok("BusinessPartners('1')?$expand=Roles").await;
    let roles = doc["Roles"].as_array().unwrap();
    assert_eq!(roles.len(), 3);
    assert_eq!(roles[1]["Label"], "Seller");
    assert_eq!(
        doc["@odata.context"],
        "http://test.local/odata/$metadata#BusinessPartners(Roles())/$entity"
    );
    let doc = app
        .ok("Roles?$expand=Partner&$filter=Code eq 2&$orderby=PartnerId")
        .await;
    assert_eq!(doc["value"][0]["Partner"]["Id"], "1");
    assert_eq!(doc["value"][1]["Partner"]["Id"], "3");
    let doc = app.ok("BusinessPartners?$expand=Owner&$orderby=Id").await;
    assert_eq!(doc["value"][0]["Owner"], json!(null));
    assert_eq!(doc["value"][1]["Owner"]["Id"], "1");
    let doc = app.ok("BusinessPartners?$expand=Roles&$orderby=Id").await;
    let sizes: Vec<usize> = doc["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["Roles"].as_array().unwrap().len())
        .collect();
    assert_eq!(
        sizes,
        [3, 1, 1, 0],
        "a parent without children has an empty array"
    );
}

async fn join_tables(app: App) {
    let doc = app
        .ok("BusinessPartners?$expand=Projects&$orderby=Id")
        .await;
    let titles = |i: usize| -> Vec<String> {
        doc["value"][i]["Projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["Title"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(titles(0), ["Alpha", "Beta"]);
    assert_eq!(titles(1), ["Beta"]);
    assert!(titles(3).is_empty());
    let doc = app
        .ok("Projects(20)?$expand=Partners($select=Name;$orderby=Id desc)")
        .await;
    assert_eq!(doc["Partners"][0]["Name"], "Second Org");
    assert_eq!(doc["Partners"][1]["Name"], "First Org");
    let doc = app
        .ok("Projects?$expand=Partners($count=true;$top=1)&$orderby=Id")
        .await;
    assert_eq!(doc["value"][1]["Partners@odata.count"], 2);
    assert_eq!(doc["value"][1]["Partners"].as_array().unwrap().len(), 1);
    assert_eq!(doc["value"][2]["Partners@odata.count"], 0);
    let doc = app.ok("Projects?$expand=Partners/$count&$orderby=Id").await;
    assert_eq!(doc["value"][1]["Partners@odata.count"], 2);
}

async fn nested_options(app: App) {
    let doc = app
        .ok("BusinessPartners('1')?$select=Name&$expand=Roles($filter=Code gt 1;$orderby=Code desc;$select=Label;$expand=Partner($select=Name))")
        .await;
    let roles = doc["Roles"].as_array().unwrap();
    assert_eq!(roles.len(), 2);
    assert_eq!(roles[0]["Label"], "Agent");
    assert_eq!(roles[0]["Partner"]["Name"], "First Org");
    assert!(roles[0]["Partner"].get("Rating").is_none());
    assert!(doc.get("Rating").is_none());
    assert!(
        doc["@odata.context"]
            .as_str()
            .unwrap()
            .contains("(Name,Roles(Label,Partner(Name)))"),
        "{}",
        doc["@odata.context"]
    );
}

async fn per_parent_top_and_skip(app: App) {
    let doc = app
        .ok("BusinessPartners?$orderby=Id&$expand=Roles($top=2;$skip=1;$orderby=Code desc)")
        .await;
    let codes = |i: usize| -> Vec<i64> {
        doc["value"][i]["Roles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["Code"].as_i64().unwrap())
            .collect()
    };
    assert_eq!(codes(0), [2, 1], "skips the highest code of partner 1");
    assert!(codes(1).is_empty(), "partner 2 has only one role");
    let doc = app
        .ok("BusinessPartners?$orderby=Id&$expand=Roles($top=1;$orderby=Code)")
        .await;
    assert_eq!(doc["value"][0]["Roles"][0]["Code"], 1);
    assert_eq!(doc["value"][1]["Roles"][0]["Code"], 1);
    assert_eq!(doc["value"][2]["Roles"][0]["Code"], 2);
}

async fn counts_and_refs(app: App) {
    let doc = app
        .ok("BusinessPartners?$orderby=Id&$expand=Roles($count=true;$top=1)")
        .await;
    assert_eq!(doc["value"][0]["Roles@odata.count"], 3);
    assert_eq!(doc["value"][0]["Roles"].as_array().unwrap().len(), 1);
    assert_eq!(doc["value"][3]["Roles@odata.count"], 0);
    let doc = app
        .ok("BusinessPartners?$orderby=Id&$expand=Roles($count=true;$filter=Code gt 1)")
        .await;
    assert_eq!(doc["value"][0]["Roles@odata.count"], 2);
    let doc = app
        .ok("BusinessPartners?$orderby=Id&$expand=Roles/$count")
        .await;
    assert_eq!(doc["value"][0]["Roles@odata.count"], 3);
    assert!(doc["value"][0].get("Roles").is_none());
    let doc = app
        .ok("BusinessPartners('2')?$expand=Roles/$ref,Owner/$ref")
        .await;
    assert_eq!(
        doc["Roles"][0]["@odata.id"],
        "http://test.local/odata/Roles(PartnerId='2',Code=1)"
    );
    assert_eq!(
        doc["Owner"]["@odata.id"],
        "http://test.local/odata/BusinessPartners('1')"
    );
}

async fn recursion(app: App) {
    let doc = app.ok("Divisions('EU')?$expand=Children($levels=2)").await;
    let de = doc["Children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["Code"] == "DE")
        .unwrap();
    assert_eq!(de["Children"].as_array().unwrap().len(), 2);
    assert!(
        de["Children"][0].get("Children").is_none(),
        "two levels only"
    );
    let doc = app
        .ok("Divisions('EU')?$expand=Children($levels=max)")
        .await;
    let de = doc["Children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["Code"] == "DE")
        .unwrap();
    assert_eq!(de["Children"][0]["Children"], json!([]));
    let doc = app.ok("Divisions('BE')?$expand=Parent($levels=2)").await;
    assert_eq!(doc["Parent"]["Parent"]["Code"], "EU");
    let doc = app
        .ok("Divisions('BE')?$expand=Parent($expand=Parent($expand=Children($select=Population)))")
        .await;
    assert_eq!(
        doc["Parent"]["Parent"]["Children"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

async fn star_and_derived(app: App) {
    let doc = app.ok("Roles(PartnerId='1',Code=1)?$expand=*").await;
    assert_eq!(doc["Partner"]["Id"], "1");
    let doc = app.ok("BusinessPartners('1')?$expand=*").await;
    assert!(doc["Roles"].is_array());
    assert!(doc["Projects"].is_array());
    assert!(doc["Owned"].is_array());
    assert_eq!(doc["Owned"].as_array().unwrap().len(), 2);
    let doc = app.ok("Roles(PartnerId='3',Code=2)?$expand=Partner").await;
    assert_eq!(doc["Partner"]["@odata.type"], "#Test.Person");
    assert_eq!(doc["Partner"]["FirstName"], "Ann");
    let doc = app
        .ok("BusinessPartners('1')?$expand=Owned($expand=Roles($select=Code))")
        .await;
    assert_eq!(doc["Owned"][0]["Roles"][0]["Code"], 1);
}

async fn collections_inside_expand(app: App) {
    let doc = app
        .ok("Roles(PartnerId='1',Code=1)?$expand=Partner($select=Comments,Phones)")
        .await;
    assert_eq!(doc["Partner"]["Comments"], json!(["first", "second"]));
    assert_eq!(doc["Partner"]["Phones"][0]["Kind"], "work");
}

async fn invalid_expands(app: App) {
    assert_eq!(app.status("BusinessPartners?$expand=Nope").await, 400);
    assert_eq!(
        app.status("BusinessPartners?$expand=Roles($filter=Nope eq 1)")
            .await,
        400
    );
    assert_eq!(
        app.status("BusinessPartners?$expand=Owner($top=1)").await,
        400
    );
    assert_eq!(app.status("BusinessPartners?$expand=Name").await, 400);
}

mod run {
    use super::common;
    use odatabase::{Database, Service};

    fn build() -> impl FnOnce(Database) -> Service {
        |db| common::builder().build(db).unwrap()
    }

    crate::scenarios!(build();
        to_many_and_to_one,
        join_tables,
        nested_options,
        per_parent_top_and_skip,
        counts_and_refs,
        recursion,
        star_and_derived,
        collections_inside_expand,
        invalid_expands,
    );
}
